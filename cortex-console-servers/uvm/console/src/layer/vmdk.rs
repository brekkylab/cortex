//! A VMDK flat descriptor: several files, read as one disk.
//!
//! Cortex writes this itself because `microsandbox_image::stitch` is `pub(crate)`. That is
//! not reverse engineering — a flat descriptor is a documented plain-text format, and this
//! is forty lines of it.
//!
//! What it buys is the whole layering scheme. The merged metadata EROFS refers to its layers
//! by device index, and those indices are the order the extents appear in here: the fsmeta
//! first, then the layers it points into, bottom layer first.

use std::path::Path;

/// A VMDK extent line addresses at most 2 GiB, in 512-byte sectors.
const MAX_EXTENT_SECTORS: u64 = 4_194_304;

/// Geometry nothing reads and every descriptor states.
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;

/// Write a descriptor at `output` that reads `extents`, in order, as one disk.
///
/// Every extent has to be a whole number of 512-byte sectors, because a descriptor addresses
/// sectors and half of one has no spelling. An EROFS is a whole number of 4 KiB blocks, so
/// this only ever fires on something that is not one.
pub fn write_descriptor(output: &Path, extents: &[&Path]) -> anyhow::Result<()> {
    anyhow::ensure!(!extents.is_empty(), "a disk needs at least one extent");

    let mut total_sectors = 0u64;
    let mut lines = Vec::new();

    for path in extents {
        let size = std::fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("stat {}: {e}", path.display()))?
            .len();
        anyhow::ensure!(
            size.is_multiple_of(512),
            "{} is {size} bytes, which is not a whole number of 512-byte sectors",
            path.display()
        );

        // Absolute, because a descriptor is read from wherever the VMM happens to stand.
        let absolute = std::fs::canonicalize(path)
            .map_err(|e| anyhow::anyhow!("resolving {}: {e}", path.display()))?;
        let absolute = absolute.display().to_string();
        // The format quotes the path and offers no escape for a quote inside one. Refused
        // rather than written, because what would be written is a descriptor that parses
        // into some other file.
        anyhow::ensure!(
            !absolute.contains('"'),
            "{absolute} has a quote in its name, which a descriptor cannot spell"
        );

        let sectors = size / 512;
        let mut offset = 0u64;
        let mut remaining = sectors;
        while remaining > 0 {
            let chunk = remaining.min(MAX_EXTENT_SECTORS);
            lines.push(format!("RW {chunk} FLAT \"{absolute}\" {offset}"));
            offset += chunk;
            remaining -= chunk;
        }
        total_sectors += sectors;
    }

    let cylinders = total_sectors.div_ceil(HEADS * SECTORS_PER_TRACK);
    let mut text = String::new();
    text.push_str("# Disk DescriptorFile\n");
    text.push_str("version=1\n");
    text.push_str("CID=fffffffe\n");
    text.push_str("parentCID=ffffffff\n");
    text.push_str("createType=\"twoGbMaxExtentFlat\"\n\n");
    text.push_str("# Extent description\n");
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("\n# The Disk Data Base\n#DDB\n");
    text.push_str("ddb.virtualHWVersion = \"4\"\n");
    text.push_str(&format!("ddb.geometry.cylinders = \"{cylinders}\"\n"));
    text.push_str(&format!("ddb.geometry.heads = \"{HEADS}\"\n"));
    text.push_str(&format!("ddb.geometry.sectors = \"{SECTORS_PER_TRACK}\"\n"));
    text.push_str("ddb.adapterType = \"ide\"\n");

    super::atomic::replace(output, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn extent(dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![0u8; bytes]).unwrap();
        path
    }

    fn extent_lines(text: &str) -> Vec<&str> {
        text.lines()
            .filter(|line| line.starts_with("RW "))
            .collect()
    }

    #[test]
    fn a_descriptor_names_every_extent_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = extent(dir.path(), "a.bin", 4096);
        let b = extent(dir.path(), "b.bin", 8192);
        let out = dir.path().join("disk.vmdk");

        write_descriptor(&out, &[&a, &b]).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();

        assert!(text.contains("createType=\"twoGbMaxExtentFlat\""), "{text}");
        let lines = extent_lines(&text);
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("RW 8 FLAT "), "{}", lines[0]);
        assert!(lines[0].contains("a.bin"), "{}", lines[0]);
        assert!(lines[1].starts_with("RW 16 FLAT "), "{}", lines[1]);
        assert!(lines[1].contains("b.bin"), "{}", lines[1]);
        // 24 sectors over 16 heads * 63 sectors, rounded up.
        assert!(text.contains("ddb.geometry.cylinders = \"1\""), "{text}");
    }

    #[test]
    fn an_extent_that_is_not_sector_aligned_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ragged = extent(dir.path(), "ragged.bin", 1000);
        let out = dir.path().join("disk.vmdk");
        let err = write_descriptor(&out, &[&ragged]).unwrap_err().to_string();
        assert!(err.contains("512"), "{err}");
        assert!(!out.exists(), "a descriptor was written anyway");
    }

    #[test]
    fn an_extent_over_two_gibibytes_is_split() {
        // Sparse, so this costs no disk: only the length is read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(3 << 30).unwrap();
        drop(file);

        let out = dir.path().join("disk.vmdk");
        write_descriptor(&out, &[&path]).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();

        let lines = extent_lines(&text);
        assert_eq!(lines.len(), 2, "a 3 GiB extent should be two lines: {text}");
        assert!(lines[0].ends_with(" 0"), "{}", lines[0]);
        assert!(lines[0].starts_with("RW 4194304 FLAT "), "{}", lines[0]);
        assert!(lines[1].ends_with(" 4194304"), "{}", lines[1]);
        assert!(lines[1].starts_with("RW 2097152 FLAT "), "{}", lines[1]);
    }

    #[test]
    fn an_extent_is_named_absolutely() {
        let dir = tempfile::tempdir().unwrap();
        let a = extent(dir.path(), "a.bin", 512);
        let out = dir.path().join("disk.vmdk");

        write_descriptor(&out, &[&a]).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let quoted = text
            .lines()
            .find(|line| line.starts_with("RW "))
            .and_then(|line| line.split('"').nth(1))
            .expect("an extent line with a path");
        assert!(
            Path::new(quoted).is_absolute(),
            "the extent is named {quoted:?}, which a VMM standing elsewhere cannot open"
        );
    }

    #[test]
    fn a_disk_of_nothing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_descriptor(&dir.path().join("disk.vmdk"), &[]).is_err());
    }
}
