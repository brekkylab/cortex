//! Helpers shared by the crate's own test modules.
//!
//! Only `scratch` so far, which existed in two copies that differed by one
//! string — and a temp-directory helper is exactly the thing that must not drift,
//! since two copies agreeing on a name would have tests deleting each other's
//! fixtures.

use std::fs;
use std::path::PathBuf;

/// A fresh, empty directory under the system temp dir, wiped first if a previous
/// run left one behind.
///
/// The name carries the process id as well as `owner`/`tag`, so tests in one
/// binary cannot collide with each other and two concurrent `cargo test` runs
/// cannot collide either. No extra crate: `tempfile` would be a dependency the
/// library itself does not need.
pub(crate) fn scratch(owner: &str, tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("cortex-{owner}-test-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}
