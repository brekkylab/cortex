//! `image` — the images a session boots from, tended without one.
//!
//! Three commands, each one a process that starts, does its work and exits: `build` makes an
//! image, `list` says which ones are here, `remove` deletes one. None of them speaks the
//! console protocol, so none of them touches stdin or stdout as a wire — a client that wants
//! an image spawns this binary a second time for the purpose, and the session half only ever
//! reads what these three leave behind.
//!
//! What is under [`home`] is content-addressed, which is what makes a rebuild cheap: a layer
//! whose digest is already there is not written again, and an image whose id is already
//! there is not built again.
//!
//! The shape of it is `microsandbox-image`'s, because that crate writes most of what is in
//! there: [`cache`] is a [`GlobalCache`] over [`home`] itself, so a pulled layer lands in
//! `layers/` under its diff_id and stays there, and the merged metadata and the descriptor
//! naming a stack of them land in `fsmeta/` and `vmdk/` beside it. What that layout has no
//! shelf for is an image as *this* end states it — a base with steps over it, which no
//! registry has heard of and nothing keys by a reference — so those go under [`images`].

mod build;
mod digest;
mod image;
mod layer;

use std::path::PathBuf;
use std::sync::OnceLock;

use cortex::rootfs_v2::RootFsV2;
use microsandbox_image::GlobalCache;

pub use build::pull::pull;
pub use digest::Digest;
pub use image::Image;
pub use layer::Layer;

/// Where the images live: content-addressed, shared by every session, and kept.
pub fn home() -> PathBuf {
    crate::home().join("rootfs")
}

/// The store under [`home`], as the crate that writes into it sees it.
///
/// Opened once and held, because opening it makes the directories and a second caller has
/// nothing to say about a failure the first would already have died of — the same reason
/// [`home`] resolves the way it does.
pub fn cache() -> &'static GlobalCache {
    static CACHE: OnceLock<GlobalCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        GlobalCache::new(&home()).unwrap_or_else(|e| panic!("opening {}: {e}", home().display()))
    })
}

/// Where an image's manifest is kept, named by its digest.
pub fn images() -> PathBuf {
    home().join("images")
}

/// Dispatch `image <command>`, where `argv` is everything after `image`.
pub async fn run(argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    let mut argv = argv.into_iter();
    let command = argv.next();
    match command.as_ref().map(|c| c.as_ref()) {
        // `-r` carries a declaration written as JSON, `-d` carries the Dockerfile one would
        // be read out of. Both the thing itself rather than a path to it: this is a process
        // a client spawns, and what such a client has in hand is a value it built or a
        // document it was handed, which need not be a file anywhere this process can reach.
        Some("build") => {
            let mut recipe: Option<String> = None;
            let mut dockerfile: Option<String> = None;

            while let Some(argument) = argv.next() {
                match argument.as_ref() {
                    "-r" | "--recipe" => {
                        recipe = Some(
                            argv.next()
                                .map(|value| value.as_ref().to_owned())
                                .ok_or_else(|| anyhow::anyhow!("`-r` takes a value"))?,
                        )
                    }
                    "-d" | "--dockerfile" => {
                        dockerfile = Some(
                            argv.next()
                                .map(|value| value.as_ref().to_owned())
                                .ok_or_else(|| anyhow::anyhow!("`-d` takes a value"))?,
                        )
                    }
                    other => anyhow::bail!("{other:?} is not an argument `image build` takes"),
                }
            }

            let rootfs = match (recipe, dockerfile) {
                // Naming both is refused rather than resolved by an order nobody stated.
                (Some(_), Some(_)) => {
                    anyhow::bail!("both a recipe and a dockerfile were given; name one")
                }
                (None, None) => {
                    anyhow::bail!("neither a recipe nor a dockerfile was given")
                }
                (Some(json), None) => serde_json::from_str::<RootFsV2>(&json)
                    .map_err(|e| anyhow::anyhow!("reading the recipe: {e}"))?,
                // A Dockerfile that says something a declaration has no form for is refused
                // at the line that said it, by the reader — an image with a `CMD` or a
                // `USER` in it is not the image this would build, and the value handed back
                // is the whole of the answer, with no second channel for what was passed
                // over.
                (None, Some(text)) => RootFsV2::from_dockerfile(text)?,
            };

            build::build(rootfs).await?;
            Ok(())
        }
        Some("list") => list(argv).await,
        Some("remove") => remove(argv).await,
        // No command a bare `image` should mean: the three differ in what they do to what is
        // on disk, and guessing between them is not a kindness.
        None => anyhow::bail!("`image` takes `build`, `list` or `remove`"),
        Some(other) => anyhow::bail!("{other:?} is not `build`, `list` or `remove`"),
    }
}

/// Write out the images that are here.
async fn list(_argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    todo!("list")
}

/// Delete an image.
async fn remove(_argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    todo!("remove")
}
