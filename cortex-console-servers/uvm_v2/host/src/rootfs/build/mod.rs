//! Making an image: a declaration, and the layers it turns into.
//!
//! # What a step becomes here
//!
//! `ENV` and `WORKDIR` are not run at all: they are what the image *states*, accumulated over
//! what its base already stated and written into the manifest beside the disk.
//!
//! A `COPY` is a layer of its own, written straight out of the host's directory. It is named
//! by what it holds, so two images copying the same bytes to the same place are two manifests
//! naming one layer.
//!
//! A `RUN` is a command in a guest, and what starts a guest is a session.

mod pull;
mod step;

use cortex::rootfs_v2::RootFsV2;

/// Build what `rootfs` declares, and hand back what the image is called.
///
/// An image that was already here is the same answer as one just built, which is what makes
/// asking twice cost a file test — so whatever names a build has to be worked out before
/// anything is pulled, out of the declaration and the files a `COPY` reads.
pub async fn build(rootfs: RootFsV2) -> anyhow::Result<()> {
    let base = pull::pull(rootfs.base.as_str()).await?;
    todo!("{} step(s) over {}", rootfs.steps.len(), base.digest()?)
}
