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
//!
//! # Building the same thing twice
//!
//! Three of the four steps are functions of what came before them: `ENV` and `WORKDIR` change
//! what the image states, and a `COPY` writes a layer out of the build context with its uids
//! and mtimes flattened, so the same files give a layer with the same digest and the store
//! already has it. Doing any of them again is cheap and lands exactly where it landed before.
//!
//! A `RUN` is the one that does not work like that — see [`rootfs`](crate::rootfs)'s docs —
//! and it is also the only expensive one, since it boots a machine. So it is the only step
//! that is remembered, and remembering it is what makes a declaration built twice one image
//! rather than two that differ in a timestamp.

mod pull;
mod step;

pub use step::forget_runs;

use cortex::rootfs_v2::RootFsV2;

use crate::rootfs::Image;

/// Build what `rootfs` declares, and hand back the image.
///
/// A second build of the same declaration is the same digest as the first, and costs the base
/// (already here, so no registry), the deterministic steps again, and one file test per `RUN`.
/// Which is the property both callers depend on: `rootfs build` so that a client can name what
/// it built, and a session so that opening one twice on the same declaration does not boot two
/// different images.
///
/// The image and not a digest, and nothing written to stdout — one of the two callers is a
/// session, whose stdout is the protocol wire. Progress goes to stderr, which is where
/// everything this binary says about itself goes.
pub async fn build(rootfs: RootFsV2) -> anyhow::Result<Image> {
    // Where the build is run from is what it is built against: a declaration names its
    // `COPY` sources relative to a directory it deliberately does not carry.
    let context = std::env::current_dir()?;

    let mut image = base(&rootfs.base).await?;
    for declared in rootfs.steps {
        image = step::step(image, declared, &context).await?;
    }

    Ok(image)
}

/// What a declaration's base names: an image already in the store, or a reference to pull.
///
/// A digest is the one spelling that cannot be a reference — a registry has no name of that
/// shape — so the two are told apart by it rather than by asking the store first and the
/// network second. Which also means a base that *looks* like a digest and is not here is a
/// missing image and not a pull that was never tried.
pub async fn base(named: &str) -> anyhow::Result<Image> {
    anyhow::ensure!(
        !named.is_empty(),
        "a rootfs is a base and the steps over it, and this one named no base"
    );

    match named.strip_prefix("sha256:") {
        Some(_) => Image::load(&named.parse()?),
        None => pull::pull(named).await,
    }
}
