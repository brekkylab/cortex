//! A booted guest: the process running it, the images it is running on, and the channel
//! into it.
//!
//! One value owns all of it, which is what makes `stop` a `drop` and nothing else. A
//! session that is released kills the boot child, deletes the ext4 image it was writing
//! and removes the boot root and the socket — and the next call that needs a guest gets a
//! new one of each, because none of it was ever shared.
//!
//! Each of those is a field whose type has a destructor, and they are declared in the
//! order they have to happen: the VMM is reaped before the files it had open are deleted.
//! So there is no `Drop` for [`Guest`] itself, and nothing to keep in step with a list of
//! things a boot happened to acquire — a piece added here cleans itself up by being the
//! kind of value that does.
//!
//! # The channel is a socket the child turns into a port
//!
//! ```text
//! this process ──unix socket──► boot child ──virtio-console port──► guest agent
//! ```
//!
//! The middle hop is a descriptor and not a copy. The child connects back here, hands the
//! socket's file descriptor to `msb_krun` as a console port, and then stops being a
//! participant — everything after that is bytes moving between this process and the agent,
//! with a VMM in the path the way a kernel is in the path of a pipe.
//!
//! Which is why the child is spawned rather than talked to. Nothing is asked of it, so
//! there is no protocol with it to get wrong, and what it does when the guest shuts down —
//! `_exit`, taking the process with it — reaches this end as the channel closing.
//!
//! # Why anything has to be waited for before the guest is usable
//!
//! [`HANDSHAKE`] answers that. A virtio-console port discards what the host writes while
//! nothing in the guest has it open, and the socket is connected within milliseconds of
//! the child starting, where a guest takes a second or two to reach userspace. So
//! [`Guest::boot`] waits for the agent to say it is reading before it treats the channel as
//! usable — and that wait doubles as the only signal there is that the guest booted at all.

use std::{
    io,
    os::fd::{FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use cortex::console::{
    Call, Message, RequestId, Response,
    stdio::{read, write},
};
use cortex_uvm_console::layer::{LayerId, LayerStore};
use tokio::{
    io::{AsyncReadExt as _, BufReader},
    net::{
        UnixListener,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::{
    abin,
    assets::{self, BootRoot, CommitScratch, SessionImage},
    contract::{BootArgs, HANDSHAKE, Network},
    helper::boot_helper,
};

/// Where layers live, shared by everything on this host that makes one.
fn layer_store() -> anyhow::Result<LayerStore> {
    LayerStore::open(&assets::home()?.join("layers"))
}

/// Guest vCPUs and memory, if this server was told to override the boot's own defaults.
///
/// Read here and passed on, rather than left for the child to find in the environment it
/// inherits: a value the boot is given is a value the server can be asked what it sent.
const VCPUS_ENV: &str = "CORTEX_UVM_VCPUS";
const MEMORY_ENV: &str = "CORTEX_UVM_MEMORY_MIB";

/// An override read out of this server's environment, ignoring anything that is not a
/// number: a caller who typed nonsense gets the boot's default and a guest that boots.
fn number<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.parse().ok()
}

/// How long a boot may take before it is called a failure.
///
/// Generous, because it covers a kernel coming up, a filesystem being overlaid and a
/// device being probed, on a machine that may be doing other things. It is not a
/// per-command bound and never becomes one: what it protects against is a guest that never
/// reaches userspace, where the alternative is a console that hangs on its first `exec`
/// with nothing to report.
const BOOT_TIMEOUT: Duration = Duration::from_secs(90);

/// A guest that is up, and everything whose lifetime is that guest's.
pub struct Guest {
    /// The VMM. First, so it is gone before anything it had open is deleted.
    _vmm: Vmm,

    /// Where responses arrive. Buffered once: the channel is the protocol's for the life
    /// of the guest, so reading ahead cannot take a byte that was somebody else's.
    incoming: BufReader<OwnedReadHalf>,

    /// Where requests go.
    outgoing: OwnedWriteHalf,

    _socket: Socket,
    _session: SessionImage,
    _boot_root: BootRoot,

    /// The layers the base this guest booted on is made of, bottom first, as ids.
    ///
    /// Kept because a `commit` stitches onto them, and because asking again would mean
    /// resolving the base a second time — which for a pulled image is a registry round trip.
    ///
    /// Ids and not layers: a `Layer` carries its whole data map, one entry per file in it, and
    /// a commit reads nothing from these but the names. The store is the way from a name back
    /// to the layer, and it is walked once at the commit rather than held until the quit.
    pub base_layers: Vec<LayerId>,

    /// Where this guest writes a commit's layer, for a session that may make one.
    ///
    /// Held so that it outlives the guest that writes into it and no longer: the directory
    /// goes when the session does.
    pub commit: Option<CommitScratch>,
}

impl Guest {
    /// Bring a guest up and wait until its agent is reading.
    ///
    /// Everything expensive happens before the child is spawned — provisioning the base
    /// image, formatting the session's — so a failure in any of it is reported as itself
    /// rather than as a boot that timed out.
    /// `workfs` is a directory on this host, or `None` for a session with no tree. It is
    /// shared into the guest **at its own path** — see [`boot`](crate::boot), which is where
    /// that decision is argued.
    pub async fn boot(
        workfs: Option<&Path>,
        image: Option<&str>,
        network: Network,
        host_ports: &[u16],
        committable: bool,
    ) -> anyhow::Result<Guest> {
        let kernel = assets::resolve_kernel()?;
        // The layers come back with the disk because the session keeps them: a `commit`
        // stitches onto what its base was made of, and asking again would mean pulling twice.
        let (base, base_layers) = crate::base::image(image).await?;
        let helper = boot_helper()?;

        // `/abin` is the same disk for every session — cortex's own executables and no
        // others — so this is a store lookup that finds it already there after the first
        // session on this host. On a blocking thread all the same: the first one reads every
        // executable and writes them back out as an EROFS, which is the same reason
        // `SessionImage::create` below is not on the runtime's own thread.
        let store = layer_store()?;
        // **The one place this variable is read.** Here rather than inside `disk`, where the
        // rest of this server's configuration is read, and so that building the disk is a
        // function of its arguments and a test can call it twice.
        let builtin = std::env::var_os(abin::DIR_ENV).map(PathBuf::from);
        let found = tokio::task::spawn_blocking(move || abin::disk(builtin.as_deref(), &store))
            .await
            .map_err(|e| anyhow::anyhow!("preparing /abin: {e}"))?;

        // Cortex has published no executables yet, so the strict reading would mean no
        // session boots at all. A session without them costs its `/abin` and not its
        // existence — said on the way past so it is not silent.
        let abin = match found {
            Ok(disk) => Some(disk),
            Err(e) => {
                eprintln!("cortex-uvm-console: this session gets no /abin: {e}");
                None
            }
        };

        // Formatting writes a filesystem's worth of metadata, which is milliseconds and
        // still not something to do on the runtime's own thread.
        let session = tokio::task::spawn_blocking(SessionImage::create)
            .await
            .map_err(|e| anyhow::anyhow!("formatting the session image: {e}"))??;
        let boot_root = BootRoot::create(&base.spec)?;

        // Made only for a session that said it might commit: it is a directory the guest can
        // write into, and a session that will never write a layer should not be handed one.
        let commit = committable.then(CommitScratch::create).transpose()?;

        let socket = Socket::bind()?;

        let args = BootArgs {
            kernel,
            boot_root: boot_root.path().to_path_buf(),
            channel: socket.path.clone(),
            base: base.path,
            base_format: base.format,
            session: session.path().to_path_buf(),
            network,
            host_ports: host_ports.to_vec(),
            // Told rather than left to the child's inherited environment, which is what made
            // one of these names mean two things once already.
            workfs: workfs.map(Path::to_path_buf),
            committable,
            commit_out: commit.as_ref().map(|scratch| scratch.path().to_path_buf()),
            abin,
            vcpus: number(VCPUS_ENV),
            memory_mib: number(MEMORY_ENV),
        };

        let mut command = Command::new(&helper);
        command
            .args(args.to_args())
            .stdin(Stdio::null())
            // The guest's console — kernel messages, and anything a command's output
            // escapes onto — goes where this process's diagnostics go. Not stdout:
            // that is the protocol's, and a boot message on it corrupts a frame.
            .stdout(Stdio::from(stderr()?))
            .stderr(Stdio::inherit());

        let vmm = Vmm(command
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting the boot helper {}: {e}", helper.display()))?);

        let stream = tokio::time::timeout(BOOT_TIMEOUT, socket.listener.accept())
            .await
            .map_err(|_| anyhow::anyhow!("the boot helper never connected"))?
            .map_err(|e| anyhow::anyhow!("accepting the boot helper's connection: {e}"))?
            .0;

        let (incoming, outgoing) = stream.into_split();
        let mut incoming = BufReader::new(incoming);

        tokio::time::timeout(BOOT_TIMEOUT, greeting(&mut incoming))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the guest never came up. Its kernel log is on stderr; a boot that \
                     stops before userspace is usually the base image or the kernel"
                )
            })??;

        Ok(Guest {
            _vmm: vmm,
            incoming,
            outgoing,
            _socket: socket,
            _session: session,
            _boot_root: boot_root,
            base_layers,
            commit,
        })
    }

    /// Put one call to the agent and bring its answer back.
    ///
    /// The id is the client's, passed through untouched. Nothing here allocates one and
    /// nothing here rewrites one: the client and the agent are the two ends that pair a
    /// response with its request, and a number rewritten in the middle would be a third
    /// opinion about which request an answer belongs to.
    ///
    /// Which is also what the check below is for. There is one call outstanding, so an
    /// answer carrying a different id is a peer that has lost its place, and a session
    /// cannot carry on from one.
    pub async fn relay(&mut self, id: RequestId, call: Call) -> io::Result<Response> {
        write(&mut self.outgoing, &Message::Request { id, call }).await?;

        match read(&mut self.incoming).await? {
            Some(Message::Response {
                id: answered,
                result,
            }) if answered == id => Ok(result),
            Some(Message::Response { id: answered, .. }) => Err(io::Error::other(format!(
                "the guest answered request {answered}, which is not the {id} it was asked"
            ))),
            Some(other) => Err(io::Error::other(format!(
                "the guest sent {other:?} instead of an answer"
            ))),
            None => Err(io::Error::other("the guest closed the channel")),
        }
    }
}

/// The boot child, killed and reaped when it goes out of scope.
///
/// Killed rather than asked. A micro-VM has no graceful shutdown this end can request —
/// the agent's answer to `quit` is to stop reading, and what would carry a request after
/// that is the thing being torn down. Everything the guest wrote is in the session's
/// image, which is deleted right after this.
struct Vmm(Child);

impl Drop for Vmm {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The socket a boot child connects back on, and the path it occupies.
struct Socket {
    path: PathBuf,
    listener: UnixListener,
}

impl Socket {
    fn bind() -> anyhow::Result<Socket> {
        // Under `/tmp` rather than `$TMPDIR`: `sockaddr_un.sun_path` is 104 bytes on
        // macOS and a per-user temp directory is most of that on its own.
        let path = assets::unique(PathBuf::from("/tmp"), "channel.sock");
        // A live pid cannot have left one behind, so anything here outlived a process
        // whose pid has been reused.
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path).map_err(|e| {
            anyhow::anyhow!("binding the console channel at {}: {e}", path.display())
        })?;
        Ok(Socket { path, listener })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read the agent's greeting, which is the first thing on the channel and not a frame.
async fn greeting(incoming: &mut BufReader<OwnedReadHalf>) -> anyhow::Result<()> {
    let mut greeting = [0u8; HANDSHAKE.len()];
    incoming.read_exact(&mut greeting).await.map_err(|e| {
        anyhow::anyhow!("the guest closed the channel before it said anything: {e}")
    })?;
    anyhow::ensure!(
        &greeting == HANDSHAKE,
        "the guest opened with {greeting:?}, which is not this protocol"
    );
    Ok(())
}

/// A copy of this process's stderr, as something a child can be given for its stdout.
fn stderr() -> io::Result<OwnedFd> {
    // SAFETY: `dup` returns a fresh descriptor for the same open file, and the `OwnedFd`
    // built from it is the only owner of that number.
    let fd = unsafe { libc::dup(libc::STDERR_FILENO) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
