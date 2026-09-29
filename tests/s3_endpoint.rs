//! A real object-store endpoint, driven through [`S3Fs::new`].
//!
//! Exercises the client `S3Fs::new` builds from an [`S3Config`] (credentials, endpoint,
//! path addressing, SigV4) and the wire format under it: `ListObjectsV2` XML, a ranged
//! `GET`, a `HEAD`, and error status codes, none of which a local stand-in produces.
//!
//! Skipped, not failed, when the host cannot be reached, so a suite without network stays
//! green.
//!
//! ```sh
//! cargo test --features s3 --test s3_endpoint -- --ignored --nocapture
//! ```
//!
//! Nothing to configure: the mock mints an S3 keypair per caller and serves the roster at
//! `/_mock/users`, so there is no secret to keep. The bucket's layout is discovered, so this
//! survives the corpus changing.

#![cfg(feature = "s3")]

use std::{path::Path, process::Command};

use cortex::fs::{DirentKind, FileSystem, S3Config, S3Fs};

/// A mock of a dozen enterprise services' read APIs, S3 among them, over a fixed corpus.
const HOST: &str = "https://enterprise-mock.brekkylab.com";

/// Path-style S3 lives under `/s3` on that host, not at the root.
const S3_PATH: &str = "/s3";

/// A bucket in the mock's corpus, chosen for having several levels of prefix.
///
/// Hardcoded because a volume is mounted *at* a bucket, so [`S3Fs`] has no `ListBuckets`.
/// If a corpus rebuild drops it, the first assertion says so; a signed `GET /s3/` lists
/// the current ones.
const BUCKET: &str = "redwood-redwood";

/// An S3-compatible endpoint without regions still wants one named.
const REGION: &str = "auto";

/// One caller the mock knows about: an email and the S3 keypair minted for them.
struct Principal {
    email: String,
    access_key_id: String,
    secret_access_key: String,
}

/// The mock's roster of callers, in the order it lists them.
///
/// `curl` rather than an HTTP client: a dev-dependency would be built by **every**
/// `cargo test`, and a test that needs the network can afford to need `curl`. `None` when
/// the host is unreachable, the signal to skip.
fn fetch_roster() -> Option<Vec<Principal>> {
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "20", &format!("{HOST}/_mock/users")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8(out.stdout).ok()?;
    // One object per user, so splitting on each one's first field walks them without a parser.
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
/// A scan rather than a JSON crate, which every test build would pay for; the fields
/// wanted are flat and unambiguous.
fn json_string(body: &str, field: &str) -> Option<String> {
    let after_field = if field.is_empty() {
        body
    } else {
        body.split_once(&format!("\"{field}\""))?.1
    };
    let after_quote = after_field.split_once('"')?.1;
    Some(after_quote.split_once('"')?.0.to_string())
}

/// The settings for one caller, with the secret separate so a test can corrupt it.
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
/// **Not the admin**, whose key bypasses the mock's ACL, which a real deployment never
/// does. The roster is walked until a caller can open the bucket; those denied on the way
/// show the ACL is applied.
///
/// The principal is returned so a test can build a *second* client for the same caller.
async fn volume() -> Option<(S3Fs, Principal)> {
    let Some(roster) = fetch_roster() else {
        eprintln!("skipped: cannot reach {HOST} (see this file's docs)");
        return None;
    };
    assert!(!roster.is_empty(), "the mock's roster came back empty");
    let total = roster.len();

    for (denied, principal) in roster.into_iter().enumerate() {
        // A code path no other test reaches.
        let vol = S3Fs::new(&config(&principal, &principal.secret_access_key))
            .expect("build the S3 client");
        // A caller without access is told the bucket does not exist, as real S3 tends to.
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

/// Largest object read whole: the assertions allocate it twice, and the corpus is not ours
/// to keep small.
const MAX_READ: u64 = 8 << 20;

/// Named to avoid clippy's `type_complexity` on the recursive `find_a_file`.
type FindFileFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<(std::path::PathBuf, u64)>> + 'a>>;

/// Walk down from the root until a file turns up, returning its path and size.
///
/// Discovered so the test encodes no one corpus. Each step is a real `ListObjectsV2` with
/// a delimiter, and the descent only continues if prefixes come back as directories.
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
                // Skip empty objects (nothing to compare) and ones too big to hold.
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

    // The root is a directory without a request.
    assert_eq!(
        vol.stat(Path::new("")).await.expect("stat root").kind,
        DirentKind::Dir
    );

    // Prefixes must come back as directories, or nothing below the root is reachable.
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

    // `HEAD` must match what the listing promised.
    let stat = vol.stat(&path).await.expect("stat the file");
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, size);
    assert!(stat.mtime.is_some(), "LastModified should reach Stat");
    assert!(stat.etag.is_some(), "ETag should reach Stat");

    // A whole read, then a ranged read from the middle to exercise `Range`/206.
    let mut whole = vec![0u8; size as usize];
    let read = vol
        .read_at(&path, &mut whole, 0)
        .await
        .expect("read from 0");
    assert_eq!(read as u64, size, "a short read here would mean EOF");

    let at = size / 2;
    let mut middle = vec![0u8; (size - at) as usize];
    // May be served from the window the first read filled; the bytes must agree either way.
    let read = vol
        .read_at(&path, &mut middle, at)
        .await
        .expect("ranged read");
    assert_eq!(read, middle.len(), "the ranged read came back short");
    assert_eq!(middle, whole[at as usize..], "ranged bytes disagree");

    // Past the end is EOF from the recorded size, not a range the store would reject.
    let mut past = [0u8; 16];
    assert_eq!(
        vol.read_at(&path, &mut past, size)
            .await
            .expect("read past end"),
        0
    );
}

/// A real 404 must arrive as `NotFound`; elsewhere the error table's inputs are synthesised.
#[tokio::test]
#[ignore = "needs a reachable object-store endpoint; see this file's docs"]
async fn a_missing_key_on_a_real_endpoint_is_not_found() {
    let Some((vol, _)) = volume().await else {
        return;
    };
    let missing = Path::new("cortex-e2e-no-such-key-8f2a1c");
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
/// Only `head` and `get` carry the status, so this `stat`s a key that exists. A valid key
/// id with a wrong signature is the only way to get a 403 from a server that answers ACL
/// denials with 404.
///
/// It must be *this* caller's key id: anyone else's would also lack bucket access, leaving
/// two reasons to refuse.
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
