//! `/abin`: the native executables a session can run.
//!
//! Cortex's own, and only those. A session does not name executables and cannot add any, so
//! the set is the same for every session — which makes `/abin` **one read-only disk shared
//! by all of them**, held in the layer store like any other layer and attached as it stands.
//! There is nothing to assemble per session and nothing to stitch.
//!
//! # Why cortex's own are a pinned tarball
//!
//! One per architecture, by URL and SHA-256, exactly as the base rootfs is — and for the
//! same reason: what is downloaded decides what a command in a guest runs, so it is a digest
//! and not a tag.
//!
//! **One tarball rather than a file per executable.** Adding one then costs no new constant,
//! and a version is atomic: there is no state in which half a release is here.
//!
//! # There is no release yet
//!
//! Cortex's executables need a C cross-compiler that its guest crate's pure-Rust
//! configuration does not provide, and publishing them is its own piece of work. Until then
//! [`DIR_ENV`] is how a build gets an `/abin` at all, and [`Builtin::fetch`] says so rather
//! than pretending otherwise.

use std::path::{Path, PathBuf};

use cortex_uvm_console::layer::{Layer, LayerId, LayerStore};
use microsandbox_image::tree::{FileTree, TreeNode};

/// A directory of executables to use instead of the release, as a host path.
///
/// The development and offline path, and the same shape as `CORTEX_UVM_KERNEL` and
/// `CORTEX_UVM_IMAGE`. No download and no digest check: what it names is whatever the person
/// running this just built.
pub const DIR_ENV: &str = "CORTEX_ABIN_DIR";

/// Cortex's own executables, by URL and by the digest they have to hash to.
pub struct Builtin {
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the tarball.
    pub sha256: &'static str,
    /// Names the cached tarball, so a change to the pin is a different entry.
    pub name: &'static str,
}

impl Builtin {
    pub fn host() -> Builtin {
        match std::env::consts::ARCH {
            "aarch64" => Builtin {
                url: "https://github.com/brekkylab/cortex/releases/download/abin-v0.1.0/abin-0.1.0-aarch64.tar.gz",
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                name: "abin-0.1.0-aarch64",
            },
            "x86_64" => Builtin {
                url: "https://github.com/brekkylab/cortex/releases/download/abin-v0.1.0/abin-0.1.0-x86_64.tar.gz",
                sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                name: "abin-0.1.0-x86_64",
            },
            // A guest runs the host's architecture, so there is nothing to fall back to.
            other => panic!("cortex ships no executables for a {other} host"),
        }
    }

    /// This release as a layer, putting it in `store` if it is not already there.
    ///
    /// `override_dir` — [`DIR_ENV`], read by the caller — short-circuits the whole of it,
    /// which is the only way this works today.
    pub fn layer(&self, override_dir: Option<&Path>, store: &LayerStore) -> anyhow::Result<Layer> {
        if let Some(dir) = override_dir {
            anyhow::ensure!(
                dir.is_dir(),
                "{DIR_ENV} is {}, which is not a directory",
                dir.display()
            );
            return layer_of(dir, store);
        }
        Err(self.unpublished())
    }

    /// What there is to say instead of a release.
    ///
    /// When there is one, fetching it is the caller's to await *before* handing a directory
    /// in — which is what `override_dir` already is, so nothing here becomes async for it.
    fn unpublished(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "cortex ships no executables yet: {} is not published, so there is nothing at {} \
             to verify against {}. Set {DIR_ENV} to a directory of guest-native executables \
             — statically linked for the guest, not for this host.",
            self.name,
            self.url,
            self.sha256
        )
    }
}

/// The disk `/abin` is: cortex's own executables, as one layer.
///
/// `builtin` is [`DIR_ENV`] when it is set, and read by the caller rather than here: what a
/// server is configured with is read where the rest of its configuration is read, and a
/// function that reached for it itself would be one no test could call twice.
///
/// The layer is handed over as it stands, which is why there is no stitch and no assembled
/// disk anywhere — a base layer carries no whiteouts, so it is already a valid image, and
/// the store already keeps one copy of it however many sessions ask.
///
/// **Blocking, and says so by not being `async`.** Reading a directory of executables and
/// writing it back out as an EROFS is file work from end to end; a caller on a runtime owes
/// this a thread of its own.
pub fn disk(builtin: Option<&Path>, store: &LayerStore) -> anyhow::Result<PathBuf> {
    Ok(Builtin::host().layer(builtin, store)?.erofs)
}

/// One directory as a layer, named by what is in it.
///
/// Content-addressed rather than named for the path, so that a directory somebody is
/// rebuilding gets a new layer each time its contents change and the same one when they do
/// not. The tree is read once and hashed from what was read, rather than walking the
/// directory twice.
fn layer_of(dir: &Path, store: &LayerStore) -> anyhow::Result<Layer> {
    let tree = cortex_uvm_console::layer::tree::from_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?;
    let id = digest(&tree)?;
    store.put(&id, &tree)
}

/// A tree's identity: everything in it, in one order.
///
/// Mode and contents as well as names, because two directories differing only in whether
/// something is executable are two different `/abin`s.
fn digest(tree: &FileTree) -> anyhow::Result<LayerId> {
    use sha2::{Digest as _, Sha256};

    fn feed(node: &TreeNode, path: &[u8], hasher: &mut Sha256) -> anyhow::Result<()> {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        match node {
            TreeNode::RegularFile(file) => {
                hasher.update(b"f");
                hasher.update(file.metadata.mode.to_le_bytes());
                let data = file.data.read_all()?;
                hasher.update((data.len() as u64).to_le_bytes());
                hasher.update(&data);
            }
            TreeNode::Symlink(link) => {
                hasher.update(b"l");
                hasher.update((link.target.len() as u64).to_le_bytes());
                hasher.update(&link.target);
            }
            TreeNode::Directory(directory) => {
                hasher.update(b"d");
                hasher.update(directory.metadata.mode.to_le_bytes());
                // `entries` is a `BTreeMap`, so this is name order and therefore the same
                // order every time the same directory is read.
                for (name, child) in &directory.entries {
                    let mut child_path = path.to_vec();
                    if !child_path.is_empty() {
                        child_path.push(b'/');
                    }
                    child_path.extend_from_slice(name.as_encoded_bytes());
                    feed(child, &child_path, hasher)?;
                }
            }
            // Nothing else belongs in an `/abin` and `from_dir` leaves them out, but a
            // digest is only sound if it accounts for everything a tree can hold.
            TreeNode::CharDevice(device) => {
                hasher.update(b"c");
                hasher.update(device.major.to_le_bytes());
                hasher.update(device.minor.to_le_bytes());
            }
            TreeNode::BlockDevice(device) => {
                hasher.update(b"b");
                hasher.update(device.major.to_le_bytes());
                hasher.update(device.minor.to_le_bytes());
            }
            TreeNode::Fifo(metadata) => {
                hasher.update(b"p");
                hasher.update(metadata.mode.to_le_bytes());
            }
            TreeNode::Socket(metadata) => {
                hasher.update(b"s");
                hasher.update(metadata.mode.to_le_bytes());
            }
        }
        Ok(())
    }

    let mut hasher = Sha256::new();
    hasher.update(b"cortex abin tree v1\n");
    feed(&TreeNode::Directory(tree.root.clone()), b"", &mut hasher)?;
    LayerId::parse(&format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_of(names: &[&str]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho {name}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    fn store_in(dir: &Path) -> LayerStore {
        LayerStore::open(&dir.join("layers")).unwrap()
    }

    #[test]
    fn a_pinned_tarball_is_named_for_this_architecture() {
        let builtin = Builtin::host();
        assert!(builtin.url.starts_with("https://"), "{}", builtin.url);
        assert!(
            builtin.url.contains(std::env::consts::ARCH),
            "{} is not this host's",
            builtin.url
        );
        assert_eq!(builtin.sha256.len(), 64);
        assert!(builtin.name.contains(std::env::consts::ARCH));
    }

    #[test]
    fn without_a_release_or_an_override_it_says_what_to_do() {
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());
        // `FileTree` has no `Debug`, so the `Ok` side cannot be unwrapped through.
        let err = match Builtin::host().layer(None, &store) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a layer came from nowhere"),
        };
        assert!(err.contains(DIR_ENV), "{err}");
    }

    #[test]
    fn a_local_directory_stands_in_for_the_release() {
        let builtin = dir_of(&["mem", "index"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let layer = Builtin::host().layer(Some(builtin.path()), &store).unwrap();

        let tree = layer
            .tree(cortex_uvm_console::layer::tree::Contents::Skip)
            .unwrap();
        assert!(tree.get(b"mem").is_some(), "the override was not used");
        assert!(tree.get(b"index").is_some());
    }

    /// The store is content-addressed, so the same directory has to name the same layer and
    /// a changed one a different layer — otherwise a rebuilt executable would never arrive.
    #[test]
    fn a_directory_names_a_layer_by_what_is_in_it() {
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let first = dir_of(&["mem"]);
        let same = layer_of(first.path(), &store).unwrap().id;
        assert_eq!(layer_of(first.path(), &store).unwrap().id, same);

        std::fs::write(first.path().join("mem"), b"#!/bin/sh\necho changed\n").unwrap();
        assert_ne!(
            layer_of(first.path(), &store).unwrap().id,
            same,
            "a changed executable named the same layer"
        );
    }

    /// The disk is the layer as it stands — an EROFS out of the store, not a descriptor.
    /// A session runs cortex's own executables and no others, so there is nothing to stitch.
    #[test]
    fn the_disk_is_the_builtin_layer_itself() {
        let builtin = dir_of(&["mem"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let disk = disk(Some(builtin.path()), &store).unwrap();
        assert_eq!(disk.extension().unwrap(), "erofs");
        assert_eq!(
            disk,
            layer_of(builtin.path(), &store).unwrap().erofs,
            "the disk is not the layer the store holds"
        );
    }

    /// And so every session on this host gets the same file, rather than one assembled apiece.
    #[test]
    fn every_session_gets_the_same_disk() {
        let builtin = dir_of(&["mem", "index"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        assert_eq!(
            disk(Some(builtin.path()), &store).unwrap(),
            disk(Some(builtin.path()), &store).unwrap()
        );
    }
}
