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
//! [`CommitResp`](cortex::console::CommitResp), because that names an image and the guest
//! has no idea what an image is. So the server replaces the answer on the way past — the same
//! thing it already does with `init`.

use std::path::Path;

use cortex::console::{CommitCall, ImageSource};
use cortex_uvm_console::built::{BuiltStore, LOCAL_HOST};
use cortex_uvm_console::layer::{Layer, LayerId, LayerStore};

/// Turn the tar a guest left in `scratch` into an image named by `commit.id`.
///
/// `base` is named by id rather than handed over as layers: an id is what identifies one, and
/// the store is the way from an id to the thing — so a caller holding a session does not have
/// to hold a copy of every layer's data map as well.
pub async fn keep(
    commit: &CommitCall,
    scratch: &Path,
    base: &[LayerId],
    layers: LayerStore,
    built: BuiltStore,
) -> anyhow::Result<ImageSource> {
    let id = LayerId::parse(&commit.id)?;
    let tar = scratch.join(crate::contract::LAYER_TAR);
    anyhow::ensure!(
        tar.is_file(),
        "the guest said it wrote a layer and there is none at {}",
        tar.display()
    );

    // The tar is the session's scratch and not the store's, and it is gone either way: read
    // whole it is a layer, and left behind after a failure it is what the *next* commit in
    // this session would find and keep under a new name.
    let tar = Consumed(tar);

    let tree = microsandbox_image::tar::ingest_tar(
        tokio::fs::File::open(tar.path()).await?,
        &microsandbox_image::tree::ResourceLimits::default(),
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("reading this session's layer: {e:?}"))?;

    // Named by the digest of the tar, which is what OCI calls a diff id — so two sessions that
    // wrote the same thing share one layer even under two image names.
    //
    // Streamed rather than read: a session's diff is as big as what it wrote, and there is no
    // reason for all of it to be resident at once to be hashed.
    let layer_id = digest_of(tar.path()).await?;

    // Writing the layer as an EROFS and stitching the image are both file work the size of
    // what the session wrote, so they go where the rest of this crate puts that kind of work
    // rather than on the runtime's own thread.
    let (base, env, working_dir) = (
        base.to_vec(),
        commit.env.clone(),
        commit.working_dir.clone(),
    );
    let kept = id.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let written = layers.put(&layer_id, &tree)?;

        let mut over: Vec<Layer> = Vec::with_capacity(base.len() + 1);
        for existing in &base {
            over.push(layers.get(existing)?);
        }
        over.push(written);

        built.keep(&kept, &over, env, working_dir)
    })
    .await
    .map_err(|e| anyhow::anyhow!("keeping this session's layer: {e}"))??;

    Ok(ImageSource::new(format!("{LOCAL_HOST}built@{id}")))
}

/// A file that is read once and then gone, whichever way this returns.
struct Consumed(std::path::PathBuf);

impl Consumed {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Consumed {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The digest of a file, a block at a time.
async fn digest_of(path: &Path) -> anyhow::Result<LayerId> {
    use sha2::{Digest as _, Sha256};
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    LayerId::parse(&format!("sha256:{:x}", hasher.finalize()))
}
