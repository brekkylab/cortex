use std::collections::HashSet;
use std::fs::File;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};

use crate::rootfs::{Digest, Image, Layer, cache};
use cortex::rootfs_v2::Step;
use microsandbox_image::erofs::write_erofs;
use microsandbox_image::tree::{
    DirectoryNode, FileData, FileTree, InodeMetadata, RegularFileId, RegularFileNode, SymlinkNode,
    TreeNode,
};
use serde::{Deserialize, Serialize};

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
        //
        // Asked for by name first, and a hit is the whole step: no machine, no command, and
        // the same layer digest the first build got — which is what makes a declaration built
        // twice one image. See [`Run`] for what that name is made of.
        Step::Run(command) => {
            let named = Run::of(&image, &command)?;
            let layer = match named.current() {
                Some(kept) => kept,
                None => {
                    // Held, and then asked a second time. A build that waited here wanted the
                    // answer the one ahead of it was making, not a second machine making the
                    // same one — which is why the lock is taken before the guest rather than
                    // around the publishing at the end.
                    let _held = named.lock()?;
                    match named.current() {
                        Some(kept) => kept,
                        None => run(&image, &command, &named).await?,
                    }
                }
            };
            image.layers.push(layer);
        }
    }

    image.store()?;
    Ok(image)
}

/// Boot a machine on `image`, run `command` in it, and keep the layer it leaves.
///
/// Split out so the two questions the caller asks — is it kept, and is it kept now that this
/// build holds the derivation — read as two questions rather than as a match whose second arm
/// is a machine.
async fn run(image: &Image, command: &str, named: &Run) -> anyhow::Result<Layer> {
    // Said only when it is about to happen. A step that was kept takes milliseconds, and the
    // point of announcing one is the wait somebody is sitting through — so silence is how a
    // build says it reused what it had, and this line is how it says it did not.
    eprintln!("{}: RUN {command}", env!("CARGO_BIN_NAME"));

    // One session with one command in it: the machine is the same machine a session boots,
    // and what makes this a step rather than a session is that nobody else gets to ask it
    // anything before it goes down.
    let argv = vec!["sh".to_string(), "-c".to_string(), command.to_string()];
    let mut uvm = crate::session::uvm::Uvm::boot(
        image,
        &[],
        // Everything, and no port on this host. A step is the declaration's own command run
        // where the declaration said to run it — `apt-get`, `pip`, a `curl` of something the
        // author named — so a build with no way out is a build that cannot install anything.
        // What it is not given is a door back into the machine doing the building.
        crate::contract::Network::Full,
        &[],
        None,
    )
    .await?;
    let exit = uvm.exec(&argv, None).await?;
    let layer = uvm.commit().await?;

    // Published only past this, so a command that failed is one that runs again rather than a
    // failure kept under a name that says nothing about it.
    //
    // Both streams, and stderr last: a build tool that failed says why on stderr and leaves
    // stdout to whatever it had been producing, so a message carrying only the one is empty
    // in exactly the case somebody is reading it. And said when the guest cut them, because
    // the end of a failing command is the part that says why, and that is the end this does
    // not have.
    anyhow::ensure!(
        exit.code == 0,
        "RUN {command:?} exited with {}:\n{}{}{}",
        exit.code,
        String::from_utf8_lossy(&exit.stdout),
        String::from_utf8_lossy(&exit.stderr),
        if exit.truncated {
            "\n(output was cut off)"
        } else {
            ""
        }
    );
    named.publish(&layer)?;
    Ok(layer)
}

// What a `RUN` left, kept under what was asked for.
//
// Every other artifact in this store is named by its own bytes. A `RUN` cannot be: it is a
// command in a guest, and the filesystem it leaves differs from the one it left an hour ago
// in timestamps, caches and logs — so the layer it commits has a fresh digest every time and
// a name computed from it is never the name it had before.
//
// # The shape, which is `microsandbox-image`'s
//
// The same problem is already solved next door, for the flat ext4 rootfs — also produced by
// doing work whose output bytes nobody can predict. `MATERIALIZATION.md` states the rule as
// *"a flat derivation includes the manifest digest, ordered layer `diff_id`s, target platform,
// and ext4 materializer ABI; the published raw ext4 blob is addressed by its verified byte
// digest"*. So: **content-address the inputs, and keep a pointer to the output.** Three
// shelves rather than one, because a pointer and the thing it points at are different kinds
// of value:
//
// | | this half of the module | `microsandbox-image` |
// |---|---|---|
// | pointer, keyed by what was asked | `runs/refs/<asked>.json` | `flat/refs/<manifest>.json` |
// | output, keyed by its own bytes | `layers/<digest>.erofs` | `flat/blobs/<digest>.raw` |
// | lock, keyed by the derivation | `runs/locks/<derivation>.lock` | `flat/locks/<derivation>.lock` |
//
// The output shelf is [`Layer`](crate::rootfs::Layer) itself, which is the one difference and not a
// deliberate one: a layer is already content-addressed and already shared with every pulled
// layer, so a `RUN` has nowhere else it would want to put one.
//
// # Two names, and why it is not one
//
// **Asked** is what a caller has in hand: the image the command runs over, and the command.
// It is the key, because looking a `RUN` up must not require knowing anything the caller
// cannot know.
//
// **The derivation** is everything that decides the answer, which is more than that: the
// platform, and [`RUN_ABI`] — the version of the agreement between the guest that walks its
// upperdir into a tar and this end that ingests one. Neither belongs in the key, because a
// caller asking "what did this command over this image leave?" is not asking about them. So
// the derivation is *stored* and checked on the way out: a ref whose derivation is not the
// one being asked for is a miss, which is how a change to [`RUN_ABI`] retires every entry
// written before it without anything having to go and delete them.
//
// `microsandbox-image` keeps the ABI in the derivation digest *and* as a field of its own,
// and checks both — "the ABI is part of the digest, but checking the persisted field also
// rejects malformed or hand-edited references before they are exposed as cache hits". This
// does the same.

/// How a `RUN` turns into a layer, as a number to compare.
///
/// Bumped by hand when the guest changes what it puts in the tar it commits, or when this end
/// changes what it makes of one — the two together are the only reason a command that ran
/// before would leave something different now. Not a digest of the guest binary: that changes
/// on every build of anything the guest links, and retiring the whole cache because a log
/// message moved is a cost with nothing behind it.
const RUN_ABI: u32 = 1;

/// The ref format, so that a shape changed later is a miss rather than a misread.
const RUN_REF_SCHEMA: u32 = 1;

/// What is kept about a `RUN` that has already happened.
///
/// Every field but the last two is there to be checked rather than used: the answer is
/// [`layer_digest`](Self::layer_digest), and the rest is what says the answer is still the
/// answer.
#[derive(Debug, Serialize, Deserialize)]
struct RunRef {
    /// Ref schema version.
    schema: u32,
    /// The image the command ran over — the first half of the key, stored so a ref read out
    /// of the wrong file is caught rather than believed.
    image_digest: String,
    /// The command, as declared.
    command: String,
    /// Everything that decides the answer, including the platform and the ABI.
    derivation_digest: String,
    /// The ABI again, on its own. See this module's docs.
    run_abi: u32,
    /// The layer the command left, in `layers/`.
    layer_digest: String,
    /// How large that layer is, so a truncated one is a miss and not a boot that fails
    /// somewhere further in.
    layer_bytes: u64,
}

/// One `RUN`, as the store names it.
struct Run {
    asked: Digest,
    derivation: Digest,
    image: String,
    command: String,
}

impl Run {
    /// Name what is about to be asked: this command, over this image.
    fn of(image: &Image, command: &str) -> anyhow::Result<Run> {
        let image_digest = image.digest()?.to_string();

        // The key: what a caller has in hand, and nothing else.
        let mut asked = Vec::new();
        asked.extend_from_slice(b"cortex.uvm-v2.run\0");
        asked.extend_from_slice(image_digest.as_bytes());
        asked.push(0);
        asked.extend_from_slice(command.as_bytes());

        // The derivation: everything that decides the answer. The ABI first, the way the
        // neighbour writes it, so that a bump moves every digest rather than only those whose
        // other fields happen to be long enough to matter.
        let mut derivation = Vec::new();
        derivation.extend_from_slice(b"cortex.uvm-v2.run-derivation\0");
        derivation.extend_from_slice(&RUN_ABI.to_le_bytes());
        derivation.push(0);
        derivation.extend_from_slice(image.os.as_bytes());
        derivation.push(b'/');
        derivation.extend_from_slice(image.arch.as_bytes());
        derivation.push(0);
        derivation.extend_from_slice(image_digest.as_bytes());
        derivation.push(0);
        derivation.extend_from_slice(command.as_bytes());

        Ok(Run {
            asked: Digest::of(&asked),
            derivation: Digest::of(&derivation),
            image: image_digest,
            command: command.to_string(),
        })
    }

    /// The layer this command left, when what is kept is still an answer to what is asked.
    ///
    /// `None` for a `RUN` that has not happened, and for one that has under a derivation that
    /// is no longer this one — a different platform, or an [`RUN_ABI`] since bumped. Also for
    /// a ref that does not parse, name the image it is filed under, or point at a layer of the
    /// size it claims: a note nobody can act on is not an answer, and running the command
    /// again is. Nothing here fails, for that reason.
    fn current(&self) -> Option<Layer> {
        let kept = std::fs::read_to_string(self.ref_path()).ok()?;
        let kept: RunRef = serde_json::from_str(&kept).ok()?;

        let matches = kept.schema == RUN_REF_SCHEMA
            && kept.run_abi == RUN_ABI
            && kept.image_digest == self.image
            && kept.command == self.command
            && kept.derivation_digest == self.derivation.to_string();
        if !matches {
            return None;
        }

        let layer = Layer::new(kept.layer_digest.parse().ok()?);
        match std::fs::metadata(layer.path()) {
            Ok(found) if found.len() == kept.layer_bytes => Some(layer),
            _ => None,
        }
    }

    /// Hold this derivation until the returned value drops.
    ///
    /// What it is worth is a machine: two builds of one declaration arriving together would
    /// otherwise both boot a guest and run the command, and one of the two answers is thrown
    /// away. Whoever waits here asks again afterwards — see the caller — and gets what the
    /// one ahead of it published.
    ///
    /// Keyed by the derivation and not by what was asked, so that two builds that would
    /// produce *different* answers do not wait on each other.
    fn lock(&self) -> anyhow::Result<Held> {
        let locks = runs().join("locks");
        std::fs::create_dir_all(&locks)?;
        let path = locks.join(format!("{}.lock", self.derivation.path_safe()));

        let file =
            File::create(&path).map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
        // SAFETY: `flock` takes a descriptor this process owns and holds open for as long as
        // the lock is held; the lock is advisory and affects no memory.
        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        anyhow::ensure!(
            held == 0,
            "locking {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok(Held(file))
    }

    /// Keep what this command left.
    ///
    /// Written beside and renamed, then both the file and the directory entry flushed, because
    /// what this points at is expensive to make again and a ref that survives a crash half
    /// written would be read as one that points nowhere.
    fn publish(&self, layer: &Layer) -> anyhow::Result<()> {
        let refs = runs().join("refs");
        std::fs::create_dir_all(&refs)?;

        let kept = RunRef {
            schema: RUN_REF_SCHEMA,
            image_digest: self.image.clone(),
            command: self.command.clone(),
            derivation_digest: self.derivation.to_string(),
            run_abi: RUN_ABI,
            layer_digest: layer.digest().to_string(),
            layer_bytes: std::fs::metadata(layer.path())
                .map_err(|e| anyhow::anyhow!("stat {}: {e}", layer.path().display()))?
                .len(),
        };

        let path = self.ref_path();
        let part = refs.join(format!(
            "{}.{}.part",
            self.asked.path_safe(),
            std::process::id()
        ));
        write(&part, &serde_json::to_vec_pretty(&kept)?)?;
        std::fs::rename(&part, &path).map_err(|e| {
            anyhow::anyhow!("renaming {} to {}: {e}", part.display(), path.display())
        })?;
        sync_dir(&refs)
    }

    fn ref_path(&self) -> PathBuf {
        runs()
            .join("refs")
            .join(format!("{}.json", self.asked.path_safe()))
    }
}

/// Forget every `RUN` filed under an image in `removed`, and say how many went.
///
/// A ref is keyed by the image the command ran over, so one whose image is gone can never be
/// asked for again: the key is computed from a digest nothing will name. It is a note about
/// something that no longer exists, and the layer it points at is swept by the same pass that
/// removed the image.
///
/// Nothing here fails. A ref that cannot be read is one nothing can act on either, and a
/// removal that stopped at it would leave the image half gone — so an unreadable ref is passed
/// over and the rest is still forgotten.
pub fn forget_runs(removed: &HashSet<String>) -> usize {
    let refs = runs().join("refs");
    let Ok(entries) = std::fs::read_dir(&refs) else {
        return 0;
    };

    let mut forgotten = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kept) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(kept) = serde_json::from_str::<RunRef>(&kept) else {
            continue;
        };
        if !removed.contains(&kept.image_digest) {
            continue;
        }

        // The lock too, which is keyed by the derivation the ref carries — an empty file whose
        // only purpose was to make two builds of this wait for each other.
        if let Ok(derivation) = kept.derivation_digest.parse::<Digest>() {
            let _ = std::fs::remove_file(
                runs()
                    .join("locks")
                    .join(format!("{}.lock", derivation.path_safe())),
            );
        }
        if std::fs::remove_file(&path).is_ok() {
            forgotten += 1;
        }
    }
    forgotten
}

/// What a `RUN` left, and the locks that keep two builds from making it twice.
fn runs() -> PathBuf {
    crate::rootfs::home().join("runs")
}

/// A derivation held. Releasing it is closing the file, which is what `flock` unlocks on.
struct Held(#[allow(dead_code)] File);

fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut file =
        File::create(path).map_err(|e| anyhow::anyhow!("creating {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| anyhow::anyhow!("flushing {}: {e}", path.display()))
}

/// Flush a directory entry, so a rename survives what the file it renamed already has.
fn sync_dir(path: &Path) -> anyhow::Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| anyhow::anyhow!("flushing {}: {e}", path.display()))
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
