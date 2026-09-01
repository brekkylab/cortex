//! The host's half of a commit: the layer a guest wrote, turned into an image.
//!
//! The guest walks its own upperdir and leaves a tar in the scratch it was given. Everything
//! after that is here — reading the tar, putting it in the layer store, and stitching it onto
//! the layers the session's base was made of.
//!
//! # Why the two halves answer differently
//!
//! The guest answers a [`GuestCommit`](crate::contract::GuestCommit), which says the tar is
//! written and how big it is. It cannot answer a
//! [`CommitResult`](cortex::console::CommitResult), because that names an image and the guest
//! has no idea what an image is. So the server replaces the answer on the way past — the same
//! thing it already does with `init`.

use std::path::Path;

use cortex::console::{Commit, CommitResult, ImageSource};
use cortex_uvm_console::built::{BuiltStore, LOCAL_HOST};
use cortex_uvm_console::layer::{Layer, LayerId, LayerStore};

/// Turn the tar a guest left in `scratch` into an image named by `commit.id`.
///
/// `base` is named by id rather than handed over as layers: an id is what identifies one, and
/// the store is the way from an id to the thing — so a caller holding a session does not have
/// to hold a copy of every layer's data map as well.
pub async fn keep(
    commit: &Commit,
    scratch: &Path,
    base: &[LayerId],
    layers: &LayerStore,
    built: &BuiltStore,
) -> anyhow::Result<CommitResult> {
    let id = LayerId::parse(&commit.id)?;
    let tar = scratch.join(crate::contract::LAYER_TAR);
    anyhow::ensure!(
        tar.is_file(),
        "the guest said it wrote a layer and there is none at {}",
        tar.display()
    );

    let tree = microsandbox_image::tar::ingest_tar(
        tokio::fs::File::open(&tar).await?,
        &microsandbox_image::tree::ResourceLimits::default(),
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("reading this session's layer: {e:?}"))?;

    // Named by the digest of the tar, which is what OCI calls a diff id — so two sessions that
    // wrote the same thing share one layer even under two image names.
    let layer_id = LayerId::of(&std::fs::read(&tar)?);
    let written = layers.put(&layer_id, &tree)?;

    let mut over: Vec<Layer> = Vec::with_capacity(base.len() + 1);
    for existing in base {
        over.push(layers.get(existing)?);
    }
    over.push(written);

    built.keep(&id, &over, commit.env.clone(), commit.working_dir.clone())?;

    // The tar has been read and is the session's scratch, not the store's. Removed now rather
    // than left for the scratch's own cleanup, so a second commit in one session does not find
    // the first one's.
    let _ = std::fs::remove_file(&tar);

    Ok(CommitResult {
        image: ImageSource::new(format!("{LOCAL_HOST}built@{id}")),
    })
}
