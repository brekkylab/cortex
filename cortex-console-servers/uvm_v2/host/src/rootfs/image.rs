use std::path::{Path, PathBuf};

use microsandbox_image::erofs::write_fsmeta;
use microsandbox_image::tree::merge_layers_with_provenance;
use olpc_cjson::CanonicalFormatter;
use serde::{Deserialize, Serialize};

use super::digest::Digest;
use super::layer::Layer;
use super::{cache, images};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub v: u32,
    pub os: String,
    pub arch: String,
    pub layers: Vec<Layer>,
    pub env: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub from: Option<String>,
}

impl Image {
    // Canonical, so that the same image is the same bytes whatever order the fields are
    // declared in. Written and hashed through here and nowhere else, or the two would drift.
    pub fn bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.serialize(&mut serde_json::Serializer::with_formatter(
            &mut bytes,
            CanonicalFormatter::new(),
        ))?;
        Ok(bytes)
    }

    pub fn digest(&self) -> anyhow::Result<Digest> {
        Ok(Digest::of(&self.bytes()?))
    }

    /// Write this image out under its digest, and say what that digest is.
    ///
    /// Written whether or not it is already there: the bytes are the same bytes either way,
    /// and a rename over a file that holds them costs less than the test that would skip it.
    pub fn store(&self) -> anyhow::Result<Digest> {
        let bytes = self.bytes()?;
        let digest = Digest::of(&bytes);

        let images = images();
        std::fs::create_dir_all(&images)?;
        let part = cache()
            .tmp_dir()
            .join(format!("{}.image.json", std::process::id()));
        std::fs::write(&part, &bytes)?;
        std::fs::rename(&part, Image::path(&digest))?;

        Ok(digest)
    }

    /// The image `digest` names, read back.
    pub fn load(digest: &Digest) -> anyhow::Result<Image> {
        Image::read(&Image::path(digest))
    }

    /// One manifest, read from where it lies.
    ///
    /// Beside [`load`](Self::load) because a listing walks the directory and so has the file
    /// before it has a digest — and naming the file it failed on is the whole of what it can
    /// say about one that does not parse.
    pub fn read(held: &Path) -> anyhow::Result<Image> {
        let bytes =
            std::fs::read(held).map_err(|e| anyhow::anyhow!("reading {}: {e}", held.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", held.display()))
    }

    /// Where the manifest `digest` names lies, whether or not anything is there.
    pub fn path(digest: &Digest) -> PathBuf {
        images().join(format!("{}.json", digest.path_safe()))
    }

    /// The digest of the stack this image is: its layers, in order, and nothing else.
    ///
    /// What a disk is made of is the layers, so this and not [`Image::digest`] is what names
    /// one — two images that differ in an `ENV` are one disk, and the second of them boots on
    /// what the first already paid for.
    pub(super) fn stack(&self) -> Digest {
        let mut named = String::new();
        for layer in &self.layers {
            named.push_str(layer.digest().as_str());
            named.push('\n');
        }
        Digest::of(named.as_bytes())
    }

    // This image as something a machine can attach: the merged metadata as one EROFS under
    // `fsmeta/`, and under `vmdk/` the descriptor naming it and every layer behind it.
    //
    // The layers stay where they are. A disk is a way of reading them, not a copy of them,
    // which is what makes two images on one base cost one copy of it — and why nothing here
    // reads a layer's file data.
    //
    // Named by the stack rather than by the image, and kept, so a second boot on those layers
    // finds it made. A pull writes a pair of its own under the manifest digest; this does not
    // reach for it, because an image with a step over it needs a pair no registry can have
    // written, and one rule that covers both is worth more than the one merge it saves.
    pub fn disk(&self) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(!self.layers.is_empty(), "a disk needs at least one layer");

        let cache = cache();
        let stack = self.stack().oci();
        let fsmeta = cache.fsmeta_erofs_path(&stack);
        let disk = cache.vmdk_path(&stack);
        if fsmeta.is_file() && disk.is_file() {
            return Ok(disk);
        }

        let mut trees = Vec::with_capacity(self.layers.len());
        let mut maps = Vec::with_capacity(self.layers.len());
        for layer in &self.layers {
            let (tree, map) = layer.read()?;
            trees.push(tree);
            maps.push(map);
        }

        // Later layers win, a character device `0:0` deletes what it stands over, and a
        // directory carrying `trusted.overlay.opaque` hides what is under it. What comes
        // back beside the merged tree is which layer each path came from, which is how an
        // inode is pointed at the right extent below.
        let (merged, provenance) = merge_layers_with_provenance(trees);

        // Into place before the descriptor is written, because what the descriptor names is
        // where the metadata ended up and not where it was made.
        let part = cache
            .tmp_dir()
            .join(format!("{}.fsmeta.erofs", std::process::id()));
        write_fsmeta(&merged, &provenance, &maps, &part)
            .map_err(|e| anyhow::anyhow!("writing the merged metadata: {e:?}"))?;
        std::fs::rename(&part, &fsmeta)?;

        // The order is the whole scheme: the metadata first, then the layers it points into,
        // bottom first. An inode names its layer by the index it has here.
        let mut extents = vec![fsmeta.clone()];
        extents.extend(self.layers.iter().map(|layer| layer.path()));

        let mut sectors = 0u64;
        let mut lines = Vec::new();
        for extent in &extents {
            let size = std::fs::metadata(extent)
                .map_err(|e| anyhow::anyhow!("stat {}: {e}", extent.display()))?
                .len();
            anyhow::ensure!(
                size.is_multiple_of(SECTOR),
                "{} is {size} bytes, which is not a whole number of {SECTOR}-byte sectors",
                extent.display()
            );

            // Absolute, because a descriptor is read from wherever the machine happens to
            // stand.
            let at = std::fs::canonicalize(extent)
                .map_err(|e| anyhow::anyhow!("resolving {}: {e}", extent.display()))?;
            // Not `display`, which replaces what is not UTF-8 and hands back a name that
            // opens some other file or none.
            let at = at.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is not UTF-8, which a descriptor cannot spell",
                    at.display()
                )
            })?;
            // The format puts one extent on one line and quotes the path, and offers no
            // escape for either delimiter: a quote ends the name early and a newline starts
            // an extent nobody asked for.
            anyhow::ensure!(
                !at.contains(['"', '\n', '\r']),
                "{at} has a quote or a line break in its name, which a descriptor cannot spell"
            );

            let mut left = size / SECTOR;
            let mut offset = 0u64;
            while left > 0 {
                let chunk = left.min(MAX_EXTENT_SECTORS);
                lines.push(format!("RW {chunk} FLAT \"{at}\" {offset}"));
                offset += chunk;
                left -= chunk;
            }
            sectors += size / SECTOR;
        }

        let mut text = String::from(
            "# Disk DescriptorFile\nversion=1\nCID=fffffffe\nparentCID=ffffffff\n\
             createType=\"twoGbMaxExtentFlat\"\n\n# Extent description\n",
        );
        for line in &lines {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str("\n# The Disk Data Base\n#DDB\nddb.virtualHWVersion = \"4\"\n");
        text.push_str(&format!(
            "ddb.geometry.cylinders = \"{}\"\n",
            sectors.div_ceil(HEADS * SECTORS_PER_TRACK)
        ));
        text.push_str(&format!("ddb.geometry.heads = \"{HEADS}\"\n"));
        text.push_str(&format!("ddb.geometry.sectors = \"{SECTORS_PER_TRACK}\"\n"));
        text.push_str("ddb.adapterType = \"ide\"\n");

        let part = cache.tmp_dir().join(format!("{}.vmdk", std::process::id()));
        std::fs::write(&part, text)?;
        std::fs::rename(&part, &disk)?;
        Ok(disk)
    }
}

// A VMDK extent line addresses sectors, and at most 2 GiB of them. The geometry is stated
// because every descriptor states it, and read by nothing.
const SECTOR: u64 = 512;
const MAX_EXTENT_SECTORS: u64 = 4_194_304;
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;
