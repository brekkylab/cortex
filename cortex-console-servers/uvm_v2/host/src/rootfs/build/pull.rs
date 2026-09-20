use crate::rootfs::{Digest, Image, Layer, cache};
use microsandbox_image::{Platform, PullOptions, Reference, Registry, RootfsMaterialization};

/// Resolve `image` against a registry, and say what it is once it is here.
///
/// Nothing is copied: what the pull writes under `layers/` is what the image names, and where
/// it wrote it is where a boot reads it from. A second pull of the same reference is answered
/// out of `manifests/` without the registry being reached at all, which is the store doing
/// the remembering rather than a table kept beside it.
pub async fn pull(image: impl Into<String>) -> anyhow::Result<Image> {
    let image = image.into();
    let reference: Reference = image
        .parse()
        .map_err(|e| anyhow::anyhow!("{image} is not an OCI reference: {e}"))?;

    // Resolved, downloaded and turned into one EROFS per layer, for linux and this host's
    // architecture whatever the host itself runs.
    let platform = Platform::host_linux();
    let cache = cache();
    let pulled = Registry::builder(platform.clone(), cache.clone())
        .build()
        .map_err(|e| anyhow::anyhow!("building a registry client: {e}"))?
        .pull(
            &reference,
            &PullOptions {
                materialization: RootfsMaterialization::Layered,
                ..PullOptions::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("pulling {reference}: {e}"))?;

    // Said here rather than left to the boot: a layer the pull reported and did not write is
    // a fault in the store, and the reference that was asked for is the last point at which
    // anything can say which store and which reference.
    for diff_id in &pulled.layer_diff_ids {
        anyhow::ensure!(
            cache.is_layer_materialized(diff_id),
            "the pull left no layer at {}",
            cache.layer_erofs_path(diff_id).display()
        );
    }

    // The image itself: its layers, and what it states.
    let image = Image {
        v: 1,
        os: platform.os.to_string(),
        arch: platform.arch.to_string(),
        layers: pulled
            .layer_diff_ids
            .iter()
            .map(|diff_id| Layer::new(Digest::from(diff_id)))
            .collect(),
        env: pulled.config.env,
        workdir: pulled.config.working_dir,
        user: pulled.config.user,
        from: Some(format!("{reference}@{}", pulled.manifest_digest)),
    };
    image.store()?;

    Ok(image)
}
