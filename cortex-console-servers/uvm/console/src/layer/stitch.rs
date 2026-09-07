//! Several layers, read as one filesystem.
//!
//! ```text
//! <into>.vmdk  =  [ <into>.fsmeta.erofs, layer 0, layer 1, … ]
//!                   │
//!                   └── inodes whose data lives in the layers behind it, addressed by
//!                       device index — which is the order they appear in above
//! ```
//!
//! The merge is `merge_layers_with_provenance`: later layers win, a character device `0:0`
//! deletes what it stands over, and a directory carrying `trusted.overlay.opaque` hides
//! whatever the layers below put in it. What comes back is the merged tree and a map from
//! each path to the layer it came from — which is exactly what `write_fsmeta` needs to point
//! an inode at the right extent.
//!
//! **Nothing here reads a layer's file data.** `write_fsmeta` takes every file's size and
//! location from the provenance map and the layer data maps, so the trees are read with
//! [`Contents::Skip`] and the bytes are never touched. Stitching a gigabyte of layers costs
//! one pass over their metadata.
//!
//! The layers stay where they are. A stitch is a way of reading them, not a copy of them,
//! which is what makes two images built on one base cost one copy of it.
//!
//! # The one thing that has to be checked
//!
//! Two accounts of where a layer's bytes are have to agree. `write_fsmeta` places layer *n*
//! at `fsmeta_blocks + Σ total_blocks[0..n]`, taking each count from the layer's map; the
//! descriptor places it after however many blocks the files before it actually hold. A layer
//! whose file has stopped matching its map desynchronizes the two, and the failure is silent
//! — the guest mounts, reads at the wrong offset, and gets another layer's bytes with no
//! error anywhere. So [`checked`] compares the map against the file before any of it is
//! written. It is one `stat` per layer against a disk nothing can otherwise catch.

use std::path::{Path, PathBuf};

use microsandbox_image::erofs::fsmeta::write_fsmeta;
use microsandbox_image::tree::merge_layers_with_provenance;

use super::tree::Contents;
use super::{Layer, atomic, vmdk};

/// Stitch `layers`, bottom first, into a disk at `<into>.vmdk`.
///
/// Writes two files — `<into>.fsmeta.erofs`, the merged metadata, and `<into>.vmdk`, the
/// descriptor naming it and every layer — and hands back the second.
pub fn stitch(layers: &[Layer], into: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!layers.is_empty(), "a disk needs at least one layer");

    let mut trees = Vec::with_capacity(layers.len());
    for layer in layers {
        checked(layer)?;
        trees.push(
            layer
                .tree(Contents::Skip)
                .map_err(|e| anyhow::anyhow!("reading layer {}: {e}", layer.id))?,
        );
    }
    let (merged, provenance) = merge_layers_with_provenance(trees);
    let maps: Vec<_> = layers.iter().map(|layer| layer.map.clone()).collect();

    let fsmeta = with_suffix(into, "fsmeta.erofs");
    if let Some(parent) = fsmeta.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("making {}: {e}", parent.display()))?;
    }

    // Written beside its destination and renamed, like everything else here: a descriptor
    // naming a half-written fsmeta is a disk that mounts to nothing.
    let temporary = atomic::unique(fsmeta.parent().unwrap_or(Path::new(".")), "fsmeta");
    write_fsmeta(&merged, &provenance, &maps, &temporary).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        anyhow::anyhow!("writing the merged metadata: {e}")
    })?;
    std::fs::rename(&temporary, &fsmeta).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        anyhow::anyhow!("placing {}: {e}", fsmeta.display())
    })?;

    let mut extents: Vec<&Path> = Vec::with_capacity(layers.len() + 1);
    extents.push(&fsmeta);
    extents.extend(layers.iter().map(|layer| layer.erofs.as_path()));

    let descriptor = with_suffix(into, "vmdk");
    vmdk::write_descriptor(&descriptor, &extents)?;
    Ok(descriptor)
}

/// An EROFS block. Stated here because `microsandbox-image` keeps its own copy `pub(crate)`,
/// and it is a constant of the format rather than of that crate.
const BLOCK: u64 = 4096;

/// Refuse a layer whose image is no longer the size its map says it is.
///
/// A stale pair is not something a working store produces — a layer is named by a digest and
/// both halves are written together — but a killed write, a half-restored backup or a hand
/// edit all make one, and every one of them ends as a guest reading the wrong bytes rather
/// than as an error. Cheap to rule out, and impossible to notice later.
fn checked(layer: &Layer) -> anyhow::Result<()> {
    let size = std::fs::metadata(&layer.erofs)
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", layer.erofs.display()))?
        .len();
    let expected = layer.map.total_blocks as u64 * BLOCK;
    anyhow::ensure!(
        size == expected,
        "layer {} is {size} bytes but its map accounts for {expected} \
         ({} blocks of {BLOCK}) — the two no longer describe the same layer",
        layer.id,
        layer.map.total_blocks,
    );
    Ok(())
}

/// `into` with `suffix` after a dot: `built/abc` and `vmdk` give `built/abc.vmdk`.
///
/// Not [`Path::with_extension`], which would eat an extension `into` already has — and a
/// layer id is hex, so it will not have one, but a name somebody chooses later might.
fn with_suffix(into: &Path, suffix: &str) -> PathBuf {
    let mut name = into.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use microsandbox_image::tree::{
        DeviceNode, DirectoryNode, FileData, FileTree, InodeMetadata, RegularFileId,
        RegularFileNode, TreeNode, Xattr,
    };

    use super::super::tree::{self, describe};
    use super::super::{LayerId, LayerStore};
    use super::*;

    const OPAQUE: &[u8] = b"trusted.overlay.opaque";

    fn meta(mode: u16) -> InodeMetadata {
        InodeMetadata {
            uid: 0,
            gid: 0,
            mode,
            mtime: 0,
            mtime_nsec: 0,
        }
    }

    fn file(contents: &str) -> TreeNode {
        TreeNode::RegularFile(RegularFileNode {
            id: RegularFileId::new(),
            metadata: meta(0o644),
            xattrs: vec![],
            data: FileData::Memory(contents.as_bytes().to_vec()),
            nlink: 1,
        })
    }

    fn dir() -> TreeNode {
        TreeNode::Directory(DirectoryNode::new(meta(0o755)))
    }

    fn base() -> FileTree {
        let mut tree = FileTree::new();
        tree.insert(b"etc", dir()).unwrap();
        tree.insert(b"etc/keep", file("kept\n")).unwrap();
        tree.insert(b"etc/replaced", file("old\n")).unwrap();
        tree.insert(b"etc/deleted", file("doomed\n")).unwrap();
        tree.insert(b"var", dir()).unwrap();
        tree.insert(b"var/cache", dir()).unwrap();
        tree.insert(b"var/cache/stale", file("stale\n")).unwrap();
        tree
    }

    fn upper() -> FileTree {
        let mut tree = FileTree::new();
        tree.insert(b"etc", dir()).unwrap();
        tree.insert(b"etc/replaced", file("new and longer\n"))
            .unwrap();
        tree.insert(b"etc/added", file("added\n")).unwrap();
        tree.insert(
            b"etc/deleted",
            TreeNode::CharDevice(DeviceNode {
                metadata: meta(0o644),
                major: 0,
                minor: 0,
            }),
        )
        .unwrap();
        tree.insert(b"var", dir()).unwrap();
        let mut emptied = DirectoryNode::new(meta(0o755));
        emptied.xattrs.push(Xattr {
            name: OPAQUE.to_vec(),
            value: b"y".to_vec(),
        });
        tree.insert(b"var/cache", TreeNode::Directory(emptied))
            .unwrap();
        tree
    }

    #[test]
    fn stitching_merges_the_layers_it_is_given() {
        let dir_ = tempfile::tempdir().unwrap();
        let store = LayerStore::open(&dir_.path().join("layers")).unwrap();
        let lower = store.put(&LayerId::of(b"base"), &base()).unwrap();
        let over = store.put(&LayerId::of(b"upper"), &upper()).unwrap();

        let descriptor = stitch(&[lower, over], &dir_.path().join("built/image")).unwrap();
        assert!(descriptor.is_file());
        assert_eq!(descriptor.extension().unwrap(), "vmdk");

        let fsmeta = dir_.path().join("built/image.fsmeta.erofs");
        assert!(
            fsmeta.is_file(),
            "no fsmeta beside {}",
            descriptor.display()
        );
        let merged = tree::from_erofs(&fsmeta, Contents::Skip).unwrap();

        assert!(
            merged.get(b"etc/keep").is_some(),
            "the base's own file went missing"
        );
        assert!(
            merged.get(b"etc/added").is_some(),
            "the upper's file did not arrive"
        );
        assert!(
            merged.get(b"etc/deleted").is_none(),
            "the whiteout was not applied"
        );
        assert!(
            merged.get(b"var/cache/stale").is_none(),
            "the opaque directory was not applied"
        );
        assert!(
            merged.get(b"var/cache").is_some(),
            "the opaque directory itself went missing"
        );

        // The upper's version won, which its length is enough to show.
        match merged.get(b"etc/replaced") {
            Some(TreeNode::RegularFile(_)) => {}
            other => panic!("etc/replaced is {}", describe(other)),
        }
    }

    #[test]
    fn the_descriptor_names_the_fsmeta_first_then_the_layers() {
        let dir_ = tempfile::tempdir().unwrap();
        let store = LayerStore::open(&dir_.path().join("layers")).unwrap();
        let lower = store.put(&LayerId::of(b"base"), &base()).unwrap();
        let over = store.put(&LayerId::of(b"upper"), &upper()).unwrap();
        let (lower_path, upper_path) = (lower.erofs.clone(), over.erofs.clone());

        let descriptor = stitch(&[lower, over], &dir_.path().join("built/image")).unwrap();
        let text = std::fs::read_to_string(&descriptor).unwrap();
        let extents: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("RW "))
            .filter_map(|line| line.split('"').nth(1))
            .collect();

        assert_eq!(extents.len(), 3, "{text}");
        assert!(extents[0].ends_with("image.fsmeta.erofs"), "{extents:?}");
        assert!(
            extents[1].ends_with(lower_path.file_name().unwrap().to_str().unwrap()),
            "the base layer is not the first extent behind the metadata: {extents:?}"
        );
        assert!(
            extents[2].ends_with(upper_path.file_name().unwrap().to_str().unwrap()),
            "{extents:?}"
        );
    }

    #[test]
    fn one_layer_stitches_too() {
        let dir_ = tempfile::tempdir().unwrap();
        let store = LayerStore::open(&dir_.path().join("layers")).unwrap();
        let only = store.put(&LayerId::of(b"base"), &base()).unwrap();

        let descriptor = stitch(&[only], &dir_.path().join("one")).unwrap();
        assert!(descriptor.is_file());
        let merged =
            tree::from_erofs(&dir_.path().join("one.fsmeta.erofs"), Contents::Skip).unwrap();
        assert!(
            merged.get(b"etc/deleted").is_some(),
            "a lone layer should be untouched"
        );
    }

    #[test]
    fn stitching_nothing_is_refused() {
        let dir_ = tempfile::tempdir().unwrap();
        assert!(stitch(&[], &dir_.path().join("none")).is_err());
    }

    /// The silent one: a layer and its map that no longer agree would stitch into a disk the
    /// guest reads at the wrong offset, and nothing downstream can tell.
    #[test]
    fn a_layer_that_does_not_match_its_map_is_refused() {
        use std::io::Write as _;

        let dir_ = tempfile::tempdir().unwrap();
        let store = LayerStore::open(&dir_.path().join("layers")).unwrap();
        let lower = store.put(&LayerId::of(b"base"), &base()).unwrap();
        let over = store.put(&LayerId::of(b"upper"), &upper()).unwrap();

        // One block more than the map accounts for. Everything stacked behind this layer in
        // the descriptor now sits a block further along than the device table says.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&lower.erofs)
            .unwrap();
        file.write_all(&[0u8; BLOCK as usize]).unwrap();
        drop(file);

        let into = dir_.path().join("built/image");
        let err = stitch(&[lower, over], &into).unwrap_err().to_string();
        assert!(err.contains("no longer describe the same layer"), "{err}");
        assert!(
            !with_suffix(&into, "vmdk").exists(),
            "a disk was written anyway"
        );
    }
}
