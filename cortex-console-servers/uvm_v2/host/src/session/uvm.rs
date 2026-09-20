//! A micro-VM, booted on an image built here.
//!
//! Two things start one, and they differ in how long it lives rather than in what it does.
//! A session boots once and runs whatever its client asks for until the client is done; a
//! build's `RUN` wants one command and the filesystem it left behind. So the long-lived one
//! is a value, and the short one is a function over it.
//!
//! Either way the guest is `cortex-uvm-v2-guest`, which libkrun execs as the first userspace
//! process — in `boot` mode for a session, `exec` mode for a build step.
//!
//! **Nothing here holds a hypervisor.** What this module does is make what a machine needs on
//! disk, spawn `cortex-uvm-v2-boot` over it, and talk to the guest down a socket that boot
//! turned into a console port. The VMM, the network stack and the entitlement they need are
//! all in that other binary — see [`helper`](super::helper) for what puts a copy of it
//! somewhere runnable, and [`BootArgs`] for the whole of what this end tells it.

use std::path::{Path, PathBuf};

use cortex::console::{
    Call, CommitCall, ExecCall, Message, Response,
    stdio::{read, write},
};
use microsandbox_image::ext4::{Ext4FormatOptions, format_ext4};

use crate::contract::{
    ARTIFACTS_PATH, BaseFormat, BootArgs, CONTEXT_PATH, GUEST_BIN_PATH, HANDSHAKE, IMAGE_SPEC_PATH,
    ImageSpec, LAYER_TAR, Network, SCRATCH_PATH,
};

/// The guest half, cross-compiled and embedded by `build.rs`. Written into every boot root,
/// which is why the guest crate optimises for size.
const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cortex-uvm-v2-guest"));
use crate::rootfs::{Image, Layer, cache};

/// What a machine needs on disk, made, and the process holding it, started.
///
/// The two things that start one  only in what they tell the guest to be and in what they
/// wait for afterwards, so everything before that is here and said once.
struct Started {
    machine: std::process::Child,
    listener: std::os::unix::net::UnixListener,
    scratch: Vec<PathBuf>,
    commit: PathBuf,
    console: PathBuf,
}

fn start(image: &Image, mounts: &[Mount], network: Network) -> anyhow::Result<Started> {
    use std::{
        os::unix::fs::PermissionsExt as _,
        sync::atomic::{AtomicU64, Ordering},
    };

    // The program a boot runs, written out of this binary and signed — see `helper`.
    let machine = super::helper::boot_binary()?;

    // Paths nothing else in this process or any other will pick, of the shape the sweep
    // in `session` reads a pid back out of: the pid says who owns them, and the counter
    // keeps two boots of one process apart.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mine = |what: &str| {
        std::env::temp_dir().join(format!(
            "{}{}-{seq}-{what}",
            crate::session::PREFIX,
            std::process::id()
        ))
    };
    let (upper, root, socket, commit, console) = (
        mine("session.ext4"),
        mine("boot"),
        mine("port.sock"),
        mine("commit"),
        mine("console.log"),
    );
    let scratch = vec![
        upper.clone(),
        root.clone(),
        socket.clone(),
        commit.clone(),
        console.clone(),
    ];

    // The base, as one disk. It is not scratch and is not made here if it was made once
    // already: the layers, the metadata merged out of them and the descriptor naming both
    // are the store's, and what this boot owns is only what it writes over them.
    let base = image.disk()?;

    // Sparse, so this is a ceiling and not an allocation: a session that writes a
    // kilobyte occupies a kilobyte. What the number bounds is how much a runaway command
    // can write before the guest reports a full disk.
    format_ext4(
        &upper,
        &Ext4FormatOptions {
            size_bytes: 2 << 30,
            // 16 MiB of journal. The default is four times that, which is most of a small
            // session's image spent on a log for writes about to be thrown away.
            journal_blocks: 4096,
        },
    )
    .map_err(|e| anyhow::anyhow!("formatting the session's disk: {e:?}"))?;

    // A real root only for the moment between the kernel handing over and the guest
    // pivoting onto the overlay — long enough to exec one file and read one other.
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&commit)?;
    let binary = root.join(GUEST_BIN_PATH.trim_start_matches('/'));
    std::fs::write(&binary, GUEST)?;
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

    // Bound before the child is started, because the child connects to it as its first
    // act and there is nothing to retry against if it is not there yet.
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .map_err(|e| anyhow::anyhow!("binding the console channel: {e}"))?;

    let mut told = BootArgs {
        boot_root: root.clone(),
        channel: socket.clone(),
        base,
        base_format: BaseFormat::Vmdk,
        session: upper.clone(),
        committable: true,
        commit_out: Some(commit.clone()),
        abin: None,
        context: None,
        artifacts: None,
        scratch: None,
        console: Some(console.clone()),
        network,
        host_ports: Vec::new(),
        vcpus: None,
        memory_mib: None,
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
    let started = std::process::Command::new(&machine)
        .args(told.to_args())
        .spawn()
        .map_err(|e| anyhow::anyhow!("starting {}: {e}", machine.display()))?;

    Ok(Started {
        machine: started,
        listener,
        scratch,
        commit,
        console,
    })
}

/// What a command exited with, and what it wrote.
pub struct Exit {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A host directory the guest can see, and where it sees it.
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
    /// Everything this session made and nothing else reads: gone when this drops, which is
    /// what keeps a session's writes inside the session.
    scratch: Vec<PathBuf>,
    /// Where the guest leaves a layer, when it was booted able to.
    commit: PathBuf,
}

impl Drop for Uvm {
    fn drop(&mut self) {
        let _ = self.machine.kill();
        let _ = self.machine.wait();
        for path in &self.scratch {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }
}

impl Uvm {
    /// Boot `image` and wait for the guest to say it is there.
    ///
    /// Returns once the guest has built its root and opened the port, not merely once the
    /// machine is running — there is nothing a caller can do with the time in between.
    pub async fn boot(image: &Image, mounts: &[Mount], network: Network) -> anyhow::Result<Uvm> {
        use std::io::Read as _;

        let Started {
            mut machine,
            listener,
            scratch,
            commit,
            console,
        } = start(image, mounts, network)?;

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
            scratch,
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
    pub async fn exec(&mut self, argv: &[String], timeout_ms: Option<u64>) -> anyhow::Result<Exit> {
        match self
            .call(Call::Exec(ExecCall {
                cmd: argv.to_vec(),
                timeout_ms,
            }))
            .await?
        {
            Response::Exec(done) => Ok(Exit {
                code: done.code,
                stdout: done.stdout,
                stderr: done.stderr,
            }),
            Response::Error(e) => anyhow::bail!("{argv:?}: {e:?}"),
            other => anyhow::bail!("the guest answered an exec with {other:?}"),
        }
    }

    /// Everything written since the boot, kept as a layer.
    ///
    /// The guest walks its own upperdir and leaves a tar in the scratch it was given; what
    /// is here is turning that into a layer, which is the one part of it that knows what an
    /// image is.
    pub async fn commit(&mut self) -> anyhow::Result<Layer> {
        match self
            .call(Call::Commit(CommitCall {
                id: String::new(),
                env: Vec::new(),
                working_dir: None,
            }))
            .await?
        {
            Response::Commit(_) => {}
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
