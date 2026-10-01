//! Live check against real Microsoft Graph, driven through [`OnedriveFs::new`].
//!
//! The loopback mock in `onedrive_tests.rs` encodes our understanding of the API, so it
//! agrees with us by construction and stays green when the API moves; this suite checks
//! **whether what we ask for is what Graph answers.** It needs credentials and a network.
//! Request counts and `Range` headers stay with the mock, since no server reports them back.
//!
//!     set -a; . ./.env; set +a
//!     cargo test -p cortex --features onedrive onedrive_endpoint -- --ignored --nocapture

#![cfg(feature = "onedrive")]

use std::path::{Path, PathBuf};

use cortex::fs::{DirentKind, FileSystem, OnedriveConfig, OnedriveFs};

/// Bounds on the walk, so the same test runs against a small account and a large one
/// without becoming the slowest thing in the suite.
const WALK_ENTRIES: usize = 10;

/// The credentials, or `None` when this host has none.
fn config() -> Option<OnedriveConfig> {
    Some(OnedriveConfig {
        client_id: std::env::var("ONEDRIVE_CLIENT_ID").ok()?,
        client_secret: std::env::var("ONEDRIVE_CLIENT_SECRET").ok(),
        refresh_token: std::env::var("ONEDRIVE_REFRESH_TOKEN").ok()?,
        origins: Default::default(),
    })
}

/// Walk a real account, then read a real file by windows.
///
/// Catches against the real service what the mock only encodes: `$select` naming the
/// download URL the way the *response* spells it is accepted and omits the field from every
/// row, so a read fails outright with "has no download url".
///
/// Skipped, not failed, without credentials.
#[tokio::test]
#[ignore = "requires ONEDRIVE_* env + network"]
async fn onedrive_endpoint_tree_and_reads() {
    let Some(cfg) = config() else {
        eprintln!("  set ONEDRIVE_CLIENT_ID / ONEDRIVE_REFRESH_TOKEN to run; skipping");
        return;
    };
    let fs = OnedriveFs::new(&cfg).unwrap();

    let root = fs.list(Path::new("/")).await.expect("root listing");
    eprintln!("  root: {} entries", root.len());
    assert!(!root.is_empty(), "an account with no files tests nothing");

    for e in root.iter().take(WALK_ENTRIES) {
        let st = e.stat().expect("a listing row carries its stat");
        let kind = if st.kind == DirentKind::Dir {
            "dir"
        } else {
            ""
        };
        eprintln!("  {:<44} {:>12} {kind}", e.name, st.size);
        // Every attribute comes off the listing, so this states the real length and never a
        // placeholder, and costs nothing to ask again.
        assert_eq!(
            fs.stat(&PathBuf::from("/").join(&e.name))
                .await
                .unwrap()
                .size,
            st.size,
            "stat and the listing agree for {}",
            e.name
        );
    }

    let Some(file) = root
        .iter()
        .find(|e| e.kind == DirentKind::File && e.stat().is_some_and(|s| s.size > 4096))
    else {
        eprintln!("  no file over 4 KiB at the root; nothing to range-read");
        return;
    };
    let path = PathBuf::from("/").join(&file.name);
    let size = file.stat().unwrap().size;
    eprintln!("  reading {} ({size} bytes)", file.name);

    let head = read_at(&fs, &path, 0, 4096).await;
    assert_eq!(head.len(), 4096);
    assert_eq!(
        read_at(&fs, &path, 2048, 2048).await,
        head[2048..4096],
        "a window is the file's bytes at that offset, and not the file's front"
    );
    // Past the end is an ordinary end rather than an error.
    assert!(read_at(&fs, &path, size, 4096).await.is_empty());
}

/// `len` bytes at `offset`, however many reads that takes.
///
/// `read_at` may answer short of the buffer, so this asks again from where the last one
/// stopped until it is satisfied or a read returns zero.
async fn read_at(fs: &OnedriveFs, path: &Path, offset: u64, len: usize) -> Vec<u8> {
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
