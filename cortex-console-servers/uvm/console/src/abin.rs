//! `/abin`: the native executables a session can run.
//!
//! Two sources, layered bottom-up. Cortex's own come first and are the same for every
//! session; whatever the caller named in `init` goes over them, in the order it named them.
//! What comes out is one read-only disk the boot attaches and the guest mounts.
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

use cortex::console::AbinSource;
use cortex_uvm_console::layer::{Layer, LayerId, LayerStore, stitch, tree};
use microsandbox_image::tree::{FileTree, TreeNode};

use crate::contract::BaseFormat;

/// A directory of executables to use instead of the release, as a host path.
///
/// The development and offline path, and the same shape as `CORTEX_UVM_KERNEL` and
/// `CORTEX_UVM_IMAGE`. No download and no digest check: what it names is whatever the person
/// running this just built.
pub const DIR_ENV: &str = "CORTEX_ABIN_DIR";

/// The disk `/abin` is, and how a boot has to attach it.
#[derive(Debug)]
pub struct Assembled {
    pub disk: PathBuf,
    pub format: BaseFormat,
}

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

    /// This release as a layer and the tree it was made from, putting it in `store` if it is
    /// not already there.
    ///
    /// `override_dir` — [`DIR_ENV`], read by the caller — short-circuits the whole of it,
    /// which is the only way this works today.
    pub fn layer(
        &self,
        override_dir: Option<&Path>,
        store: &LayerStore,
    ) -> anyhow::Result<(Layer, FileTree)> {
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

/// A name two of this session's executable directories both answer to.
///
/// Its own type rather than a message, because the two ways an assembly fails deserve
/// opposite answers. That cortex has published nothing yet is this host's gap and costs the
/// session an `/abin`; a collision is something the caller said, is fixable by saying
/// something else, and travels back as
/// [`DUPLICATE_EXECUTABLE`](cortex::console::Error::DUPLICATE_EXECUTABLE).
#[derive(Debug)]
pub struct Duplicate(pub String);

impl std::fmt::Display for Duplicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Duplicate {}

/// Assemble `/abin` out of `builtin` — cortex's own executables — and whatever `sources` name.
///
/// `builtin` is [`DIR_ENV`] when it is set, and read by the caller rather than here: what a
/// server is configured with is read where the rest of its configuration is read, and a
/// function that reached for it itself would be one no test could call twice.
///
/// `dir` is where assembled disks live, not one disk's path: the name inside it is the
/// digest of the layer set, so two sessions whose executables differ get two disks rather
/// than writing over each other.
///
/// With no `sources` the builtin layer is handed over as it is — a base layer is a valid
/// image on its own, so the common case costs no stitch at all.
///
/// **Blocking, and says so by not being `async`.** Reading a directory of executables and
/// writing it back out as an EROFS is file work from end to end; a caller on a runtime owes
/// this a thread of its own.
pub fn assemble(
    sources: &[AbinSource],
    builtin: Option<&Path>,
    store: &LayerStore,
    dir: &Path,
) -> anyhow::Result<Assembled> {
    let (builtin, builtin_tree) = Builtin::host().layer(builtin, store)?;

    if sources.is_empty() {
        return Ok(Assembled {
            disk: builtin.erofs,
            format: BaseFormat::Raw,
        });
    }

    let mut layers = vec![builtin];
    // Kept beside the layers, because the collision check wants exactly these and reading
    // them back out of the images they were just written to would be a second pass over
    // every executable in the session.
    let mut trees = vec![builtin_tree];
    for source in sources {
        let named = source.file_path().ok_or_else(|| {
            anyhow::anyhow!(
                "{} names its executables by `{}`, which this server has no provider for",
                source.url,
                source.scheme()
            )
        })?;
        anyhow::ensure!(
            named.is_dir(),
            "{} is not a directory on this host",
            named.display()
        );
        let (layer, tree) = layer_of(named, store)?;
        layers.push(layer);
        trees.push(tree);
    }

    refuse_collisions(&trees)?;

    let set = LayerId::of(
        layers
            .iter()
            .map(|layer| layer.id.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    let disk = stitch(&layers, &dir.join(set.file_stem()))?;
    reclaim(dir, set.file_stem());
    Ok(Assembled {
        disk,
        format: BaseFormat::Vmdk,
    })
}

/// How long an assembled disk nobody has asked for again is kept.
///
/// Generous, because the cost of keeping one is a descriptor and a metadata EROFS while the
/// cost of removing one early is nothing at all — a stitch is derived and is rewritten on
/// every assembly anyway. Long enough that it is only ever a disk from another day.
const KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Drop the assembled disks in `dir` that nothing has asked for in a day, except `keep`.
///
/// A caller rebuilding a `target/release` names a new layer set on every build, so without
/// this the directory grows by one descriptor and one fsmeta per build and never shrinks.
/// Nothing tracks which sets are in use, so age stands in for it — and safely: a guest that
/// is running already holds these open, and an unlinked file a process has open stays whole
/// until it lets go.
///
/// Best effort throughout, like the layer store's own sweep. A directory that could not be
/// tidied is still a directory that works.
fn reclaim(dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_ours = name.ends_with(".vmdk") || name.ends_with(".fsmeta.erofs");
        if !is_ours || name.starts_with(keep) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|when| when.elapsed().unwrap_or_default() > KEEP)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// One directory as a layer, named by what is in it — and the tree it was named from.
///
/// Content-addressed rather than named for the path, so that a directory somebody is
/// rebuilding gets a new layer each time its contents change and the same one when they do
/// not. The tree is read once and hashed from what was read, rather than walking the
/// directory twice — and handed back for the same reason, since the caller needs the names
/// in it and reading them out of the image would be a third pass.
fn layer_of(dir: &Path, store: &LayerStore) -> anyhow::Result<(Layer, FileTree)> {
    let tree =
        tree::from_dir(dir).map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?;
    let id = digest(&tree)?;
    let layer = store.put(&id, &tree)?;
    Ok((layer, tree))
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

/// Refuse a name two layers both answer to.
///
/// Never resolved by precedence. `/abin` goes first on `PATH` so that what cortex undertakes
/// to provide is not shadowed, and a caller's layer silently shadowing it would make that
/// ordering pointless — while the other order would hide work somebody deliberately
/// supplied. Somebody who really means to replace a shipped executable replaces the whole
/// bottom layer with [`DIR_ENV`].
///
/// Only the top level is compared, because that is what `/abin` is: a flat directory of
/// commands, and two of them answering to one name is the collision. A subdirectory two
/// layers both have is not one.
///
/// A [`Duplicate`] and not a message, so that the client hears this one rather than a
/// server-side note about a session it thinks it configured.
fn refuse_collisions(trees: &[FileTree]) -> anyhow::Result<()> {
    use std::collections::HashMap;

    let mut seen: HashMap<&std::ffi::OsStr, usize> = HashMap::new();
    for (index, tree) in trees.iter().enumerate() {
        for name in tree.root.entries.keys() {
            if let Some(first) = seen.insert(name, index) {
                let which = |n: usize| {
                    if n == 0 {
                        "the ones cortex provides".to_string()
                    } else {
                        format!("directory {n}")
                    }
                };
                return Err(Duplicate(format!(
                    "{:?} is in two of this session's executable directories — in {} and in \
                     {}. Rename one, or replace cortex's own with {DIR_ENV}.",
                    name,
                    which(first),
                    which(index)
                ))
                .into());
            }
        }
    }
    Ok(())
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

        let (layer, _) = Builtin::host().layer(Some(builtin.path()), &store).unwrap();

        let tree = layer.tree(tree::Contents::Skip).unwrap();
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
        let same = layer_of(first.path(), &store).unwrap().0.id;
        assert_eq!(layer_of(first.path(), &store).unwrap().0.id, same);

        std::fs::write(first.path().join("mem"), b"#!/bin/sh\necho changed\n").unwrap();
        assert_ne!(
            layer_of(first.path(), &store).unwrap().0.id,
            same,
            "a changed executable named the same layer"
        );
    }

    #[test]
    fn with_no_named_directories_the_builtin_layer_is_attached_as_it_is() {
        let builtin = dir_of(&["mem"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let assembled =
            assemble(&[], Some(builtin.path()), &store, &work.path().join("abin")).unwrap();

        assert_eq!(assembled.format, BaseFormat::Raw);
        assert_eq!(
            assembled.disk.extension().unwrap(),
            "erofs",
            "the common case should not have been stitched"
        );
    }

    #[test]
    fn a_named_directory_is_layered_over_cortexs_own() {
        let builtin = dir_of(&["mem", "index"]);
        let caller = dir_of(&["mytool"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let assembled = assemble(
            &[AbinSource::of_path(caller.path()).unwrap()],
            Some(builtin.path()),
            &store,
            &work.path().join("abin"),
        )
        .unwrap();

        assert_eq!(assembled.format, BaseFormat::Vmdk);
        let merged = tree::from_erofs(
            &assembled.disk.with_extension("fsmeta.erofs"),
            tree::Contents::Skip,
        )
        .unwrap();
        for name in [&b"mem"[..], b"index", b"mytool"] {
            assert!(
                merged.get(name).is_some(),
                "{:?} is missing",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// Two sessions whose executables differ must get two disks, or a running guest would be
    /// reading a file somebody else was replacing.
    #[test]
    fn two_different_sets_are_two_disks() {
        let builtin = dir_of(&["mem"]);
        let one = dir_of(&["one"]);
        let two = dir_of(&["two"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());
        let into = work.path().join("abin");

        let disk = |source: &Path| {
            assemble(
                &[AbinSource::of_path(source).unwrap()],
                Some(builtin.path()),
                &store,
                &into,
            )
            .unwrap()
            .disk
        };
        let (a, b) = (disk(one.path()), disk(two.path()));
        assert_ne!(a, b, "one disk was written twice");
        assert!(a.is_file() && b.is_file());
    }

    #[test]
    fn a_name_in_two_places_is_refused_by_name() {
        let builtin = dir_of(&["mem"]);
        let caller = dir_of(&["mem"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let error = assemble(
            &[AbinSource::of_path(caller.path()).unwrap()],
            Some(builtin.path()),
            &store,
            &work.path().join("abin"),
        )
        .unwrap_err();

        let said = error.to_string();
        assert!(said.contains("mem"), "{said}");
        assert!(said.contains("cortex"), "{said}");
        // And as itself, not just as a message: this is the one assembly failure the client
        // hears about, and `Guest::boot` tells the two apart by downcasting to exactly this.
        assert!(
            error.downcast_ref::<Duplicate>().is_some(),
            "a collision was not a `Duplicate`, so the client would never be told"
        );
    }

    /// The other side of it: a failure that is this host's gap and not the caller's mistake
    /// must *not* come back as a collision, or a session would be refused for it.
    #[test]
    fn nothing_else_is_read_as_a_collision() {
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());
        let error = assemble(&[], None, &store, &work.path().join("abin")).unwrap_err();
        assert!(error.downcast_ref::<Duplicate>().is_none(), "{error}");
    }

    #[test]
    fn a_scheme_with_no_provider_is_refused_by_name() {
        let builtin = dir_of(&["mem"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let err = assemble(
            &[AbinSource::new("https://example.com/bin")],
            Some(builtin.path()),
            &store,
            &work.path().join("abin"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("https"), "{err}");
    }
}
