//! A micro-VM, booted on an image built here.
//!
//! Two things want one, and they differ only in how long they hold it. A session boots once
//! and asks whatever its client asks until the client is done; a build's `RUN` boots the same
//! machine, asks it one command, keeps the layer and drops it. So there is one [`Uvm`] and no
//! shorter shape beside it: a step is a session nobody else gets to ask anything.
//!
//! The guest is `cortex-uvm-v2-guest`, which libkrun execs as the first userspace process.
//!
//! **Nothing here holds a hypervisor.** What this module does is make what a machine needs on
//! disk, spawn `cortex-uvm-v2-boot` over it, and talk to the guest down a socket that boot
//! turned into a console port. The VMM, the network stack and the entitlement they need are
//! all in that other binary — see [`halves`](super::halves) for where it and the guest are
//! found, and [`BootArgs`] for the whole of what this end tells it.

use std::path::{Path, PathBuf};

use cortex::console::{
    Call, ExecCall, ExecResp, MAX_PAYLOAD, Message, Response, SnapshotCall,
    stdio::{read, write},
};
use microsandbox_image::ext4::{Ext4FormatOptions, format_ext4};

use crate::contract::{
    ARTIFACTS_PATH, BaseFormat, BootArgs, CONTEXT_PATH, GUEST_BIN_PATH, HANDSHAKE, IMAGE_SPEC_PATH,
    ImageSpec, LAYER_TAR, Network, SCRATCH_PATH, SNAPSHOT_PATH,
};
use crate::rootfs::{Image, Layer, cache};

/// How much of a snapshot one response may carry.
///
/// A frame is capped at [`MAX_PAYLOAD`], and a `snapshot`'s answer is its `blob` plus the
/// members around it — `jsonrpc`, `id`, `method` and `result`. A kibibyte covers those several
/// times over.
const MAX_BLOB: u64 = MAX_PAYLOAD as u64 - 1024;

/// Guest vCPUs and memory, when this server was told to override what a boot would otherwise
/// pick.
///
/// Read here and passed on the command line, rather than left for the boot to find in the
/// environment it inherits: what a machine was given is then something this end can be asked,
/// and the boot has one place it learns its shape from.
const VCPUS_ENV: &str = "CORTEX_UVM_VCPUS";
const MEMORY_ENV: &str = "CORTEX_UVM_MEMORY_MIB";

/// An override read out of this server's environment, ignoring anything that is not a number.
///
/// A value nobody can act on is not a reason to refuse a session: these two are a knob for
/// whoever started the process, and the boot's own default is a machine that works. What a
/// typo costs is the override, which is the smallest thing it can cost.
fn number<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.parse().ok()
}

/// A host directory the guest can see, and where it sees it.
#[derive(Clone)]
pub struct Mount {
    pub at: String,
    pub from: PathBuf,
}

/// A booted micro-VM, held for as long as there is something to ask it.
///
/// Dropping it takes the machine down and removes the scratch it was given, which is what
/// makes a session's writes go no further than the session.
pub struct Uvm {
    /// The signed copy holding the hypervisor. Killed when this drops, which is what takes
    /// the machine down.
    machine: std::process::Child,
    /// The console port, split: one call goes out and one answer comes back, so there is
    /// never more than one outstanding and nothing to pair beyond the id.
    incoming: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    outgoing: tokio::net::unix::OwnedWriteHalf,
    /// The next call's id. A number about the wire and nothing else.
    next: u64,
    /// Everything this boot made and nothing else reads: gone when this drops, which is
    /// what keeps a session's writes inside the session.
    dir: PathBuf,
    /// Where the guest leaves a layer, when it was booted able to.
    commit: PathBuf,
}

impl Drop for Uvm {
    fn drop(&mut self) {
        let _ = self.machine.kill();
        let _ = self.machine.wait();
        // This boot's, and only this boot's: the disk beside it is the session's, and a
        // `stop` is what drops this.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Uvm {
    /// Boot `image` and wait for the guest to say it is there.
    ///
    /// Returns once the guest has built its root and opened the port, not merely once the
    /// machine is running — there is nothing a caller can do with the time in between.
    pub async fn boot(
        image: &Image,
        mounts: &[Mount],
        network: Network,
        // Ports on this host the guest may open, on top of whatever `network` allows. Beside
        // it rather than folded into it because that is how the boot is told — see
        // [`BootArgs`] — and because widening the one must never widen the other.
        host_ports: &[u16],
        // What a previous session left, as the layer tar a `snapshot` answered with. Put in
        // the boot root below, for the guest to unpack onto the blank disk it is given.
        snapshot: Option<&[u8]>,
    ) -> anyhow::Result<Uvm> {
        use std::io::Read as _;
        use std::os::unix::fs::PermissionsExt as _;

        // The program a boot runs, signed when it was built — see `halves`.
        let program = super::halves::boot()?;

        // Two directories, because two lifetimes. The session's is named by this process's
        // pid, which is what the sweep in `session` reads back, and holds the one thing that
        // outlives a machine — the disk. `machine/` under it holds what only makes sense
        // while one is up, and is what a `stop` throws away.
        //
        // One name and not a counter per boot, because there is never more than one machine
        // here at a time: dropping the last one kills it, reaps it and removes this directory
        // before anything asks for another, all of it before the drop returns.
        let owner = super::sessions().join(std::process::id().to_string());
        let dir = owner.join("machine");
        std::fs::create_dir_all(&dir)?;

        let upper = owner.join("session.ext4");
        let (root, socket, commit, console) = (
            dir.join("boot"),
            dir.join("port.sock"),
            dir.join("commit"),
            dir.join("console.log"),
        );

        // The base, as one disk. It is not scratch and is not made here if it was made once
        // already: the layers, the metadata merged out of them and the descriptor naming both
        // are the store's, and what this boot owns is only what it writes over them.
        let base = image.disk()?;

        // Once per session and not once per boot: a `stop` hands the machine back and the call
        // after it takes another, and what the session wrote has to still be there when it
        // does. A disk already here is that session carrying on.
        //
        // Sparse, so the size is a ceiling and not an allocation: a session that writes a
        // kilobyte occupies a kilobyte. What the number bounds is how much a runaway command
        // can write before the guest reports a full disk.
        let formatted = !upper.exists();
        if formatted {
            format_ext4(
                &upper,
                &Ext4FormatOptions {
                    size_bytes: 2 << 30,
                    // 16 MiB of journal. The default is four times that, which is most of a
                    // small session's image spent on a log for writes about to be thrown away.
                    journal_blocks: 4096,
                },
            )
            .map_err(|e| anyhow::anyhow!("formatting the session's disk: {e:?}"))?;
        }

        // A real root only for the moment between the kernel handing over and the guest
        // pivoting onto the overlay — long enough to exec one file and read one other.
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&commit)?;

        // A copy and not a link: this tree is served to the guest over virtio-fs, and what it
        // holds has to be a file under the root rather than a name pointing out of it.
        let binary = root.join(GUEST_BIN_PATH.trim_start_matches('/'));
        let from = super::halves::guest()?;
        std::fs::copy(&from, &binary)
            .map_err(|e| anyhow::anyhow!("copying {} into the boot root: {e}", from.display()))?;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
        // Written even when it says nothing: a boot root has one shape either way, and a
        // guest reading a spec that states nothing is simpler than one reasoning about a
        // missing file.
        std::fs::write(
            root.join(IMAGE_SPEC_PATH.trim_start_matches('/')),
            bson::serialize_to_vec(&ImageSpec {
                env: image.env.clone(),
                working_dir: image.workdir.clone(),
            })
            .map_err(|e| anyhow::anyhow!("encoding the image spec: {e}"))?,
        )?;

        // Onto a disk this boot formatted and onto no other. A snapshot says where the
        // session *starts*, and a `stop` hands the machine back with the disk still holding
        // everything the session has done since — so putting it there again on the next boot
        // would undo the session's own work. The boot root is remade per boot, which is what
        // makes leaving the file out the whole of the decision.
        if let Some(blob) = snapshot.filter(|_| formatted) {
            std::fs::write(root.join(SNAPSHOT_PATH.trim_start_matches('/')), blob)
                .map_err(|e| anyhow::anyhow!("writing the snapshot into the boot root: {e}"))?;
        }

        // Bound before the child is started, because the child connects to it as its first
        // act and there is nothing to retry against if it is not there yet.
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .map_err(|e| anyhow::anyhow!("binding the console channel: {e}"))?;

        let mut told = BootArgs {
            boot_root: root,
            channel: socket,
            base,
            base_format: BaseFormat::Vmdk,
            session: upper,
            committable: true,
            commit_out: Some(commit.clone()),
            abin: None,
            context: None,
            artifacts: None,
            scratch: None,
            console: Some(console.clone()),
            network,
            host_ports: host_ports.to_vec(),
            vcpus: number(VCPUS_ENV),
            memory_mib: number(MEMORY_ENV),
        };
        for mount in mounts {
            match mount.at.as_str() {
                CONTEXT_PATH => told.context = Some(mount.from.clone()),
                ARTIFACTS_PATH => told.artifacts = Some(mount.from.clone()),
                SCRATCH_PATH => told.scratch = Some(mount.from.clone()),
                other => anyhow::bail!(
                    "{other} is not one of the three a session names: {CONTEXT_PATH}, \
                     {ARTIFACTS_PATH}, {SCRATCH_PATH}"
                ),
            }
        }

        // Its stdio is this process's: the guest's console — kernel messages, a panic,
        // whatever a boot that failed has to say — arrives there and nowhere else, and a
        // caller looking at a machine that did not come up needs to be able to read it.
        let mut machine = std::process::Command::new(&program)
            .args(told.to_args())
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting {}: {e}", program.display()))?;

        // A virtio-console port discards what the host writes while no process in the guest
        // has it open, rather than queueing it — the socket is up long before the kernel is.
        // So this waits for the guest to say that something is reading, and only then is
        // there a session.
        //
        // Waited for with a deadline and with an eye on the child, because the three ways this
        // does not happen are worth telling apart: a boot that could not start says so by
        // exiting, a guest that died inside says so the same way, and a machine that is merely
        // slow says nothing at all. Without this they are one hang.
        listener.set_nonblocking(true)?;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let (mut port, _) = loop {
            match listener.accept() {
                Ok(connected) => break connected,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(status) = machine.try_wait()? {
                        anyhow::bail!(
                            "the boot exited with {status} before the guest connected{}",
                            said(&console)
                        );
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < until,
                        "the guest did not connect within 60s{}",
                        said(&console)
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => anyhow::bail!("waiting for the guest to connect: {e}"),
            }
        };
        // Accepted from a non-blocking listener, which on some platforms is where the new
        // socket gets it from.
        port.set_nonblocking(false)?;
        port.set_read_timeout(Some(std::time::Duration::from_secs(60)))?;

        let mut heard = [0u8; HANDSHAKE.len()];
        port.read_exact(&mut heard).map_err(|e| {
            anyhow::anyhow!(
                "waiting for the guest to say it is there: {e}{}",
                said(&console)
            )
        })?;
        anyhow::ensure!(
            &heard == HANDSHAKE,
            "the guest said {heard:?} where a handshake was expected"
        );

        // From here the channel is this end's to drive, so it moves to the runtime's socket
        // type: the framing below is async and the accepted one is not.
        port.set_nonblocking(true)?;
        let (incoming, outgoing) = tokio::net::UnixStream::from_std(port)?.into_split();

        Ok(Uvm {
            machine,
            incoming: tokio::io::BufReader::new(incoming),
            outgoing,
            next: 0,
            dir,
            commit,
        })
    }

    /// One call out and its answer back.
    ///
    /// There is one outstanding at a time, so an answer carrying a different id is a peer
    /// that has lost its place and a session cannot carry on from one.
    pub async fn call(&mut self, call: Call) -> anyhow::Result<Response> {
        self.next += 1;
        let id = self.next;
        write(&mut self.outgoing, &Message::Request { id, call }).await?;

        match read(&mut self.incoming).await? {
            Some(Message::Response {
                id: answered,
                result,
            }) if answered == id => Ok(result),
            Some(Message::Response { id: answered, .. }) => anyhow::bail!(
                "the guest answered request {answered}, which is not the {id} it was asked"
            ),
            Some(other) => anyhow::bail!("the guest sent {other:?} instead of an answer"),
            None => anyhow::bail!("the guest closed the channel"),
        }
    }

    /// Run one command, with what the image states already in front of it.
    /// Answered with the protocol's own [`ExecResp`] rather than a shape of this end's: what
    /// a command exited with is what the guest already said, and a copy of it here is a place
    /// for a member to be dropped — which is what [`truncated`](ExecResp::truncated) was.
    pub async fn exec(
        &mut self,
        argv: &[String],
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<ExecResp> {
        match self
            .call(Call::Exec(ExecCall {
                cmd: argv.to_vec(),
                timeout_ms,
            }))
            .await?
        {
            Response::Exec(done) => Ok(done),
            Response::Error(e) => anyhow::bail!("{argv:?}: {e:?}"),
            other => anyhow::bail!("the guest answered an exec with {other:?}"),
        }
    }

    /// Everything written since the boot, as a blob a later boot can start on.
    ///
    /// The same tar [`commit`](Self::commit) turns into a layer, handed back as bytes instead:
    /// the overlay's upperdir, with the session's deletions as the whiteouts overlayfs left.
    /// A [`boot`](Self::boot) given it back unpacks it into a fresh upperdir, which puts the
    /// session where this one stopped.
    ///
    /// **Not the session's disk.** That ext4 is formatted to a ceiling no frame could carry,
    /// and nearly all of what is in it — the journal, the free space, the filesystem's own
    /// metadata — is not the session's work. The upperdir is the part that is.
    ///
    /// The guest is what produces it: the upperdir is a directory only it can see, and the
    /// writes are its kernel's until it syncs. It leaves the tar in the shared scratch rather
    /// than answering with it, because a layer can be far larger than the channel this
    /// protocol runs on — so what comes back over the wire is an acknowledgement, and the
    /// bytes are read from the file here.
    pub async fn snapshot(&mut self) -> anyhow::Result<Vec<u8>> {
        match self.call(Call::Snapshot(SnapshotCall {})).await? {
            Response::Snapshot(_) => {}
            Response::Error(e) => anyhow::bail!("snapshotting: {e:?}"),
            other => anyhow::bail!("the guest answered a snapshot with {other:?}"),
        }

        let tar = self.commit.join(LAYER_TAR);
        anyhow::ensure!(
            tar.is_file(),
            "the guest left no layer at {}",
            tar.display()
        );

        // Said here rather than left to the frame writer. A session can write more than one
        // frame holds, and that end has no way to refuse a single answer — it would fail the
        // write, which is the whole channel and so the whole session, over one call that could
        // have been answered with an error.
        let size = std::fs::metadata(&tar)?.len();
        anyhow::ensure!(
            size <= MAX_BLOB,
            "this session has written {size} bytes, which is more than the {MAX_BLOB} a \
             snapshot can carry"
        );

        std::fs::read(&tar).map_err(|e| anyhow::anyhow!("reading this session's snapshot: {e}"))
    }

    /// Everything written since the boot, kept as a layer.
    ///
    /// The guest walks its own upperdir and leaves a tar in the scratch it was given; what
    /// is here is turning that into a layer, which is the one part of it that knows what an
    /// image is.
    pub async fn commit(&mut self) -> anyhow::Result<Layer> {
        match self.call(Call::Snapshot(SnapshotCall {})).await? {
            Response::Snapshot(_) => {}
            Response::Error(e) => anyhow::bail!("committing: {e:?}"),
            other => anyhow::bail!("the guest answered a commit with {other:?}"),
        }

        let tar = self.commit.join(LAYER_TAR);
        anyhow::ensure!(
            tar.is_file(),
            "the guest left no layer at {}",
            tar.display()
        );

        // The same shape a pulled layer takes: read once, written as an EROFS, named by what
        // came out. Nothing about a step's layer differs from a base's once it is on disk.
        let tree = microsandbox_image::tar::ingest_tar(
            tokio::fs::File::open(&tar).await?,
            &microsandbox_image::tree::ResourceLimits::default(),
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("reading this session's layer: {e:?}"))?;

        let written = cache()
            .tmp_dir()
            .join(format!("{}.erofs", std::process::id()));
        microsandbox_image::erofs::write_erofs(&tree, &written)
            .map_err(|e| anyhow::anyhow!("writing this session's layer: {e:?}"))?;
        Layer::publish(&written)
    }
}

fn said(console: &Path) -> String {
    match std::fs::read_to_string(console) {
        Ok(text) if !text.trim().is_empty() => {
            let tail: Vec<&str> = text.lines().rev().take(20).collect();
            format!(
                ". The machine said:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )
        }
        _ => String::new(),
    }
}
