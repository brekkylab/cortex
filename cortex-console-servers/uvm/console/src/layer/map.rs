//! Keeping an [`ErofsDataMap`] next to the layer it describes.
//!
//! # Why it has to be kept at all
//!
//! `write_erofs` hands the map back as it writes, and there is no way to ask for it again:
//! `ErofsReader` does not expose where a file's data begins, and `microsandbox-image`'s own
//! answer to having lost one is to re-pull the image. Stitching needs the map of every layer
//! it stitches, so a layer kept without its map is a layer nothing can use.
//!
//! # Why a mirror of the type
//!
//! [`ErofsDataMap`] belongs to another crate and derives no `Serialize`. [`Stored`] is the
//! same thing in a shape that does, exists for no other purpose, and is converted at the two
//! functions below.
//!
//! Paths travel as **bytes**. A path in a layer is whatever the filesystem it came from
//! allowed, which is not necessarily UTF-8, and a sidecar that could not hold one would be a
//! layer that could not be stored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use microsandbox_image::erofs::ErofsDataMap;
use serde::{Deserialize, Serialize};

/// The stored form of a data map.
#[derive(Serialize, Deserialize)]
struct Stored {
    total_blocks: u32,
    files: Vec<StoredFile>,
}

/// Where one file's data is: the block it starts at, and how many bytes of it there are.
#[derive(Serialize, Deserialize)]
struct StoredFile {
    #[serde(with = "serde_bytes")]
    path: Vec<u8>,
    start_block: u32,
    size: u64,
}

/// Write `map` beside its layer.
pub fn write(map: &ErofsDataMap, path: &Path) -> anyhow::Result<()> {
    let mut files: Vec<StoredFile> = map
        .file_blocks
        .iter()
        .map(|(path, (start_block, size))| StoredFile {
            path: path.as_os_str().as_encoded_bytes().to_vec(),
            start_block: *start_block,
            size: *size,
        })
        .collect();
    // Sorted, so that one map is one sequence of bytes. A `HashMap` iterates in whatever
    // order it likes, and a sidecar that differed run to run would make two identical layers
    // look different to anything comparing them.
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let encoded = bson::serialize_to_vec(&Stored {
        total_blocks: map.total_blocks,
        files,
    })
    .map_err(|e| anyhow::anyhow!("encoding a layer's data map: {e}"))?;

    super::atomic::replace(path, &encoded)
}

/// Read one back.
pub fn read(path: &Path) -> anyhow::Result<ErofsDataMap> {
    let encoded =
        std::fs::read(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let stored: Stored = bson::deserialize_from_slice(&encoded)
        .map_err(|e| anyhow::anyhow!("decoding {}: {e}", path.display()))?;

    let mut file_blocks = HashMap::with_capacity(stored.files.len());
    for file in stored.files {
        // SAFETY: these bytes came from `OsStr::as_encoded_bytes` in `write`, which is the
        // documented precondition — the encoding is self-consistent and unchanged in between.
        let path = PathBuf::from(unsafe {
            std::ffi::OsString::from_encoded_bytes_unchecked(file.path)
        });
        file_blocks.insert(path, (file.start_block, file.size));
    }
    Ok(ErofsDataMap {
        file_blocks,
        total_blocks: stored.total_blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layer.map");

        let mut map = ErofsDataMap {
            file_blocks: HashMap::new(),
            total_blocks: 41,
        };
        map.file_blocks.insert(PathBuf::from("bin/sh"), (2, 1024));
        map.file_blocks
            .insert(PathBuf::from("etc/os-release"), (3, 17));

        write(&map, &path).unwrap();
        let back = read(&path).unwrap();

        assert_eq!(back.total_blocks, map.total_blocks);
        assert_eq!(back.file_blocks, map.file_blocks);
    }

    #[test]
    fn a_map_of_nothing_still_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.map");
        let map = ErofsDataMap {
            file_blocks: HashMap::new(),
            total_blocks: 1,
        };
        write(&map, &path).unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.total_blocks, 1);
        assert!(back.file_blocks.is_empty());
    }

    /// A path that is not UTF-8 is the case the byte encoding exists for.
    #[test]
    fn a_path_that_is_not_text_survives() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("odd.map");
        let odd = PathBuf::from(OsStr::from_bytes(b"bin/\xff\xfenot-utf8"));

        let mut map = ErofsDataMap {
            file_blocks: HashMap::new(),
            total_blocks: 3,
        };
        map.file_blocks.insert(odd.clone(), (1, 9));

        write(&map, &path).unwrap();
        assert_eq!(read(&path).unwrap().file_blocks.get(&odd), Some(&(1, 9)));
    }

    #[test]
    fn the_same_map_is_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = ErofsDataMap {
            file_blocks: HashMap::new(),
            total_blocks: 9,
        };
        for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            map.file_blocks.insert(PathBuf::from(name), (1, 1));
        }

        let first = dir.path().join("first.map");
        let second = dir.path().join("second.map");
        write(&map, &first).unwrap();
        write(&map, &second).unwrap();
        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap(),
            "the encoding depends on hash order"
        );
    }

    #[test]
    fn a_file_that_is_not_a_map_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.map");
        std::fs::write(&path, b"not bson").unwrap();
        assert!(read(&path).is_err());
    }
}
