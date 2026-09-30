//! A Drive API that this repository does not write, driven through [`GdriveFs::new`].
//!
//! Every other gdrive test answers from a loopback mock in `gdrive_tests.rs`, which is the
//! right tool for one question — *what did we ask for* — and the wrong one for another:
//! **whether what we ask for is what Drive answers.** A mock encodes our understanding of
//! the API, so it agrees with us by construction and stays green when the API moves. This
//! file exists for the other half.
//!
//! It drives **backlot** (the server in `brekkylab/enterprise-mock`), which follows the real
//! Drive, Docs, Sheets and Slides shapes over a corpus rather than a live account: paging
//! cursors, `orderBy`, the ACL-filtered view a token sees, the error *statuses* Google
//! actually returns. When backlot changes to match Google, this is what notices.
//!
//! Skipped, not failed, when the host cannot be reached. A suite with no network stays
//! green.
//!
//! ```sh
//! cargo test --features gdrive --test gdrive_endpoint -- --ignored --nocapture
//! ```
//!
//! Nothing to configure. The host is named below and the credentials come from it: backlot
//! publishes a per-user token roster at `/_mock/users`, and its token endpoint's refresh
//! grant takes one of those tokens and hands it back — so a `GdriveConfig` needs no secret
//! kept anywhere. The corpus is discovered rather than assumed, so this survives it
//! changing underneath.
//!
//! ## What this cannot cover
//!
//! **Anything about a blob.** The corpus holds Docs-editors files in bulk and almost no
//! binary ones, and those it has are reachable only through `Shared with me`, whose listing
//! is truncated at `MAX_FOLDER_FILES` before reaching them. So the blob test below skips
//! against this corpus, and runs once it gains one.
//!
//! `FIRST_SPAN` and `READ_SPAN` are larger than any file this corpus holds, so the span
//! policy cannot be exercised here.
//!
//! **What we asked for.** No server reports the number of requests it received back to its
//! caller, or the `Range` header on each. Every claim of the form "eight windows, one
//! request" is a claim about our own behaviour, and it stays with the loopback mock in
//! `gdrive_tests.rs`, which counts.

#![cfg(feature = "gdrive")]

use std::path::{Path, PathBuf};

use cortex::fs::{DirentKind, FileSystem, GdriveConfig, GdriveFs, GdriveOrigins};

/// The mock this drives — a stand-in for the read APIs of several enterprise services,
/// Drive among them. `BACKLOT_URL` overrides it for a local instance.
const HOST: &str = "https://enterprise-mock.brekkylab.com";

/// Bounds on the walk, so the same test runs against a small sample and a large
/// corpus without becoming the slowest thing in the suite.
const WALK_DIRS: usize = 12;
const WALK_FILES: usize = 40;

fn host() -> String {
    std::env::var("BACKLOT_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HOST.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// A config pointed at backlot, with a token taken from its own roster.
///
/// `None` when the host cannot be reached or does not publish the roster — the caller
/// prints why and returns, rather than failing a suite that has no network.
///
/// The token is used as the `refresh_token`, which is exactly what backlot's token endpoint
/// expects: a user's refresh token *is* their bearer token there, and the refresh grant
/// validates it and hands it back. `client_id` and `client_secret` are not checked, so they
/// are named rather than fetched.
async fn config() -> Option<GdriveConfig> {
    let host = host();
    let roster: serde_json::Value = reqwest::Client::new()
        .get(format!("{host}/_mock/users"))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;

    // The admin token bypasses ACL filtering, which is what a crawl of the whole corpus
    // wants. A per-user token is the other interesting case and is left to a test that
    // asks about ACLs.
    let token = roster
        .get("admin_token")
        .and_then(|t| t.as_str())
        .map(str::to_string)?;

    Some(GdriveConfig {
        client_id: "gdrive-endpoint-test".into(),
        client_secret: "unchecked".into(),
        refresh_token: token,
        origins: GdriveOrigins::behind(&host),
    })
}

macro_rules! skip_unless_reachable {
    () => {
        match config().await {
            Some(cfg) => cfg,
            None => {
                eprintln!(
                    "  {} is not reachable (or does not publish /_mock/users); skipping",
                    host()
                );
                return;
            }
        }
    };
}

/// Walk the tree the way a reader walks it, and read what it lists.
///
/// The three root sections are ours rather than Drive's, and each takes a different listing
/// — `My Drive` a folder id, `Shared with me` a query with no id at all, a shared drive its
/// own `driveId`. This is where that distinction meets a server that enforces it: a request
/// scoped wrong comes back empty or `notFound` rather than merely different.
#[tokio::test]
#[ignore = "requires backlot (BACKLOT_URL, or the host named in this file)"]
async fn gdrive_endpoint_tree_and_reads() {
    let cfg = skip_unless_reachable!();
    let fs = GdriveFs::new(&cfg).unwrap();

    let root = fs.list(Path::new("/")).await.expect("root sections");
    eprintln!("  root: {} sections", root.len());
    assert!(
        root.iter().any(|d| d.name == "My Drive"),
        "the account's own drive is always a section: {:?}",
        root.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
    for d in &root {
        assert_eq!(d.kind, DirentKind::Dir, "{} is a section", d.name);
    }

    // Breadth-first, bounded. Every entry's stat has to agree with the listing it came
    // from — a listing that reported one number and a stat that reports another is the
    // failure this cannot see from a mock that answers both from the same fixture.
    let mut queue: Vec<PathBuf> = root
        .iter()
        .map(|d| PathBuf::from("/").join(&d.name))
        .collect();
    let (mut dirs, mut files, mut checked) = (0usize, 0usize, 0usize);

    while let Some(dir) = queue.pop() {
        if dirs >= WALK_DIRS {
            break;
        }
        dirs += 1;
        let Ok(entries) = fs.list(&dir).await else {
            // A section an account cannot see is an ordinary outcome, not a failure.
            continue;
        };
        for e in entries {
            let path = dir.join(&e.name);
            match e.kind {
                DirentKind::Dir => queue.push(path),
                DirentKind::File => {
                    if files >= WALK_FILES {
                        continue;
                    }
                    files += 1;
                    let listed = e.stat().expect("a gdrive listing row carries its stat");
                    let stated = fs.stat(&path).await.expect("stat of a listed file");
                    assert_eq!(
                        stated.kind,
                        DirentKind::File,
                        "{} listed as a file",
                        path.display()
                    );
                    // A blob's two numbers must match exactly. A document's may not: the
                    // listing can only offer the placeholder, while `stat` answers the
                    // JSON's real length once something has produced it.
                    if !e.name.ends_with(".json") {
                        assert_eq!(
                            stated.size,
                            listed.size,
                            "{} states one length",
                            path.display()
                        );
                    }
                    checked += 1;
                }
            }
        }
    }
    eprintln!("  walked {dirs} directories, checked {checked} files");
    assert!(checked > 0, "a corpus with no readable file tests nothing");
}

/// A blob's bytes come back at the offset they were asked for, and the file ends where it
/// says it does.
///
/// Not the span *sizes* — the corpus has no blob near them, see the note at the top. What
/// this asks is whether a server we did not write honours a ranged `GET` the way this store
/// assumes it does: the window starts where it should, and past the last byte is an
/// ordinary end rather than an error or a wrap.
///
/// Small is the interesting case here anyway. A blob under one span is fetched whole and
/// then cut locally, so the arithmetic that turns a file offset into an offset into the
/// held span is what answers — and getting that wrong reads as the right length of the
/// wrong bytes.
#[tokio::test]
#[ignore = "requires backlot (BACKLOT_URL, or the host named in this file)"]
async fn gdrive_endpoint_reads_a_blob_at_the_offset_asked_for() {
    let cfg = skip_unless_reachable!();
    let fs = GdriveFs::new(&cfg).unwrap();

    let Some((path, size)) = find_file(&fs, 64, |name| !name.ends_with(".json")).await else {
        // Expected against the corpus as it stands — see the note at the top of this file.
        // Not an assertion, so the test runs the day a blob appears.
        eprintln!(
            "  no blob reachable in {WALK_DIRS} directories; \
             the corpus keeps its two behind a truncated listing"
        );
        return;
    };
    eprintln!("  {} is {} bytes", path.display(), size);

    let whole = read_at(&fs, &path, 0, size as usize).await;
    assert_eq!(whole.len() as u64, size, "the file states its own length");

    // Every window of it, against the whole. A stride that does not divide the file is
    // deliberate: the last one is short, and short is the end.
    let stride = 37usize;
    let mut at = 0u64;
    while at < size {
        let got = read_at(&fs, &path, at, stride).await;
        let want = &whole[at as usize..(at as usize + stride).min(whole.len())];
        assert_eq!(got, want, "the window at {at} is the file's bytes there");
        at += stride as u64;
    }

    // Past the end is an end. A walk that runs off the end asks for exactly this.
    assert!(
        read_at(&fs, &path, size, 4096).await.is_empty(),
        "nothing lives past the last byte"
    );
    assert!(
        read_at(&fs, &path, size + 1_000_000, 4096).await.is_empty(),
        "nor well past it"
    );
}

/// A Docs-editors file arrives as its own API's JSON, and parses.
///
/// The mock hands back a fixture shaped the way we believe the Docs API shapes one. This
/// asks a server that implements the API instead — and the answer has to be JSON a parser
/// accepts after the tail this store pads it with.
#[tokio::test]
#[ignore = "requires backlot (BACKLOT_URL, or the host named in this file)"]
async fn gdrive_endpoint_serves_a_document_as_json() {
    let cfg = skip_unless_reachable!();
    let fs = GdriveFs::new(&cfg).unwrap();

    let mut seen = 0usize;
    for suffix in [".gdoc.json", ".gsheet.json", ".gslide.json"] {
        let Some((path, _)) = find_file(&fs, 0, |name| name.ends_with(suffix)).await else {
            continue;
        };
        // The way a reader reads it: to the end, then trim the whitespace this store pads
        // the declared length out with. JSON is defined to ignore what follows a value,
        // which is the whole reason that padding is whitespace.
        let whole = read_to_end(&fs, &path).await;
        let v: serde_json::Value = serde_json::from_slice(&whole)
            .unwrap_or_else(|e| panic!("{} did not parse: {e}", path.display()));
        let keys: Vec<&str> = v
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        eprintln!(
            "  {:<44} {:>9} bytes, top keys {:?}",
            path.display(),
            whole.len(),
            &keys[..keys.len().min(4)]
        );
        assert!(!keys.is_empty(), "a document's JSON is an object");
        seen += 1;
    }
    assert!(seen > 0, "no Docs-editors file in the corpus");
}

// ---------------------------------------------------------------------------
// Helpers — the public surface only, which is the point of an integration test
// ---------------------------------------------------------------------------

/// `len` bytes at `offset`, however many reads that takes.
///
/// `read_at` may answer short of the buffer, and a short answer is end of file — so this
/// asks again from where the last one stopped until it is satisfied or the file ends.
async fn read_at(fs: &GdriveFs, path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        let n = fs
            .read_at(path, &mut out[got..], offset + got as u64)
            .await
            .unwrap_or_else(|e| panic!("read {} at {}: {e}", path.display(), offset + got as u64));
        if n == 0 {
            break;
        }
        got += n;
    }
    out.truncate(got);
    out
}

/// The whole file, then trimmed of the whitespace tail a declared length is padded out to.
async fn read_to_end(fs: &GdriveFs, path: &Path) -> Vec<u8> {
    const CHUNK: usize = 256 * 1024;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let at = out.len() as u64;
        let mut buf = vec![0u8; CHUNK];
        let n = fs
            .read_at(path, &mut buf, at)
            .await
            .unwrap_or_else(|e| panic!("read {} at {at}: {e}", path.display()));
        if n == 0 {
            break;
        }
        buf.truncate(n);
        out.extend_from_slice(&buf);
    }
    while out.last().is_some_and(|b| b.is_ascii_whitespace()) {
        out.pop();
    }
    out
}

/// The first file in the corpus over `min` bytes whose name `want` accepts, breadth-first
/// and bounded. Discovered rather than named, so the corpus can change underneath.
async fn find_file(fs: &GdriveFs, min: u64, want: impl Fn(&str) -> bool) -> Option<(PathBuf, u64)> {
    // The account's own drive first. A listing can run to many pages, so the order the
    // sections are tried in is most of the wall clock.
    let mut sections: Vec<PathBuf> = fs
        .list(Path::new("/"))
        .await
        .ok()?
        .iter()
        .map(|d| PathBuf::from("/").join(&d.name))
        .collect();
    sections.sort_by_key(|p| p.file_name().map(|n| n != "My Drive").unwrap_or(true));

    let mut queue = sections;
    let mut dirs = 0usize;
    while let Some(dir) = queue.pop() {
        if dirs >= WALK_DIRS {
            return None;
        }
        dirs += 1;
        let Ok(entries) = fs.list(&dir).await else {
            continue;
        };
        for e in entries {
            let path = dir.join(&e.name);
            match e.kind {
                DirentKind::Dir => queue.push(path),
                // The listing already carries the size, so this costs no request. A
                // document's would be the placeholder, which is why the caller's `want`
                // is what excludes them rather than a size test.
                DirentKind::File if want(&e.name) => {
                    if e.stat().is_some_and(|s| s.size >= min) {
                        return Some((path, e.stat().unwrap().size));
                    }
                }
                DirentKind::File => {}
            }
        }
    }
    None
}
