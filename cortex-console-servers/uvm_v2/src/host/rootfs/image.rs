use std::path::{Path, PathBuf};

use microsandbox_image::erofs::write_fsmeta;
use microsandbox_image::tree::merge_layers_with_provenance;
use olpc_cjson::CanonicalFormatter;
use serde::{Deserialize, Serialize};

use super::digest::Digest;
use super::layer::Layer;

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

    pub fn current_layer(&self) -> Option<&Layer> {
        self.layers.last()
    }

    pub fn current_layer_digest(&self) -> Option<&Digest> {
        self.current_layer().map(Layer::digest)
    }

    // This image as something a machine can attach: `<into>.fsmeta.erofs`, the merged
    // metadata, and `<into>.vmdk`, the descriptor naming it and every layer behind it.
    //
    // The layers stay where they are. A disk is a way of reading them, not a copy of them,
    // which is what makes two images on one base cost one copy of it — and why nothing here
    // reads a layer's file data.
    pub fn disk(&self, into: &Path) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(!self.layers.is_empty(), "a disk needs at least one layer");

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

        let fsmeta = into.with_extension("fsmeta.erofs");
        if let Some(parent) = fsmeta.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_fsmeta(&merged, &provenance, &maps, &fsmeta)
            .map_err(|e| anyhow::anyhow!("writing the merged metadata: {e:?}"))?;

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
                anyhow::anyhow!("{} is not UTF-8, which a descriptor cannot spell", at.display())
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

        let disk = into.with_extension("vmdk");
        std::fs::write(&disk, text)?;
        Ok(disk)
    }
}

// A VMDK extent line addresses sectors, and at most 2 GiB of them. The geometry is stated
// because every descriptor states it, and read by nothing.
const SECTOR: u64 = 512;
const MAX_EXTENT_SECTORS: u64 = 4_194_304;
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;
