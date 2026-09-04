//! A base image as the layers it is made of.
//!
//! A commit is a layer stitched onto the ones its base already has, so a base that is not
//! layers is a base nothing can be committed onto. Everything here turns each of the three
//! kinds into the same thing.
//!
//! | what a session named | what the base is |
//! |---|---|
//! | nothing | the pinned rootfs tarball, as one layer |
//! | `cortex.local/built@…` | an image committed here, as the layers its manifest lists |
//! | anything else | an OCI reference, pulled and seeded as one layer |
//!
//! # Why a pulled image is seeded rather than used where it lies
//!
//! `microsandbox-image` writes each pulled layer as its own EROFS and stitches them into a
//! disk of its own — but the [`ErofsDataMap`] needed to stitch anything *else* onto them
//! exists only during the pull and is not kept, and `ErofsReader` does not expose where a
//! file's data begins, so it cannot be recovered. The crate's own answer to having lost one
//! is to re-pull with `force`.
//!
//! So the pulled layers are read once, merged, and written as a single layer of ours whose
//! map we keep. It costs one copy of the base, once, and every image built on it afterwards
//! shares that copy.
//!
//! [`ErofsDataMap`]: microsandbox_image::erofs::ErofsDataMap

use std::path::{Path, PathBuf};

use cortex_uvm_console::built::{self, BuiltStore, LOCAL_HOST};
use cortex_uvm_console::layer::{Layer, LayerId, LayerStore, stitch, tree};

use crate::assets;
use crate::contract::{BaseFormat, ImageSpec};

/// A read-only base image, and the two things about it that are not the path.
pub struct BaseImage {
    pub path: PathBuf,
    pub format: BaseFormat,
    pub spec: ImageSpec,
}

/// The layer store every base is kept in.
pub fn store() -> anyhow::Result<LayerStore> {
    LayerStore::open(&assets::home()?.join("layers"))
}

/// Where images made here live.
pub fn built_store() -> anyhow::Result<BuiltStore> {
    BuiltStore::open(&assets::home()?.join("built"))
}

/// The disk a boot attaches for `reference`, and the ids of the layers behind it.
///
/// One layer is attached as it stands — a base carries no whiteouts, so its layer is a valid
/// image on its own, and the common boot therefore costs no stitch. Several are stitched,
/// which is what a committed image is — except that a committed image was stitched once when
/// it was committed, and [`Resolved::disk`] hands that one back rather than making it again.
///
/// The layer **ids** come back with it because the session needs them: a `commit` stitches
/// onto the layers its base was made of, and going back for them would mean pulling a second
/// time. Ids and not layers, because that is all a commit reads — a `Layer` carries its whole
/// data map, and a session has no reason to hold one of those per layer until it quits.
pub async fn image(reference: Option<&str>) -> anyhow::Result<(BaseImage, Vec<LayerId>)> {
    let resolved = resolve(reference).await?;
    let ids = resolved
        .layers
        .iter()
        .map(|layer| layer.id.clone())
        .collect();

    let base = match (&resolved.disk, resolved.layers.as_slice()) {
        (_, []) => anyhow::bail!("a base with no layers"),
        // Already stitched, and named for the image rather than for the layer set. Booting it
        // is the whole reason a commit wrote it.
        (Some(disk), _) => BaseImage {
            path: disk.clone(),
            format: BaseFormat::Vmdk,
            spec: resolved.spec,
        },
        (None, [only]) => BaseImage {
            path: only.erofs.clone(),
            format: BaseFormat::Raw,
            spec: resolved.spec,
        },
        (None, many) => {
            let set = LayerId::of(
                many.iter()
                    .map(|layer| layer.id.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_bytes(),
            );
            let into = assets::home()?.join("stitched");
            let path = stitch(many, &into.join(set.file_stem()))?;
            reclaim(&into, set.file_stem());
            BaseImage {
                path,
                format: BaseFormat::Vmdk,
                spec: resolved.spec,
            }
        }
    };
    Ok((base, ids))
}

/// What a reference turned out to name.
struct Resolved {
    layers: Vec<Layer>,
    spec: ImageSpec,
    /// The disk to attach, when one already exists. Only a committed image has one: it was
    /// stitched when it was committed, and stitching the same layers again would be the same
    /// merged metadata written twice under two names.
    disk: Option<PathBuf>,
}

/// The layers `reference` names and what it states, resolved once.
///
/// One function because a pulled image gives both at the same moment, and asking for them
/// separately would mean contacting a registry twice for one session.
async fn resolve(reference: Option<&str>) -> anyhow::Result<Resolved> {
    let store = store()?;
    match reference {
        None => Ok(Resolved {
            layers: vec![pinned(&store).await?],
            spec: ImageSpec::default(),
            disk: None,
        }),
        Some(reference) => match reference.strip_prefix(LOCAL_HOST) {
            Some(rest) => {
                let id = built::digest_of(rest)?;
                let built = built_store()?;
                let manifest = built.manifest(&id)?;
                Ok(Resolved {
                    layers: built.layers(&id, &store)?,
                    spec: ImageSpec {
                        env: manifest.env,
                        working_dir: manifest.working_dir,
                    },
                    disk: Some(built.disk(&id)),
                })
            }
            None => {
                let pulled = assets::pull(reference).await?;
                let spec = ImageSpec {
                    env: pulled.config.env.clone(),
                    working_dir: pulled.config.working_dir.clone(),
                };
                Ok(Resolved {
                    layers: vec![seeded(&pulled, &store)?],
                    spec,
                    disk: None,
                })
            }
        },
    }
}

/// How long a stitched base nobody has asked for again is kept. As `abin`'s, and for the same
/// reason: a stitch is derived, and a running guest already holds its disk open.
const KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Drop the stitched bases in `dir` that nothing has asked for in a day, except `keep`.
///
/// Every distinct layer set makes one, so without this the directory grows by a descriptor
/// and a metadata EROFS per commit and never shrinks. Best effort: a directory that could not
/// be tidied is still a directory that works.
fn reclaim(dir: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name.ends_with(".vmdk") || name.ends_with(".fsmeta.erofs");
        if !ours || name.starts_with(keep) {
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

/// The pinned rootfs, as one layer.
///
/// Named by the digest of its tarball, so the pin and the layer are one fact said twice:
/// changing the pin is a different layer rather than a stale one, and no cache has to be
/// invalidated by hand.
///
/// This is where `images/` went. It used to hold one EROFS per pinned rootfs, encoded from
/// the same tree by the same call — the same bytes under a different name. Keeping both cost
/// an encode and a copy for nothing.
async fn pinned(store: &LayerStore) -> anyhow::Result<Layer> {
    let rootfs = assets::Rootfs::host();
    let tarball = rootfs.fetch().await?;
    let id = LayerId::of(&std::fs::read(&tarball)?);
    if store.has(&id) {
        return store.get(&id);
    }

    let tree = assets::ingest(&tarball).await?;
    store.put(&id, &tree)
}

/// A pulled image, merged into one layer of ours.
///
/// Named by the manifest digest, which is what identifies the image the registry answered
/// with — so two sessions on the same reference seed once, and a tag that moved is a
/// different layer.
fn seeded(pulled: &assets::Pulled, store: &LayerStore) -> anyhow::Result<Layer> {
    let id = LayerId::parse(&pulled.manifest_digest.to_string())?;
    if store.has(&id) {
        return store.get(&id);
    }

    eprintln!(
        "cortex-uvm-console: seeding {} layers into the layer store",
        pulled.layers.len()
    );
    let mut trees = Vec::with_capacity(pulled.layers.len());
    for path in &pulled.layers {
        // Contents and all: this tree is about to be written somewhere else, which is the one
        // case that needs them.
        trees.push(tree::from_erofs(path, tree::Contents::Read)?);
    }
    let (merged, _) = microsandbox_image::tree::merge_layers_with_provenance(trees);
    store.put(&id, &merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// The default base is one layer, and the same layer every time — which is what lets a
    /// commit onto it be a second layer rather than a copy of it.
    #[test]
    #[ignore = "downloads and encodes the pinned rootfs"]
    fn the_default_base_is_one_layer() {
        let runtime = runtime();
        let (_, first) = runtime.block_on(image(None)).unwrap();
        assert_eq!(first.len(), 1);
        let (_, again) = runtime.block_on(image(None)).unwrap();
        assert_eq!(again[0], first[0], "the same rootfs named two layers");
    }

    /// A base with no image named attaches its single layer as it stands, which is what makes
    /// the common boot cost nothing extra.
    #[test]
    #[ignore = "downloads and encodes the pinned rootfs"]
    fn the_default_base_is_attached_raw() {
        let (base, layers) = runtime().block_on(image(None)).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(base.format, BaseFormat::Raw);
        assert_eq!(base.path.extension().unwrap(), "erofs");
        assert_eq!(
            base.path.file_stem().unwrap(),
            layers[0].file_stem(),
            "the layer is the disk"
        );
    }
}
