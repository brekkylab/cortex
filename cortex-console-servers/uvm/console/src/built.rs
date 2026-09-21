//! An image a commit made: what it is on disk, and how one is written.
//!
//! ```text
//! <root>/<id>.vmdk           the disk a boot attaches
//! <root>/<id>.fsmeta.erofs   the merged metadata behind it
//! <root>/<id>.manifest       the layers it is made of, and what it states
//! ```
//!
//! # The manifest is what makes it an image
//!
//! Without the list of layers it would be a disk nobody could add anything to — the same wall
//! a pulled image runs into, and the reason those are seeded into the layer store rather than
//! used where they lie. A commit onto a built image is a layer stitched onto the layers this
//! names.
//!
//! It is also where what the image *states* lives. A stitched disk says nothing about running
//! a process in it, so what a commit stated travels beside the disk rather than inside it.
//!
//! # Why this is in the library
//!
//! A built image's shape on disk is the same kind of thing as the layer store's: something
//! both the server and a test have to be able to write and read. Two definitions of it would
//! be two that drift.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layer::{Layer, LayerId, LayerStore, atomic, stitch};

/// The reserved host a locally built image is named under.
///
/// `.local` is reserved by RFC 6762, so this can never be a registry somebody reaches, and the
/// whole reference is still real OCI grammar — which matters, because a server turns what
/// `init` named into a reference before anything else happens.
pub const LOCAL_HOST: &str = "cortex.local/";

/// What a built image is made of and what it says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The layers, bottom first, as ids in the layer store.
    pub layers: Vec<String>,
    /// `KEY=value`, as an image config spells it.
    pub env: Vec<String>,
    pub working_dir: Option<String>,
}

/// Where images made here live.
pub struct BuiltStore {
    root: PathBuf,
}

impl BuiltStore {
    pub fn open(root: &Path) -> anyhow::Result<BuiltStore> {
        std::fs::create_dir_all(root)
            .map_err(|e| anyhow::anyhow!("making {}: {e}", root.display()))?;
        Ok(BuiltStore {
            root: root.to_path_buf(),
        })
    }

    /// Whether an image by this name was built here.
    ///
    /// A file test, which is why it can be answered at `init` where whether a registry has
    /// something cannot.
    pub fn has(&self, id: &LayerId) -> bool {
        self.manifest_path(id).is_file() && self.disk(id).is_file()
    }

    /// The disk a boot attaches for this image.
    pub fn disk(&self, id: &LayerId) -> PathBuf {
        self.root.join(format!("{}.vmdk", id.file_stem()))
    }

    pub fn manifest(&self, id: &LayerId) -> anyhow::Result<Manifest> {
        read_manifest(&self.manifest_path(id))
    }

    /// The layers this image is made of, as they stand in `layers`.
    pub fn layers(&self, id: &LayerId, layers: &LayerStore) -> anyhow::Result<Vec<Layer>> {
        self.manifest(id)?
            .layers
            .iter()
            .map(|spelling| layers.get(&LayerId::parse(spelling)?))
            .collect()
    }

    /// Stitch `over` into an image named `id`, and record what it is.
    ///
    /// The manifest is written **after** the disk, so an image that [`has`](Self::has) says is
    /// here is one that can be booted rather than one that is half-made.
    pub fn keep(
        &self,
        id: &LayerId,
        over: &[Layer],
        env: Vec<String>,
        working_dir: Option<String>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!over.is_empty(), "an image needs at least one layer");

        stitch(over, &self.root.join(id.file_stem()))?;
        write_manifest(
            &Manifest {
                layers: over.iter().map(|layer| layer.id.to_string()).collect(),
                env,
                working_dir,
            },
            &self.manifest_path(id),
        )
    }

    fn manifest_path(&self, id: &LayerId) -> PathBuf {
        self.root.join(format!("{}.manifest", id.file_stem()))
    }
}

pub fn write_manifest(manifest: &Manifest, path: &Path) -> anyhow::Result<()> {
    let encoded = bson::serialize_to_vec(manifest)
        .map_err(|e| anyhow::anyhow!("encoding a built image's manifest: {e}"))?;
    atomic::replace(path, &encoded)
}

pub fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let encoded =
        std::fs::read(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    bson::deserialize_from_slice(&encoded)
        .map_err(|e| anyhow::anyhow!("decoding {}: {e}", path.display()))
}

/// The digest out of a `built@sha256:…`, which is what follows [`LOCAL_HOST`] in a reference.
pub fn digest_of(rest: &str) -> anyhow::Result<LayerId> {
    let digest = rest
        .split_once('@')
        .map(|(_repository, digest)| digest)
        .ok_or_else(|| anyhow::anyhow!("{LOCAL_HOST}{rest} names no digest"))?;
    LayerId::parse(digest)
}

#[cfg(test)]
mod tests {
    use microsandbox_image::tree::{DirectoryNode, FileTree, InodeMetadata, TreeNode};

    use super::*;

    fn sample() -> Manifest {
        Manifest {
            layers: vec!["sha256:aa".into(), "sha256:bb".into()],
            env: vec!["TZ=UTC".into()],
            working_dir: Some("/srv".into()),
        }
    }

    fn a_layer(store: &LayerStore, name: &[u8]) -> Layer {
        let mut tree = FileTree::new();
        tree.insert(
            name,
            TreeNode::Directory(DirectoryNode::new(InodeMetadata {
                uid: 0,
                gid: 0,
                mode: 0o755,
                mtime: 0,
                mtime_nsec: 0,
            })),
        )
        .unwrap();
        store.put(&LayerId::of(name), &tree).unwrap()
    }

    #[test]
    fn a_manifest_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m");
        write_manifest(&sample(), &path).unwrap();
        assert_eq!(read_manifest(&path).unwrap(), sample());
    }

    #[test]
    fn a_manifest_that_states_nothing_round_trips_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m");
        let bare = Manifest {
            layers: vec!["sha256:aa".into()],
            env: Vec::new(),
            working_dir: None,
        };
        write_manifest(&bare, &path).unwrap();
        assert_eq!(read_manifest(&path).unwrap(), bare);
    }

    #[test]
    fn an_image_kept_can_be_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let layers = LayerStore::open(&dir.path().join("layers")).unwrap();
        let built = BuiltStore::open(&dir.path().join("built")).unwrap();
        let id = LayerId::of(b"an image");

        assert!(!built.has(&id));
        built
            .keep(
                &id,
                &[a_layer(&layers, b"lower"), a_layer(&layers, b"upper")],
                vec!["TZ=UTC".into()],
                Some("/srv".into()),
            )
            .unwrap();

        assert!(built.has(&id));
        assert!(built.disk(&id).is_file());
        let manifest = built.manifest(&id).unwrap();
        assert_eq!(manifest.layers.len(), 2);
        assert_eq!(manifest.env, vec!["TZ=UTC".to_string()]);
        assert_eq!(built.layers(&id, &layers).unwrap().len(), 2);
    }

    #[test]
    fn an_image_that_was_never_built_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let built = BuiltStore::open(dir.path()).unwrap();
        let id = LayerId::of(b"never made");
        assert!(!built.has(&id));
        assert!(built.manifest(&id).is_err());
    }

    #[test]
    fn an_image_of_nothing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let built = BuiltStore::open(dir.path()).unwrap();
        assert!(
            built
                .keep(&LayerId::of(b"empty"), &[], Vec::new(), None)
                .is_err()
        );
    }

    #[test]
    fn a_name_that_is_not_one_is_refused() {
        assert!(digest_of("built").is_err(), "no digest at all");
        assert!(digest_of("built@nonsense").is_err());
    }
}
