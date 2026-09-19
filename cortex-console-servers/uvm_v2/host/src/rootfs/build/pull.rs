use crate::rootfs::{Digest, Image, Layer, home};
use microsandbox_image::{
    GlobalCache, Platform, PullOptions, Reference, Registry, RootfsMaterialization,
};

pub async fn pull(image: impl Into<String>) -> anyhow::Result<Image> {
    let image = image.into();
    let key = Digest::of(format!("pull\0{image}").as_bytes());
    // Answered before?
    let keys = home().join("cache_key");
    if let Ok(text) = std::fs::read_to_string(&keys) {
        for line in text.lines() {
            if let Some((held, digest)) = line.split_once(' ')
                && held == key.as_str()
            {
                let held = home()
                    .join("blobs")
                    .join(format!("{}.json", &digest["sha256:".len()..]));
                return Ok(serde_json::from_slice(&std::fs::read(&held).map_err(
                    |e| anyhow::anyhow!("reading {}: {e}", held.display()),
                )?)?);
            }
        }
    }

    let reference: Reference = image
        .parse()
        .map_err(|e| anyhow::anyhow!("{image} is not an OCI reference: {e}"))?;

    let root = home();
    let blobs = root.join("blobs");
    let scratch = root.join("tmp");
    std::fs::create_dir_all(&blobs)?;
    std::fs::create_dir_all(&scratch)?;

    // Resolved, downloaded and turned into one EROFS per layer, for linux and this host's
    // architecture whatever the host itself runs.
    let platform = Platform::host_linux();
    let cache = GlobalCache::new(&root.join("oci"))
        .map_err(|e| anyhow::anyhow!("opening the image cache: {e}"))?;
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

    // Linked rather than copied: the bytes are the same bytes, and a link outlives whatever
    // the cache above decides to do with its own copy.
    let mut layers = Vec::with_capacity(pulled.layer_diff_ids.len());
    for diff_id in &pulled.layer_diff_ids {
        let made = cache.layer_erofs_path(diff_id);
        anyhow::ensure!(
            made.is_file(),
            "the pull left no layer at {}",
            made.display()
        );

        let digest = Digest::of_file(&made)?;
        let held = blobs.join(format!("{}.erofs", digest.file_stem()));
        if !held.is_file() {
            let part = scratch.join(format!("{}.erofs", std::process::id()));
            let _ = std::fs::remove_file(&part);
            if std::fs::hard_link(&made, &part).is_err() {
                std::fs::copy(&made, &part)?;
            }
            std::fs::rename(&part, &held)?;
        }
        layers.push(Layer::new(digest));
    }

    // The image itself: its layers, and what it states.
    let image = Image {
        v: 1,
        os: platform.os.to_string(),
        arch: platform.arch.to_string(),
        layers,
        env: pulled.config.env,
        workdir: pulled.config.working_dir,
        user: pulled.config.user,
        from: Some(format!("{reference}@{}", pulled.manifest_digest)),
    };

    let digest = image.digest()?;
    let part = scratch.join(format!("{}.json", std::process::id()));
    std::fs::write(&part, &image.bytes()?)?;
    std::fs::rename(&part, blobs.join(format!("{}.json", digest.file_stem())))?;

    // So the next pull stops at the read above.
    let mut lines: Vec<String> = std::fs::read_to_string(&keys)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with(key.as_str()))
        .map(str::to_string)
        .collect();
    lines.push(format!("{key} {digest}"));
    lines.sort();
    let part = scratch.join(format!("{}.keys", std::process::id()));
    std::fs::write(&part, lines.join("\n") + "\n")?;
    std::fs::rename(&part, &keys)?;

    Ok(image)
}
