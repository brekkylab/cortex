//! Turning something else into a [`FileTree`]: an EROFS already on disk, or a directory.
//!
//! # Why the nid is the key
//!
//! `walk` reports an inode number and not a link count, so a recovery that mints a fresh
//! [`RegularFileId`] per path turns two names on one inode into two files — which
//! `write_erofs` then stores twice. Two names on one inode share their nid, so keying the
//! ids off it restores both the count and the sharing, and a tree written back out comes
//! out the size it went in.
//!
//! # Why reading the contents is a choice
//!
//! `write_fsmeta` takes each file's size and location from the provenance map and the layer
//! data maps, and never looks at a tree's file data. Stitching therefore wants
//! [`Contents::Skip`], which costs one pass over the metadata; seeding one layer out of
//! another wants [`Contents::Read`], which costs the whole image.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use microsandbox_image::erofs::{ErofsEntryKind, ErofsReader};
use microsandbox_image::tree::{
    DeviceNode, DirectoryNode, FileData, FileTree, InodeMetadata, RegularFileId, RegularFileNode,
    SymlinkNode, TreeNode,
};

/// Whether a recovered tree carries its file data, or only its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// Leave every regular file empty. Enough to stitch with.
    Skip,
    /// Read every regular file out of the image. Needed to write the tree somewhere else.
    Read,
}

/// Read `image` back as a tree.
pub fn from_erofs(image: &Path, contents: Contents) -> anyhow::Result<FileTree> {
    let file = std::fs::File::open(image)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", image.display()))?;
    let mut reader = ErofsReader::new(file)
        .map_err(|e| anyhow::anyhow!("reading {} as erofs: {e}", image.display()))?;
    let entries = reader
        .walk()
        .map_err(|e| anyhow::anyhow!("walking {}: {e}", image.display()))?;

    // One pass first, to learn how many names each inode has. A node is built with its link
    // count and the names sharing an inode are not adjacent, so there is nothing to count
    // while building.
    let mut names_per_nid: HashMap<u32, u32> = HashMap::new();
    for entry in &entries {
        if matches!(entry.kind, ErofsEntryKind::RegularFile) {
            *names_per_nid.entry(entry.nid).or_insert(0) += 1;
        }
    }

    let mut ids: HashMap<u32, RegularFileId> = HashMap::new();
    let mut tree = FileTree::new();
    for entry in entries {
        let path = entry.path.as_os_str().as_encoded_bytes().to_vec();
        if path.is_empty() {
            continue;
        }

        let node = match entry.kind {
            ErofsEntryKind::Directory => {
                let mut node = DirectoryNode::new(entry.metadata.clone());
                node.xattrs = entry.xattrs;
                TreeNode::Directory(node)
            }
            ErofsEntryKind::RegularFile => {
                let data = match contents {
                    Contents::Skip => Vec::new(),
                    Contents::Read => {
                        let mut buf = Vec::with_capacity(entry.size as usize);
                        reader
                            .file_data_reader(entry.nid)
                            .map_err(|e| anyhow::anyhow!("opening {}: {e}", entry.path.display()))?
                            .read_to_end(&mut buf)
                            .map_err(|e| {
                                anyhow::anyhow!("reading {}: {e}", entry.path.display())
                            })?;
                        buf
                    }
                };
                TreeNode::RegularFile(RegularFileNode {
                    id: *ids.entry(entry.nid).or_default(),
                    metadata: entry.metadata.clone(),
                    xattrs: entry.xattrs,
                    data: FileData::Memory(data),
                    nlink: names_per_nid.get(&entry.nid).copied().unwrap_or(1),
                })
            }
            ErofsEntryKind::Symlink => TreeNode::Symlink(SymlinkNode {
                metadata: entry.metadata.clone(),
                target: reader
                    .read_link_by_nid(entry.nid)
                    .map_err(|e| anyhow::anyhow!("reading {}: {e}", entry.path.display()))?,
            }),
            ErofsEntryKind::CharDevice => {
                let (major, minor) = entry.rdev.unwrap_or((0, 0));
                TreeNode::CharDevice(DeviceNode {
                    metadata: entry.metadata.clone(),
                    major,
                    minor,
                })
            }
            ErofsEntryKind::BlockDevice => {
                let (major, minor) = entry.rdev.unwrap_or((0, 0));
                TreeNode::BlockDevice(DeviceNode {
                    metadata: entry.metadata.clone(),
                    major,
                    minor,
                })
            }
            ErofsEntryKind::Fifo => TreeNode::Fifo(entry.metadata.clone()),
            ErofsEntryKind::Socket => TreeNode::Socket(entry.metadata.clone()),
        };

        tree.insert(&path, node)
            .map_err(|e| anyhow::anyhow!("rebuilding {}: {e:?}", entry.path.display()))?;
    }
    Ok(tree)
}

/// Read a host directory as a tree.
///
/// `microsandbox-image` ingests tarballs and not directories, and `/abin` is assembled out
/// of directories, so this is cortex's.
///
/// **Symlinks are recorded, never followed.** The directory is the caller's, and walking
/// through a link in it would put whatever it points at — possibly the whole filesystem —
/// into the layer.
///
/// Entries are visited in name order, so the same directory gives the same tree and
/// therefore the same image. That is what makes a directory something the store can address
/// by content.
pub fn from_dir(root: &Path) -> anyhow::Result<FileTree> {
    let mut tree = FileTree::new();
    walk_dir(root, &mut Vec::new(), &mut tree)?;
    Ok(tree)
}

fn walk_dir(dir: &Path, prefix: &mut Vec<u8>, tree: &mut FileTree) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let restore = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(entry.file_name().as_encoded_bytes());

        // `symlink_metadata`, not `metadata`: a link is a node here, not the thing it names.
        let found = entry
            .path()
            .symlink_metadata()
            .map_err(|e| anyhow::anyhow!("stat {}: {e}", entry.path().display()))?;
        let metadata = InodeMetadata {
            // Owned by root, because a layer is something a guest mounts and the host uid
            // that happened to build it means nothing in there.
            uid: 0,
            gid: 0,
            mode: (found.permissions().mode() & 0o7777) as u16,
            // Zeroed, so that the same directory gives the same image whenever it is read.
            mtime: 0,
            mtime_nsec: 0,
        };

        if found.file_type().is_symlink() {
            let target = std::fs::read_link(entry.path())
                .map_err(|e| anyhow::anyhow!("reading the link {}: {e}", entry.path().display()))?;
            tree.insert(
                prefix,
                TreeNode::Symlink(SymlinkNode {
                    metadata: InodeMetadata {
                        mode: 0o777,
                        ..metadata
                    },
                    target: target.as_os_str().as_encoded_bytes().to_vec(),
                }),
            )
            .map_err(|e| anyhow::anyhow!("adding {}: {e:?}", entry.path().display()))?;
        } else if found.is_dir() {
            tree.insert(prefix, TreeNode::Directory(DirectoryNode::new(metadata)))
                .map_err(|e| anyhow::anyhow!("adding {}: {e:?}", entry.path().display()))?;
            walk_dir(&entry.path(), prefix, tree)?;
        } else if found.is_file() {
            let data = std::fs::read(entry.path())
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", entry.path().display()))?;
            tree.insert(
                prefix,
                TreeNode::RegularFile(RegularFileNode {
                    id: RegularFileId::new(),
                    metadata,
                    xattrs: vec![],
                    data: FileData::Memory(data),
                    nlink: 1,
                }),
            )
            .map_err(|e| anyhow::anyhow!("adding {}: {e:?}", entry.path().display()))?;
        } else {
            // A socket, a fifo, a device somebody left here. Left out rather than refused:
            // the directory is the caller's and this is not what it is for, but nothing about
            // it makes the rest unusable. Said aloud so it is not a silent omission.
            eprintln!(
                "cortex-uvm-console: {} is not a file, a directory or a link — leaving it out",
                entry.path().display()
            );
        }

        prefix.truncate(restore);
    }
    Ok(())
}

/// What a node is, in words.
///
/// `TreeNode` implements no `Debug`, and an assertion that fails still has to say what it
/// found. Used by this module's tests and by the ones beside it.
#[cfg(test)]
pub(crate) fn describe(node: Option<&TreeNode>) -> &'static str {
    match node {
        None => "absent",
        Some(TreeNode::RegularFile(_)) => "a file",
        Some(TreeNode::Directory(_)) => "a directory",
        Some(TreeNode::Symlink(_)) => "a symlink",
        Some(TreeNode::CharDevice(_)) => "a character device",
        Some(TreeNode::BlockDevice(_)) => "a block device",
        Some(TreeNode::Fifo(_)) => "a fifo",
        Some(TreeNode::Socket(_)) => "a socket",
    }
}

#[cfg(test)]
mod tests {
    use microsandbox_image::erofs::write_erofs;

    use super::*;

    fn meta(mode: u16) -> InodeMetadata {
        InodeMetadata {
            uid: 0,
            gid: 0,
            mode,
            mtime: 0,
            mtime_nsec: 0,
        }
    }

    /// Two names on one inode, plus a plain file to tell them from.
    fn hardlinked() -> FileTree {
        let mut tree = FileTree::new();
        let shared = RegularFileId::new();
        let payload = "x".repeat(9000);
        tree.insert(b"bin", TreeNode::Directory(DirectoryNode::new(meta(0o755))))
            .unwrap();
        for name in [&b"bin/a"[..], &b"bin/b"[..]] {
            tree.insert(
                name,
                TreeNode::RegularFile(RegularFileNode {
                    id: shared,
                    metadata: meta(0o755),
                    xattrs: vec![],
                    data: FileData::Memory(payload.as_bytes().to_vec()),
                    nlink: 2,
                }),
            )
            .unwrap();
        }
        tree.insert(
            b"bin/c",
            TreeNode::RegularFile(RegularFileNode {
                id: RegularFileId::new(),
                metadata: meta(0o644),
                xattrs: vec![],
                data: FileData::Memory(b"alone\n".to_vec()),
                nlink: 1,
            }),
        )
        .unwrap();
        tree
    }

    #[test]
    fn a_recovered_tree_keeps_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("hl.erofs");
        let written = write_erofs(&hardlinked(), &image).unwrap();

        let back = from_erofs(&image, Contents::Read).unwrap();
        match back.get(b"bin/a") {
            Some(TreeNode::RegularFile(f)) => assert_eq!(f.nlink, 2, "the link count was lost"),
            other => panic!("bin/a is {}", describe(other)),
        }
        match back.get(b"bin/c") {
            Some(TreeNode::RegularFile(f)) => assert_eq!(f.nlink, 1),
            other => panic!("bin/c is {}", describe(other)),
        }

        // The one that matters: re-writing must not duplicate the shared data.
        let again = dir.path().join("hl-again.erofs");
        let rewritten = write_erofs(&back, &again).unwrap();
        assert_eq!(
            rewritten.total_blocks, written.total_blocks,
            "re-writing a recovered tree changed its size — hardlinks were not shared"
        );
        assert_eq!(
            rewritten.file_blocks.get(Path::new("bin/a")),
            rewritten.file_blocks.get(Path::new("bin/b")),
            "the two names no longer point at one copy"
        );
    }

    #[test]
    fn contents_are_recovered_when_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("hl.erofs");
        write_erofs(&hardlinked(), &image).unwrap();

        let back = from_erofs(&image, Contents::Read).unwrap();
        match back.get(b"bin/c") {
            Some(TreeNode::RegularFile(f)) => assert_eq!(f.data.read_all().unwrap(), b"alone\n"),
            other => panic!("bin/c is {}", describe(other)),
        }
    }

    #[test]
    fn contents_are_skipped_when_they_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("hl.erofs");
        write_erofs(&hardlinked(), &image).unwrap();

        let back = from_erofs(&image, Contents::Skip).unwrap();
        match back.get(b"bin/c") {
            Some(TreeNode::RegularFile(f)) => {
                assert!(
                    f.data.read_all().unwrap().is_empty(),
                    "contents were read anyway"
                );
            }
            other => panic!("bin/c is {}", describe(other)),
        }
    }

    /// The two things a layer says that are not files: a deletion and an emptied directory.
    #[test]
    fn whiteouts_and_opaque_directories_survive_a_round_trip() {
        use microsandbox_image::tree::Xattr;

        const OPAQUE: &[u8] = b"trusted.overlay.opaque";

        let mut tree = FileTree::new();
        tree.insert(
            b"etc",
            TreeNode::CharDevice(DeviceNode {
                metadata: meta(0o644),
                major: 0,
                minor: 0,
            }),
        )
        .unwrap();
        let mut emptied = DirectoryNode::new(meta(0o755));
        emptied.xattrs.push(Xattr {
            name: OPAQUE.to_vec(),
            value: b"y".to_vec(),
        });
        tree.insert(b"media", TreeNode::Directory(emptied)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("marks.erofs");
        write_erofs(&tree, &image).unwrap();

        let back = from_erofs(&image, Contents::Skip).unwrap();
        match back.get(b"etc") {
            Some(TreeNode::CharDevice(d)) => assert_eq!((d.major, d.minor), (0, 0)),
            other => panic!("the whiteout is {}", describe(other)),
        }
        match back.get(b"media") {
            Some(TreeNode::Directory(d)) => assert!(
                d.xattrs.iter().any(|x| x.name == OPAQUE),
                "the opaque xattr was lost"
            ),
            other => panic!("the opaque directory is {}", describe(other)),
        }
    }

    #[test]
    fn a_directory_becomes_a_tree() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        std::fs::write(root.join("plain.txt"), b"hello\n").unwrap();
        std::fs::write(root.join("nested/deeper/leaf"), b"leaf\n").unwrap();
        let exe = root.join("runnable");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("plain.txt", root.join("alias")).unwrap();

        let tree = from_dir(root).unwrap();

        match tree.get(b"plain.txt") {
            Some(TreeNode::RegularFile(f)) => {
                assert_eq!(f.data.read_all().unwrap(), b"hello\n");
                assert_eq!(f.metadata.mode & 0o777, 0o644);
                assert_eq!(f.metadata.uid, 0, "the host's uid reached the layer");
            }
            other => panic!("plain.txt is {}", describe(other)),
        }
        match tree.get(b"runnable") {
            Some(TreeNode::RegularFile(f)) => assert_eq!(f.metadata.mode & 0o777, 0o755),
            other => panic!("runnable is {}", describe(other)),
        }
        match tree.get(b"alias") {
            Some(TreeNode::Symlink(s)) => assert_eq!(s.target, b"plain.txt"),
            other => panic!("alias is {}", describe(other)),
        }
        assert!(matches!(
            tree.get(b"nested/deeper"),
            Some(TreeNode::Directory(_))
        ));
        assert!(matches!(
            tree.get(b"nested/deeper/leaf"),
            Some(TreeNode::RegularFile(_))
        ));
    }

    #[test]
    fn a_symlink_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/inside"), b"x").unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();

        let tree = from_dir(root).unwrap();
        assert!(
            matches!(tree.get(b"link"), Some(TreeNode::Symlink(_))),
            "the link was walked"
        );
        assert!(
            tree.get(b"link/inside").is_none(),
            "the link was walked into"
        );
    }

    #[test]
    fn an_empty_directory_is_an_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        assert!(from_dir(dir.path()).unwrap().root.entries.is_empty());
    }

    /// What makes a directory content-addressable: read it twice, get the same image.
    #[test]
    fn the_same_directory_gives_the_same_image() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/one"), b"1").unwrap();
        std::fs::write(root.join("two"), b"2").unwrap();

        let first = dir.path().join("first.erofs");
        let second = dir.path().join("second.erofs");
        write_erofs(&from_dir(&root).unwrap(), &first).unwrap();
        write_erofs(&from_dir(&root).unwrap(), &second).unwrap();
        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap()
        );
    }
}
