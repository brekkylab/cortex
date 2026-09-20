use std::path::Path;

use crate::rootfs::{Image, Layer, cache};
use cortex::rootfs_v2::Step;
use microsandbox_image::erofs::write_erofs;
use microsandbox_image::tree::{
    DirectoryNode, FileData, FileTree, InodeMetadata, RegularFileId, RegularFileNode, SymlinkNode,
    TreeNode,
};

pub async fn step(
    mut image: Image,
    step: Step,
    context: impl AsRef<Path>,
) -> anyhow::Result<Image> {
    match step {
        // In the position it was first given: saying it again changes the value and leaves
        // the variable where it was.
        Step::Env { key, value } => {
            let assignment = format!("{key}={value}");
            match image.env.iter_mut().find(|stated| {
                stated
                    .split_once('=')
                    .is_some_and(|(stated, _)| stated == key)
            }) {
                Some(stated) => *stated = assignment,
                None => image.env.push(assignment),
            }
        }

        Step::Workdir(dir) => image.workdir = Some(dir),

        // A layer of its own, holding what was copied and the directories it goes under.
        Step::Copy { src, dst } => {
            anyhow::ensure!(
                src.is_relative(),
                "a COPY source is relative to the build context, and {} is not",
                src.display()
            );
            anyhow::ensure!(
                !src.components()
                    .any(|part| part == std::path::Component::ParentDir),
                "a COPY source is inside the build context, and {} climbs out of it",
                src.display()
            );

            let named: Vec<&str> = dst.split('/').filter(|part| !part.is_empty()).collect();
            let Some((_, parents)) = named.split_last() else {
                anyhow::bail!("a COPY destination is a path in the image, and {dst:?} names none")
            };

            // The destination's parents are made along with it, at the mode a directory
            // nobody declared has to have for the image to be usable.
            let mut tree = FileTree::new();
            let mut walked = String::new();
            for parent in parents {
                walked.push('/');
                walked.push_str(parent);
                tree.insert(
                    walked.as_bytes(),
                    TreeNode::Directory(DirectoryNode::new(InodeMetadata {
                        uid: 0,
                        gid: 0,
                        mode: 0o755,
                        mtime: 0,
                        mtime_nsec: 0,
                    })),
                )
                .map_err(|e| anyhow::anyhow!("adding {walked}: {e:?}"))?;
            }
            tree.insert(dst.as_bytes(), node(&context.as_ref().join(&src))?)
                .map_err(|e| anyhow::anyhow!("adding {dst}: {e:?}"))?;

            let written = cache()
                .tmp_dir()
                .join(format!("{}.erofs", std::process::id()));
            write_erofs(&tree, &written)
                .map_err(|e| anyhow::anyhow!("writing the layer: {e:?}"))?;
            image.layers.push(Layer::publish(&written)?);
        }

        // A command in a guest, and the layer it left behind. The machine is booted on the
        // image as it stands, so what a `RUN` sees is every step before it.
        Step::Run(command) => {
            let argv = vec!["sh".to_string(), "-c".to_string(), command.clone()];
            // One session with one command in it: the machine is the same machine a session
            // boots, and what makes this a step rather than a session is that nobody else
            // gets to ask it anything before it goes down.
            let mut uvm =
                crate::session::uvm::Uvm::boot(&image, &[], crate::contract::Network::Full).await?;
            let exit = uvm.exec(&argv, None).await?;
            let layer = uvm.commit().await?;
            anyhow::ensure!(
                exit.code == 0,
                "RUN {command:?} exited with {}:\n{}",
                exit.code,
                String::from_utf8_lossy(&exit.stdout)
            );
            image.layers.push(layer);
        }
    }

    image.store()?;
    Ok(image)
}

// Everything under `path`, owned by root with its mtimes zeroed, so the same files give the
// same layer whenever they are read. Recursive, because a directory is.
fn node(path: &Path) -> anyhow::Result<TreeNode> {
    use std::os::unix::fs::PermissionsExt as _;

    // `symlink_metadata`, not `metadata`: a link is a node in the layer, not the thing it
    // names.
    let found = std::fs::symlink_metadata(path)
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", path.display()))?;
    let owned = InodeMetadata {
        uid: 0,
        gid: 0,
        mode: (found.permissions().mode() & 0o7777) as u16,
        mtime: 0,
        mtime_nsec: 0,
    };

    if found.file_type().is_symlink() {
        Ok(TreeNode::Symlink(SymlinkNode {
            metadata: InodeMetadata {
                mode: 0o777,
                ..owned
            },
            target: std::fs::read_link(path)?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        }))
    } else if found.is_dir() {
        let mut dir = DirectoryNode::new(owned);
        for entry in std::fs::read_dir(path)? {
            let name = entry?.file_name();
            let below = node(&path.join(&name))?;
            dir.entries.insert(name, below);
        }
        Ok(TreeNode::Directory(dir))
    } else if found.is_file() {
        Ok(TreeNode::RegularFile(RegularFileNode {
            id: RegularFileId::new(),
            metadata: owned,
            xattrs: vec![],
            data: FileData::Memory(std::fs::read(path)?),
            nlink: 1,
        }))
    } else {
        anyhow::bail!(
            "{} is not a file, a directory or a link, and a layer has nowhere to put it",
            path.display()
        )
    }
}
