//! A real object-store endpoint, driven through [`S3Fs::new`].
//!
//! Every other S3 test builds the volume from a store handed to it, which skips the
//! one thing this file exercises: the client `S3Fs::new` builds from an
//! [`S3Config`] — credentials, endpoint, path addressing, SigV4 — and the wire
//! format underneath it. `ListObjectsV2` XML, a ranged `GET`, a `HEAD`, and the
//! status codes an error arrives as are all upstream's to produce, and nothing that
//! stands in for a store locally produces them.
//!
//! Skipped, not failed, when the host cannot be reached: this needs a server, and a
//! suite that has no network should stay green.
//!
//! ```sh
//! cargo test --features s3 --test s3_endpoint -- --ignored --nocapture
//! ```
//!
//! Nothing to configure. The host is named below and the credentials come from it —
//! the mock mints an S3 keypair per caller and serves the roster at `/_mock/users`,
//! so there is no secret to keep anywhere. The bucket's layout is discovered rather
//! than assumed, so this survives the corpus changing under it.

#![cfg(feature = "s3")]

use std::{path::Path, process::Command};

use virtx::fs::{DirentKind, FileSystem, S3Config, S3Fs};

/// The mock this drives — a stand-in for the read APIs of a dozen enterprise
/// services, S3 among them, over a corpus rather than a live account.
const HOST: &str = "https://enterprise-mock.brekkylab.com";

/// Path-style S3 lives under `/s3` on that host, not at the root.
const S3_PATH: &str = "/s3";

/// A bucket in the mock's corpus, chosen for having several levels of prefix.
///
/// Named here because bucket names are not discoverable through [`S3Fs`]:
/// `ListBuckets` is not a `FileSystem` operation, and would not be — a volume is
/// mounted *at* a bucket. If the corpus is rebuilt and this name goes away, the
/// first assertion says so; the current list comes from a signed `GET /s3/`.
const BUCKET: &str = "redwood-redwood";

/// An S3-compatible endpoint with no regions of its own still wants one named.
const REGION: &str = "auto";

/// One caller the mock knows about: an email and the S3 keypair minted for them.
struct Principal {
    email: String,
    access_key_id: String,
    secret_access_key: String,
}

/// The mock's roster of callers, in the order it lists them.
///
/// This is the "rest of the settings" — the mock derives an access key and secret per
/// caller, so the test carries no credentials of its own.
///
/// Fetched with `curl` rather than an HTTP client: adding one as a dev-dependency
/// would make **every** `cargo test` build it, including the bare one that currently
/// pulls no crates at all. A test that already needs the network can afford to need
/// `curl` too. Returns `None` when the host is unreachable, which is the signal to
/// skip rather than fail.
fn fetch_roster() -> Option<Vec<Principal>> {
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "20", &format!("{HOST}/_mock/users")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8(out.stdout).ok()?;
    // One object per user, so splitting on the first field of each is enough to walk
    // them without a parser. See `json_string` for why there isn't one.
    Some(
        body.split("\"email\"")
            .skip(1)
            .filter_map(|chunk| {
                Some(Principal {
                    email: json_string(chunk, "")?,
                    access_key_id: json_string(chunk, "s3_access_key_id")?,
                    secret_access_key: json_string(chunk, "s3_secret_access_key")?,
                })
            })
            .collect(),
    )
}

/// Pull one string field out of a JSON fragment, or the first string when `field` is
/// empty (the value the fragment was split on).
///
/// A scan rather than a JSON crate, for the reason [`fetch_roster`] gives: the fields
/// wanted here are flat and unambiguous, and a parser would cost every test build.
fn json_string(body: &str, field: &str) -> Option<String> {
    let after_field = if field.is_empty() {
        body
    } else {
        body.split_once(&format!("\"{field}\""))?.1
    };
    let after_quote = after_field.split_once('"')?.1;
    Some(after_quote.split_once('"')?.0.to_string())
}

/// The settings for one caller, with the secret passed separately so a test can
/// corrupt it. Everything else is fixed by the host.
fn config(principal: &Principal, secret: &str) -> S3Config {
    S3Config {
        endpoint: Some(format!("{HOST}{S3_PATH}")),
        bucket: BUCKET.into(),
        region: REGION.into(),
        access_key_id: principal.access_key_id.clone(),
        secret_access_key: secret.into(),
        key_prefix: None,
    }
}

/// A volume on [`BUCKET`], and the caller who turned out to be allowed to read it.
///
/// **Not the admin.** The admin key bypasses the mock's ACL and sees every bucket,
/// which is the one principal a real deployment never uses — and it would skip the
/// filtering that decides what a listing even contains. So the roster is walked until
/// a caller can open the bucket: most cannot, and the ones who cannot are evidence
/// that the ACL is being applied rather than ignored.
///
/// Which caller that is comes from the corpus, not from here, so this keeps working
/// when the corpus is rebuilt with different people in it. The principal comes back
/// with the volume so a test that needs a *second* client for the same caller does not
/// have to fetch the roster again and guess which one it was.
async fn volume() -> Option<(S3Fs, Principal)> {
    let Some(roster) = fetch_roster() else {
        eprintln!("skipped: cannot reach {HOST} (see this file's docs)");
        return None;
    };
    assert!(!roster.is_empty(), "the mock's roster came back empty");
    let total = roster.len();

    for (denied, principal) in roster.into_iter().enumerate() {
        // Building the client is itself a code path no other test reaches, and it
        // fails here rather than at the first request.
        let vol = S3Fs::new(&config(&principal, &principal.secret_access_key))
            .expect("build the S3 client");
        // The one request `check_reachable` exists to make. A caller without access is
        // told the bucket does not exist — the mock hides existence rather than
        // admitting it, which is what real S3 tends to do too.
        if vol.check_reachable().await.is_ok() {
            eprintln!(
                "using {} ({denied} earlier callers were denied {BUCKET})",
                principal.email
            );
            return Some((vol, principal));
        }
    }
    panic!("no caller in the roster of {total} can read {BUCKET}");
}

/// Nothing above this is read whole — the assertions allocate the object twice over,
/// and the corpus is not ours to keep small.
const MAX_READ: u64 = 8 << 20;

/// Walk down from the root until a file turns up, returning its path and size.
///
/// Discovered rather than hardcoded so this test does not encode one corpus. Also
/// evidence in itself: every step is a real `ListObjectsV2` with a delimiter, and a
/// prefix has to come back as a directory for the descent to continue at all.
/// A boxed, borrowing future — named so the recursive `find_a_file` can spell its
/// own return type without a clippy `type_complexity` warning.
type FindFileFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<(std::path::PathBuf, u64)>> + 'a>>;

fn find_a_file<'a>(vol: &'a S3Fs, at: &'a Path, depth: usize) -> FindFileFut<'a> {
    Box::pin(async move {
        if depth == 0 {
            return None;
        }
        let entries = vol.list(at).await.expect("list");
        for entry in &entries {
            if entry.kind == DirentKind::File {
                let path = at.join(&entry.name);
                let size = match entry.stat() {
                    Some(s) => s.size,
                    None => vol.stat(&path).await.expect("stat").size,
                };
                // Keep looking rather than read something the assertions cannot hold, or
                // something empty that has no bytes to compare.
                if size == 0 || size > MAX_READ {
                    continue;
                }
                return Some((path, size));
            }
        }
        for entry in &entries {
            if entry.kind == DirentKind::Dir {
                let sub = at.join(&entry.name);
                if let Some(found) = find_a_file(vol, &sub, depth - 1).await {
                    return Some(found);
                }
            }
        }
        None
    })
}

#[tokio::test]
#[ignore = "needs a reachable object-store endpoint; see this file's docs"]
async fn a_real_endpoint_answers_the_whole_read_surface() {
    let Some((vol, _)) = volume().await else {
        return;
    };

    // The root is a directory without asking anyone.
    assert_eq!(
        vol.stat(Path::new("")).await.expect("stat root").kind,
        DirentKind::Dir
    );

    // Real `ListObjectsV2` with a delimiter: prefixes have to come back as
    // directories, or nothing below the root is reachable.
    let root = vol.list(Path::new("")).await.expect("list root");
    assert!(!root.is_empty(), "the bucket looks empty; pick another");
    println!(
        "root: {} entries ({} dirs)",
        root.len(),
        root.iter().filter(|e| e.kind == DirentKind::Dir).count()
    );

    let (path, size) = find_a_file(&vol, Path::new(""), 6)
        .await
        .expect("a file somewhere in the bucket");
    println!("found {} ({size} bytes)", path.display());
    assert!(size > 0, "expected a non-empty object to read");

    // Real `HEAD`: the metadata a listing promised has to match what the object says.
    let stat = vol.stat(&path).await.expect("stat the file");
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, size);
    assert!(stat.mtime.is_some(), "LastModified should reach Stat");
    assert!(stat.etag.is_some(), "ETag should reach Stat");

    // A whole read, then the same bytes through one ranged GET from the middle — the second is
    // what exercises `Range`/206 rather than a plain body.
    let mut whole = vec![0u8; size as usize];
    let read = vol
        .read_at(&path, &mut whole, 0)
        .await
        .expect("read from 0");
    assert_eq!(read as u64, size, "a short read here would mean EOF");

    let at = size / 2;
    let mut middle = vec![0u8; (size - at) as usize];
    // The store's window is keyed by path and the first read filled it, so this one is served
    // from cache or from a fresh range — either way the bytes have to agree.
    let read = vol
        .read_at(&path, &mut middle, at)
        .await
        .expect("ranged read");
    assert_eq!(read, middle.len(), "the ranged read came back short");
    assert_eq!(middle, whole[at as usize..], "ranged bytes disagree");

    // Past the end is EOF, answered from the size the store recorded rather than by sending a
    // range it would reject.
    let mut past = [0u8; 16];
    assert_eq!(
        vol.read_at(&path, &mut past, size)
            .await
            .expect("read past end"),
        0
    );
}

/// A real 404 has to arrive as `NotFound` — the status is upstream's to produce, and
/// the error table's inputs are only ever synthesised elsewhere.
#[tokio::test]
#[ignore = "needs a reachable object-store endpoint; see this file's docs"]
async fn a_missing_key_on_a_real_endpoint_is_not_found() {
    let Some((vol, _)) = volume().await else {
        return;
    };
    let missing = Path::new("virtx-e2e-no-such-key-8f2a1c");
    let err = vol
        .stat(missing)
        .await
        .expect_err("a missing key must not stat");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "expected NotFound, got {err:?}"
    );
    let mut buf = [0u8; 1];
    assert_eq!(
        vol.read_at(missing, &mut buf, 0)
            .await
            .expect_err("a read of a missing key must not succeed")
            .kind(),
        std::io::ErrorKind::NotFound,
        "a read should agree with stat"
    );
}

/// A 403 has to arrive as `PermissionDenied`, not as the generic variant.
///
/// Only `head` and `get` carry the status — a `list` failure is generic whatever the
/// code — so this asks with a `stat` on a key that exists. A valid key id with a wrong
/// signature is the one way to get a 403 out of a server that answers an ACL denial
/// with 404.
///
/// It has to be *this* caller's key id, corrupted. Anyone else's would leave two
/// reasons to refuse — a bad signature and no access to the bucket — and no way to tell
/// from the answer which one was tested.
#[tokio::test]
#[ignore = "needs a reachable object-store endpoint; see this file's docs"]
async fn a_bad_signature_on_a_real_endpoint_is_permission_denied() {
    let Some((good, principal)) = volume().await else {
        return;
    };
    let (path, _size) = find_a_file(&good, Path::new(""), 6)
        .await
        .expect("a file to ask about");

    let wrong = format!("{}x", principal.secret_access_key);
    let vol = S3Fs::new(&config(&principal, &wrong)).expect("build the S3 client");

    let err = vol
        .stat(&path)
        .await
        .expect_err("a bad signature must not stat");
    assert!(
        err.kind() == std::io::ErrorKind::PermissionDenied,
        "expected PermissionDenied from a 403 on HEAD, got {err:?}"
    );
}
