use std::collections::HashMap;
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;

use microsandbox_image::erofs::{ErofsDataMap, ErofsEntryKind, ErofsReader};
use microsandbox_image::tree::{
    DeviceNode, DirectoryNode, FileData, FileTree, InodeMetadata, RegularFileId, RegularFileNode,
    SymlinkNode, TreeNode,
};
use serde::{Deserialize, Serialize};

use super::digest::Digest;
use super::home;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Layer(Digest);

impl Layer {
    pub fn new(digest: Digest) -> Layer {
        Layer(digest)
    }

    pub fn path(&self) -> PathBuf {
        home()
            .join("blobs")
            .join(format!("{}.erofs", self.0.file_stem()))
    }

    // What an EROFS written here holds, read back out of the finished file.
    //
    // `write_fsmeta` wants a tree and, for every regular file, where its data starts inside
    // the layer. The crate that wrote it keeps that second half only while writing, so this
    // takes the tree from the public walk and reads the block address out of the inode
    // itself — which is possible because regular files are emitted as `FLAT_PLAIN`, whose
    // inode records the address rather than computing it.
    //
    // File contents are deliberately left empty: fsmeta takes every size and location from
    // the map, and reading the bytes of a layer to stitch it would be the one cost this
    // whole arrangement exists to avoid.
    pub fn read(&self) -> anyhow::Result<(FileTree, ErofsDataMap)> {
        let path = self.path();
        let file = std::fs::File::open(&path)
            .map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
        let total_blocks = u32::try_from(file.metadata()?.len().div_ceil(BLOCK))
            .map_err(|_| anyhow::anyhow!("{} is too large to be a layer", path.display()))?;

        let mut superblock = [0u8; 128];
        file.read_exact_at(&mut superblock, SUPERBLOCK)?;
        let magic = u32::from_le_bytes(superblock[0..4].try_into()?);
        anyhow::ensure!(
            magic == MAGIC,
            "{} does not begin like an EROFS image",
            path.display()
        );
        let root = u16::from_le_bytes(superblock[0x0E..0x10].try_into()?) as u32;
        let inodes = u32::from_le_bytes(superblock[0x28..0x2C].try_into()?);

        // One inode, as the format lays it out. The extended form is the longer of the two
        // and the compact one is its prefix, so reading the longer and taking what is
        // common to both is safe for the fields this needs.
        let inode = |nid: u32| -> anyhow::Result<(InodeMetadata, u8, u32, u64)> {
            let mut raw = [0u8; 64];
            file.read_exact_at(&mut raw, u64::from(inodes) * BLOCK + u64::from(nid) * 32)?;
            let format = u16::from_le_bytes(raw[0..2].try_into()?);
            Ok((
                InodeMetadata {
                    uid: u32::from_le_bytes(raw[24..28].try_into()?),
                    gid: u32::from_le_bytes(raw[28..32].try_into()?),
                    mode: u16::from_le_bytes(raw[4..6].try_into()?),
                    mtime: u64::from_le_bytes(raw[32..40].try_into()?),
                    mtime_nsec: u32::from_le_bytes(raw[40..44].try_into()?),
                },
                ((format >> 1) & 0x07) as u8,
                u32::from_le_bytes(raw[16..20].try_into()?),
                u64::from_le_bytes(raw[8..16].try_into()?),
            ))
        };

        let (metadata, ..) = inode(root)?;
        let mut tree = FileTree {
            root: DirectoryNode::new(metadata),
        };
        let mut file_blocks = HashMap::new();
        let mut hardlinks: HashMap<u32, RegularFileId> = HashMap::new();

        let mut reader = ErofsReader::new(std::fs::File::open(&path)?)?;
        for entry in reader.walk()? {
            let bytes = entry.path.as_os_str().as_encoded_bytes().to_vec();
            if bytes.is_empty() {
                continue;
            }

            let node = match entry.kind {
                ErofsEntryKind::RegularFile => {
                    let (_, layout, start, size) = inode(entry.nid)?;
                    anyhow::ensure!(
                        layout == FLAT_PLAIN || size == 0,
                        "{} is not laid out flat, and its data cannot be addressed",
                        entry.path.display()
                    );
                    file_blocks.insert(entry.path.clone(), (start, size));
                    TreeNode::RegularFile(RegularFileNode {
                        id: *hardlinks
                            .entry(entry.nid)
                            .or_insert_with(RegularFileId::new),
                        metadata: entry.metadata,
                        xattrs: entry.xattrs,
                        data: FileData::Memory(Vec::new()),
                        nlink: 1,
                    })
                }
                ErofsEntryKind::Directory => {
                    let mut directory = DirectoryNode::new(entry.metadata);
                    directory.xattrs = entry.xattrs;
                    TreeNode::Directory(directory)
                }
                ErofsEntryKind::Symlink => TreeNode::Symlink(SymlinkNode {
                    metadata: entry.metadata,
                    target: reader.read_link_by_nid(entry.nid)?,
                }),
                ErofsEntryKind::CharDevice | ErofsEntryKind::BlockDevice => {
                    let (major, minor) = entry.rdev.unwrap_or((0, 0));
                    let device = DeviceNode {
                        metadata: entry.metadata,
                        major,
                        minor,
                    };
                    match entry.kind {
                        ErofsEntryKind::CharDevice => TreeNode::CharDevice(device),
                        _ => TreeNode::BlockDevice(device),
                    }
                }
                ErofsEntryKind::Fifo => TreeNode::Fifo(entry.metadata),
                ErofsEntryKind::Socket => TreeNode::Socket(entry.metadata),
            };

            tree.insert(&bytes, node)
                .map_err(|e| anyhow::anyhow!("adding {}: {e:?}", entry.path.display()))?;
        }

        Ok((
            tree,
            ErofsDataMap {
                file_blocks,
                total_blocks,
            },
        ))
    }
}

// The format, as the kernel lays it out.
const SUPERBLOCK: u64 = 1024;
const MAGIC: u32 = 0xE0F5_E1E2;
const BLOCK: u64 = 4096;
const FLAT_PLAIN: u8 = 0;
