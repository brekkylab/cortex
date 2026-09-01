//! Layers on disk, by id.
//!
//! A layer is two files: `<id>.erofs` and `<id>.map`. Both have to be there for the layer to
//! exist. The map cannot be rebuilt from the image, so an image without one is a file nothing
//! can stitch, and counting it as present would mean handing out a layer that fails later
//! instead of one that is missing now.

use std::path::{Path, PathBuf};

use microsandbox_image::erofs::{ErofsDataMap, write_erofs};
use microsandbox_image::tree::FileTree;

use super::tree::{self, Contents};
use super::{LayerId, atomic, map};

/// Where layers live.
pub struct LayerStore {
    root: PathBuf,
}

/// One layer: the image, and the map saying where its files are inside it.
pub struct Layer {
    pub id: LayerId,
    pub erofs: PathBuf,
    pub map: ErofsDataMap,
}

impl Layer {
    /// This layer's tree, read back out of its image.
    pub fn tree(&self, contents: Contents) -> anyhow::Result<FileTree> {
        tree::from_erofs(&self.erofs, contents)
    }
}

/// Written by hand rather than derived, so that a layer of fifty thousand files says which
/// layer it is instead of listing them. What identifies one is its id and where it is.
impl std::fmt::Debug for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layer")
            .field("id", &self.id)
            .field("erofs", &self.erofs)
            .field("files", &self.map.file_blocks.len())
            .field("blocks", &self.map.total_blocks)
            .finish()
    }
}

impl LayerStore {
    /// Open the store under `root`, making it if it is not there.
    pub fn open(root: &Path) -> anyhow::Result<LayerStore> {
        std::fs::create_dir_all(root)
            .map_err(|e| anyhow::anyhow!("making the layer store at {}: {e}", root.display()))?;
        Ok(LayerStore {
            root: root.to_path_buf(),
        })
    }

    /// Whether both halves of this layer are here.
    pub fn has(&self, id: &LayerId) -> bool {
        self.image_path(id).is_file() && self.map_path(id).is_file()
    }

    /// Put `tree` in the store under `id`, or hand back what is already there.
    ///
    /// The id is the caller's: it is a digest of whatever the tree was built from — a
    /// tarball, a directory, a manifest — and only the caller knows what that was. Putting
    /// the same id twice therefore does nothing, which is what makes a cached layer free.
    pub fn put(&self, id: &LayerId, tree: &FileTree) -> anyhow::Result<Layer> {
        if self.has(id) {
            return self.get(id);
        }

        let temporary = atomic::unique(&self.root, id.file_stem());
        let map = write_erofs(tree, &temporary).map_err(|e| {
            let _ = std::fs::remove_file(&temporary);
            anyhow::anyhow!("writing layer {id}: {e:?}")
        })?;

        // The map first, and the image last. An image with no map beside it is not a layer,
        // so the other order would leave a window in which `has` says yes and `get` fails.
        map::write(&map, &self.map_path(id))?;
        std::fs::rename(&temporary, self.image_path(id)).map_err(|e| {
            let _ = std::fs::remove_file(&temporary);
            anyhow::anyhow!("placing layer {id}: {e}")
        })?;

        Ok(Layer {
            id: id.clone(),
            erofs: self.image_path(id),
            map,
        })
    }

    /// The layer `id` names.
    pub fn get(&self, id: &LayerId) -> anyhow::Result<Layer> {
        anyhow::ensure!(self.has(id), "no layer {id} in {}", self.root.display());
        Ok(Layer {
            id: id.clone(),
            erofs: self.image_path(id),
            map: map::read(&self.map_path(id))?,
        })
    }

    fn image_path(&self, id: &LayerId) -> PathBuf {
        self.root.join(format!("{}.erofs", id.file_stem()))
    }

    fn map_path(&self, id: &LayerId) -> PathBuf {
        self.root.join(format!("{}.map", id.file_stem()))
    }
}

#[cfg(test)]
mod tests {
    use microsandbox_image::tree::{
        DirectoryNode, FileData, InodeMetadata, RegularFileId, RegularFileNode, TreeNode,
    };

    use super::super::tree::describe;
    use super::*;

    fn sample() -> FileTree {
        let mut tree = FileTree::new();
        let meta = |mode| InodeMetadata {
            uid: 0,
            gid: 0,
            mode,
            mtime: 0,
            mtime_nsec: 0,
        };
        tree.insert(
            b"etc",
            TreeNode::Directory(DirectoryNode::new(meta(0o755))),
        )
        .unwrap();
        tree.insert(
            b"etc/hello",
            TreeNode::RegularFile(RegularFileNode {
                id: RegularFileId::new(),
                metadata: meta(0o644),
                xattrs: vec![],
                data: FileData::Memory(b"hello\n".to_vec()),
                nlink: 1,
            }),
        )
        .unwrap();
        tree
    }

    #[test]
    fn a_layer_put_can_be_got_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(dir.path()).unwrap();
        let id = LayerId::of(b"sample");

        assert!(!store.has(&id));
        let put = store.put(&id, &sample()).unwrap();
        assert!(store.has(&id));
        assert!(put.erofs.is_file());

        let got = store.get(&id).unwrap();
        assert_eq!(got.erofs, put.erofs);
        assert_eq!(got.map.total_blocks, put.map.total_blocks);
        assert_eq!(got.map.file_blocks, put.map.file_blocks);

        let tree = got.tree(Contents::Read).unwrap();
        match tree.get(b"etc/hello") {
            Some(TreeNode::RegularFile(f)) => assert_eq!(f.data.read_all().unwrap(), b"hello\n"),
            other => panic!("etc/hello is {}", describe(other)),
        }
    }

    #[test]
    fn putting_the_same_id_twice_does_not_write_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(dir.path()).unwrap();
        let id = LayerId::of(b"sample");

        let first = store.put(&id, &sample()).unwrap();
        let stamp = std::fs::metadata(&first.erofs).unwrap().modified().unwrap();
        let second = store.put(&id, &sample()).unwrap();
        assert_eq!(
            std::fs::metadata(&second.erofs).unwrap().modified().unwrap(),
            stamp,
            "the layer was written again"
        );
    }

    #[test]
    fn getting_what_was_never_put_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(dir.path()).unwrap();
        let err = store
            .get(&LayerId::of(b"absent"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no layer"), "{err}");
    }

    #[test]
    fn a_layer_without_its_map_is_not_a_layer() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(dir.path()).unwrap();
        let id = LayerId::of(b"sample");
        let put = store.put(&id, &sample()).unwrap();

        std::fs::remove_file(put.erofs.with_extension("map")).unwrap();
        assert!(!store.has(&id), "a half-present layer counted as present");
        assert!(store.get(&id).is_err());
    }

    #[test]
    fn nothing_partial_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(dir.path()).unwrap();
        store.put(&LayerId::of(b"sample"), &sample()).unwrap();

        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.iter().all(|name| !name.contains("partial")),
            "{names:?}"
        );
    }

    /// Two layers in one store must not tread on each other's files.
    #[test]
    fn two_layers_keep_their_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(dir.path()).unwrap();

        let one = store.put(&LayerId::of(b"one"), &sample()).unwrap();
        let mut other = sample();
        other
            .insert(
                b"etc/second",
                TreeNode::RegularFile(RegularFileNode {
                    id: RegularFileId::new(),
                    metadata: InodeMetadata {
                        uid: 0,
                        gid: 0,
                        mode: 0o644,
                        mtime: 0,
                        mtime_nsec: 0,
                    },
                    xattrs: vec![],
                    data: FileData::Memory(b"second\n".to_vec()),
                    nlink: 1,
                }),
            )
            .unwrap();
        let two = store.put(&LayerId::of(b"two"), &other).unwrap();

        assert_ne!(one.erofs, two.erofs);
        assert!(
            one.tree(Contents::Skip).unwrap().get(b"etc/second").is_none(),
            "the second layer's file appeared in the first"
        );
        assert!(
            two.tree(Contents::Skip)
                .unwrap()
                .get(b"etc/second")
                .is_some()
        );
    }
}
