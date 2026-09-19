//! A micro-VM, booted on an image built here.
//!
//! Two things start one, and they differ in how long it lives rather than in what it does.
//! A session boots once and runs whatever its client asks for until the client is done; a
//! build's `RUN` wants one command and the filesystem it left behind. So the long-lived one
//! is a value, and the short one is a function over it.
//!
//! Either way the guest is `cortex-uvm-v2-guest`, which libkrun execs as the first userspace
//! process — in `boot` mode for a session, `exec` mode for a build step.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use std::convert::Infallible;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use anyhow::Context as _;
use microsandbox_image::ext4::{Ext4FormatOptions, format_ext4};
use microsandbox_network::network::SmoltcpNetwork;
use microsandbox_network::policy::{
    Action, Destination, DestinationGroup, Direction, NetworkPolicy, PortRange, Protocol, Rule,
};
use msb_krun::backends::net::NetBackend;
use msb_krun::{DiskImageFormat, VmBuilder};

use cortex::console::stdio::{read, write};
use cortex::console::{Call, CommitCall, ExecCall, Message, Response};

use crate::contract::{
    ABIN_ENV, ARTIFACTS_ENV, ARTIFACTS_PATH, ARTIFACTS_TAG, COMMIT_ENV, COMMIT_PATH, COMMIT_TAG,
    COMMITTABLE_ENV, CONTEXT_ENV, CONTEXT_PATH, CONTEXT_TAG, GUEST_ABIN_DEV, GUEST_BIN_PATH,
    GUEST_LOWER_DEV, GUEST_UPPER_DEV, HANDSHAKE, IMAGE_SPEC_PATH, ImageSpec, LAYER_TAR, LOWER_ENV,
    Network, PORT_NAME, SCRATCH_ENV, SCRATCH_PATH, SCRATCH_TAG, UPPER_ENV,
};

/// The guest half, cross-compiled and embedded by `build.rs`. Written into every boot root,
/// which is why the guest crate optimises for size.
const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cortex-uvm-v2-guest"));
use crate::rootfs::{Digest, Image, home};

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
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    // The executable a boot runs is never this process: entitlements are read at `exec`,
    // so they are on a file before it starts or they are nowhere. And a copy rather than
    // this file itself, because signing a binary while it runs would leave the running
    // process unsigned against a file that no longer matches it.
    //
    // Named by the bytes it holds, so a rebuild signs again and two boots of one build
    // share the copy.
    let exe = std::env::current_exe()?;
    let bytes =
        std::fs::read(&exe).map_err(|e| anyhow::anyhow!("reading {}: {e}", exe.display()))?;
    let name = Digest::of(&bytes).file_stem()[..16].to_string();

    let cache = home().join("boot");
    std::fs::create_dir_all(&cache)?;
    let machine = cache.join(&name);
    if !machine.is_file() {
        // Written, made executable and signed at a path nothing else will pick, then
        // renamed into place — two boots racing here both do the work and the rename
        // decides which copy survives, where sharing one path would mean signing a file
        // the other was still writing.
        let part = cache.join(format!("{name}.{}.part", std::process::id()));
        std::fs::write(&part, &bytes)?;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;

        #[cfg(target_os = "macos")]
        if let Err(e) = crate::session::entitlement::sign(&part) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }

        std::fs::rename(&part, &machine)?;
    }

    // Not downloaded. A kernel is loaded into the guest's memory by a `dlopen`ed library
    // that has to be signed compatibly with the process loading it, so where it came from
    // is a property of the installation rather than something this decides — and getting
    // it wrong fails deep inside the VMM, well after the point where a helpful message is
    // easy to give.
    let kernel = match std::env::var_os("CORTEX_UVM_KERNEL") {
        Some(named) => PathBuf::from(named),
        None => {
            // Both spellings of the name: the versioned one a release ships, and the
            // plain one a package manager symlinks.
            let (versioned, plain) = match std::env::consts::OS {
                "macos" => ("libkrunfw.5.dylib", "libkrunfw.dylib"),
                _ => ("libkrunfw.so.5", "libkrunfw.so"),
            };
            let mut looked = Vec::new();
            if let Some(user) = std::env::var_os("HOME") {
                looked.push(PathBuf::from(&user).join(".microsandbox/lib"));
            }
            looked.extend(
                ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]
                    .into_iter()
                    .map(PathBuf::from),
            );
            looked
                .into_iter()
                .flat_map(|dir| [dir.join(versioned), dir.join(plain)])
                .find(|at| at.exists())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no libkrunfw found. Install it (`brew install libkrunfw`, or \
                         microsandbox) or set CORTEX_UVM_KERNEL to the library's path"
                    )
                })?
        }
    };

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
    let (lower, upper, root, socket, commit, console) = (
        mine("lower"),
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
        lower.with_extension("vmdk"),
        lower.with_extension("fsmeta.erofs"),
    ];

    // The base, as one disk. The layers behind it stay where they are — this writes the
    // merged metadata and a descriptor naming them, and nothing else.
    let base = image.disk(&lower)?;

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
        kernel,
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
        network: Network::Disabled,
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
        .arg("boot")
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
    pub async fn boot(
        image: &Image,
        mounts: &[Mount],
        network: Network,
    ) -> anyhow::Result<Uvm> {
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
    pub async fn commit(&mut self) -> anyhow::Result<Digest> {
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

        let written = home()
            .join("tmp")
            .join(format!("{}.erofs", std::process::id()));
        microsandbox_image::erofs::write_erofs(&tree, &written)
            .map_err(|e| anyhow::anyhow!("writing this session's layer: {e:?}"))?;
        let digest = Digest::of_file(&written)?;
        let kept = home()
            .join("blobs")
            .join(format!("{}.erofs", digest.file_stem()));
        if kept.is_file() {
            std::fs::remove_file(&written)?;
        } else {
            std::fs::rename(&written, &kept)?;
        }
        Ok(digest)
    }
}

/// Everything a console server tells a boot, as the boot's own command line.
///
/// # Why arguments and not the environment
///
/// The two halves of this file are two channels, and only one of them is constrained. What a
/// boot tells the *guest* has to be environment, because libkrun puts an exec's argv and env
/// alike on the kernel command line. What a *server* tells a boot is an ordinary
/// `Command::spawn`, and there was never a reason for it to be ambient.
///
/// **Ambient was the problem.** A child inherits its parent's environment, so a name the server
/// wrote for the boot was also a name an operator could set on the server, and one of them came
/// to mean two things. An argument cannot be inherited: everything here is either passed or
/// absent, and absent is a parse error rather than a silently different session.
///
/// The other half of it is that this is one type with one `to_args` and one `parse`, so a value
/// added to one side is a value the other side stops compiling without. The constants this
/// replaced were shared names read in two places, which is a weaker promise.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BootArgs {
    /// The libkrunfw kernel the child boots.
    kernel: PathBuf,

    /// A directory served as the guest's virtio-fs root, holding the guest binary and the
    /// image spec and nothing else.
    boot_root: PathBuf,

    /// Where the child connects to reach the server. One socket, one connection, and its
    /// descriptor becomes the guest's console port.
    channel: PathBuf,

    /// The read-only base image, as a host path, and how the VMM has to read it. The two travel
    /// together because they are one fact: what the server provisioned.
    base: PathBuf,
    base_format: BaseFormat,

    /// The session's writable image, as a host path.
    session: PathBuf,

    /// Whether this session may commit, which decides one thing at boot and nothing after:
    /// the old root is kept rather than detached, so the guest can still reach the upperdir of
    /// the overlay it is standing on.
    committable: bool,

    /// A directory on this host the guest may write a commit's layer into, shared in at
    /// [`COMMIT_PATH`]. `None` for a session that cannot commit.
    ///
    /// Writable, unlike the `/abin` disk, and that is not a contradiction: `/abin` is a cache
    /// every later session reads, where this is one session's scratch — made by the server,
    /// thrown away with the session, and holding nothing but that session's own output.
    commit_out: Option<PathBuf>,

    /// A read-only image of native executables to mount at `/abin`.
    ///
    /// `None` is a session that gets none. Always a raw EROFS and never a descriptor, so
    /// there is no format to say alongside it: `/abin` is cortex's own executables and no
    /// others, which is one layer attached as it stands rather than anything stitched.
    abin: Option<PathBuf>,

    /// The host directory to put in front of the guest, and `None` for a session that declared
    /// no tree. Mounted in the guest at [`CONTEXT_PATH`], which is where it is named from
    /// there on — see [`CONTEXT_ENV`].
    context: Option<PathBuf>,

    /// Where the session leaves what it produces, and `None` for a session that named none.
    /// Shared exactly as the context is, and mounted at [`ARTIFACTS_PATH`].
    artifacts: Option<PathBuf>,

    /// Room for the session to work in, and `None` for a session that named none. Shared as
    /// the two above are, mounted at [`SCRATCH_PATH`], and additionally **where the guest
    /// stands** — see [`SCRATCH_ENV`].
    scratch: Option<PathBuf>,

    /// How much of a network the session gets. Decided by the server, because it decides a
    /// *device*, which is attached before a kernel comes up.
    /// Where the guest's own console goes — kernel messages, a panic, whatever a boot that
    /// failed has to say. A file rather than this process's stderr, so that a machine which
    /// never connected can be asked what happened.
    console: Option<PathBuf>,

    network: Network,

    /// TCP ports on the host this session may open, beyond whatever [`network`](Self::network)
    /// allows.
    ///
    /// # Why a grant of its own and not a wider [`Network`]
    ///
    /// Because the convenient way to say "the host" is `DestinationGroup::Host` with no ports on
    /// it, and a rule with no ports matches **every** port. Spelling host reach that way would
    /// mean a session that asked to talk to one service on this machine could talk to all of
    /// them — and, worse, that widening a session's *outside* reach silently widened its inside
    /// one.
    ///
    /// So the two are separate axes. `network` says how far out a session goes; this says which
    /// doors on this machine are open to it, one port at a time, TCP only. A session gets both,
    /// and neither implies the other.
    ///
    /// Meaningless without a device, so [`Network::Disabled`] with ports named is a
    /// contradiction the server refuses rather than a grant it quietly drops.
    host_ports: Vec<u16>,

    /// Guest vCPUs and memory, when the server was told to override them.
    vcpus: Option<u8>,
    memory_mib: Option<u32>,
}

impl BootArgs {
    /// The arguments a server spawns a boot with, after [`BOOT_ARG`](crate::server::BOOT_ARG).
    ///
    /// `OsString` throughout: a path that is not UTF-8 is still a path, and nothing here has to
    /// read one as text. The exceptions are the three trees, which the guest is told about as
    /// strings, and that is refused where it is used rather than here.
    fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();

        // First, before the closure below takes its borrow of `args`. This is the one flag
        // that carries no value — it is a fact about the session rather than a thing with a
        // spelling — so it cannot go through `put`, and where it sits does not matter to
        // `parse`.
        if self.committable {
            args.push(OsString::from("--committable"));
        }

        let mut put = |flag: &str, value: &OsStr| {
            args.push(OsString::from(flag));
            args.push(value.to_os_string());
        };

        put("--kernel", self.kernel.as_os_str());
        put("--boot-root", self.boot_root.as_os_str());
        put("--channel", self.channel.as_os_str());
        put("--base", self.base.as_os_str());
        put("--base-format", OsStr::new(self.base_format.as_str()));
        put("--session", self.session.as_os_str());
        put("--network", OsStr::new(self.network.as_str()));
        if let Some(console) = &self.console {
            put("--console", console.as_os_str());
        }
        for port in &self.host_ports {
            put("--host-port", OsStr::new(&port.to_string()));
        }
        if let Some(context) = &self.context {
            put("--context", context.as_os_str());
        }
        if let Some(artifacts) = &self.artifacts {
            put("--artifacts", artifacts.as_os_str());
        }
        if let Some(scratch) = &self.scratch {
            put("--scratch", scratch.as_os_str());
        }
        if let Some(abin) = &self.abin {
            put("--abin", abin.as_os_str());
        }
        if let Some(out) = &self.commit_out {
            put("--commit-out", out.as_os_str());
        }
        if let Some(vcpus) = self.vcpus {
            put("--vcpus", OsStr::new(&vcpus.to_string()));
        }
        if let Some(memory) = self.memory_mib {
            put("--memory-mib", OsStr::new(&memory.to_string()));
        }
        args
    }

    /// Read them back, refusing anything this does not recognise.
    ///
    /// A boot is started by a console server and by nothing else, so a missing argument is a
    /// mismatch between two halves of one binary rather than a caller to be lenient with. It is
    /// refused before a hypervisor is touched, which is the cheapest place it can be.
    fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<BootArgs> {
        let mut kernel = None;
        let mut boot_root = None;
        let mut channel = None;
        let mut base = None;
        let mut base_format = None;
        let mut session = None;
        let mut network = None;
        let mut host_ports = Vec::new();
        let mut console = None;
        let mut context = None;
        let mut artifacts = None;
        let mut scratch = None;
        let mut abin = None;
        let mut committable = false;
        let mut commit_out = None;
        let mut vcpus = None;
        let mut memory_mib = None;

        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let flag = flag
                .into_string()
                .map_err(|flag| anyhow::anyhow!("{flag:?} is not an argument this reads"))?;
            let mut value = || {
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("{flag} was given nothing to be"))
            };
            match flag.as_str() {
                "--console" => console = Some(PathBuf::from(value()?)),
                "--kernel" => kernel = Some(PathBuf::from(value()?)),
                "--boot-root" => boot_root = Some(PathBuf::from(value()?)),
                "--channel" => channel = Some(PathBuf::from(value()?)),
                "--base" => base = Some(PathBuf::from(value()?)),
                "--base-format" => base_format = Some(text(&flag, value()?)?),
                "--session" => session = Some(PathBuf::from(value()?)),
                "--network" => network = Some(text(&flag, value()?)?),
                "--host-port" => host_ports.push(number(&flag, value()?)?),
                "--context" => context = Some(PathBuf::from(value()?)),
                "--artifacts" => artifacts = Some(PathBuf::from(value()?)),
                "--scratch" => scratch = Some(PathBuf::from(value()?)),
                "--abin" => abin = Some(PathBuf::from(value()?)),
                "--committable" => committable = true,
                "--commit-out" => commit_out = Some(PathBuf::from(value()?)),
                "--vcpus" => vcpus = Some(number(&flag, value()?)?),
                "--memory-mib" => memory_mib = Some(number(&flag, value()?)?),
                other => anyhow::bail!("{other} is not an argument a boot takes"),
            }
        }

        let required = |name: &str, value: Option<PathBuf>| {
            value
                .ok_or_else(|| anyhow::anyhow!("{name} is missing — a boot is started by a server"))
        };
        Ok(BootArgs {
            kernel: required("--kernel", kernel)?,
            boot_root: required("--boot-root", boot_root)?,
            channel: required("--channel", channel)?,
            base: required("--base", base)?,
            base_format: match base_format {
                Some(spelling) => BaseFormat::parse(&spelling)?,
                None => anyhow::bail!("--base-format is missing — a boot is started by a server"),
            },
            session: required("--session", session)?,
            network: match network {
                Some(name) => Network::parse(Some(&name))?,
                None => anyhow::bail!("--network is missing — a boot is started by a server"),
            },
            console,
            host_ports,
            context,
            artifacts,
            scratch,
            committable,
            commit_out,
            abin,
            vcpus,
            memory_mib,
        })
    }
}

/// An argument's value as text, for the few that are not paths.
fn text(flag: &str, value: OsString) -> anyhow::Result<String> {
    value
        .into_string()
        .map_err(|value| anyhow::anyhow!("{flag} is not utf-8: {value:?}"))
}

/// An argument's value as a number. Refused rather than defaulted: a server computed it, so a
/// value that is not one is a bug on this side of the process boundary.
fn number<T: std::str::FromStr>(flag: &str, value: OsString) -> anyhow::Result<T> {
    text(flag, value)?
        .parse()
        .map_err(|_| anyhow::anyhow!("{flag} is not a number"))
}

/// How the read-only base image is laid out on the host — the value of [`BASE_FORMAT_ENV`],

/// and what a boot turns into a disk format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BaseFormat {
    /// A single raw image: what a rootfs tarball is encoded to.
    #[default]
    Raw,
    /// A VMDK descriptor stitching per-layer blobs into one disk: what a registry pull's
    /// layered materialization produces.
    Vmdk,
}

impl BaseFormat {
    fn as_str(self) -> &'static str {
        match self {
            BaseFormat::Raw => "raw",
            BaseFormat::Vmdk => "vmdk",
        }
    }

    /// Parse the environment's spelling. An unrecognised one is an error rather than the
    /// default, which would fail at the guest's mount instead — several seconds and a kernel
    /// log away from the mistake.
    fn parse(spelling: &str) -> anyhow::Result<BaseFormat> {
        match spelling {
            "raw" => Ok(BaseFormat::Raw),
            "vmdk" => Ok(BaseFormat::Vmdk),
            other => anyhow::bail!("--base-format: {other} is not `raw` or `vmdk`"),
        }
    }
}

/// Where a boot writes [`ImageSpec`] in the boot root, and the path the guest reads it from.
///
/// A file rather than another environment value, because the environment here *is* the
/// kernel command line: libkrun passes it as `KRUN_ENV=…`, which is size-limited and cannot
/// carry a space, and an image's `ENV` is neither short nor free of them. The boot root is
/// already shared over virtio-fs, so a file in it costs nothing — and the guest reads it

/// Hold a hypervisor: parse what the session that spawned this copy said, and enter the VM.
///
/// Never returns on success — `Vm::enter` is where this process goes.
pub fn boot(argv: impl IntoIterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    match enter(BootArgs::parse(argv)?)? {}
}

/// Guest vCPUs when nothing says otherwise. Two rather than one because a command and the
/// agent collecting its output are two things wanting the processor at once, and one vCPU
/// turns that into a queue.
const DEFAULT_VCPUS: u8 = 2;

/// Guest memory in MiB when nothing says otherwise. Enough for a package install, which is
/// the first thing anyone does in a sandbox.
const DEFAULT_MEMORY_MIB: u32 = 2048;

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

fn enter(args: BootArgs) -> anyhow::Result<Infallible> {
    // The console port is a descriptor, and this is where it comes from: one connection back
    // to the process that spawned this one. Held for the length of this function, which is
    // the length of the process.
    let channel = UnixStream::connect(&args.channel)
        .map_err(|e| anyhow::anyhow!("connecting to the console channel: {e}"))?;
    let port = channel.as_raw_fd();

    let lower_format = match args.base_format {
        BaseFormat::Raw => DiskImageFormat::Raw,
        BaseFormat::Vmdk => DiskImageFormat::Vmdk,
    };
    let mut builder = VmBuilder::new()
        .machine(|m| {
            m.vcpus(args.vcpus.unwrap_or(DEFAULT_VCPUS))
                .memory_mib(args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB) as usize)
        })
        .kernel(|k| k.krunfw_path(&args.kernel))
        .fs(|fs| fs.root(&args.boot_root))
        // Attach order is what fixes the names: the session's own image is `/dev/vda` and the
        // base is `/dev/vdb`, which is the promise the guest mounts against.
        .disk(|d| d.path(&args.session).format(DiskImageFormat::Raw))
        .disk(|d| d.path(&args.base).read_only(true).format(lower_format))
        // The same descriptor both ways: a socket is bidirectional, and the port the guest
        // opens is one thing rather than a pair. The console beside it is the guest's own,
        // and goes to a file so that a boot nobody could talk to can still be read.
        .console(|c| {
            let c = match &args.console {
                Some(at) => c.output(at),
                None => c,
            };
            c.port(PORT_NAME, port, port)
        });

    // `/abin`, third and so `/dev/vdc`. Read-only **at the device**, which is the whole reason
    // it is a disk and not a share: a virtio-fs share has no such option, and the guest is
    // root inside itself, so a guest-side mount flag would be a guard rail rather than a
    // boundary.
    if let Some(abin) = &args.abin {
        builder = builder.disk(|d| d.path(abin).read_only(true).format(DiskImageFormat::Raw));
    }

    // The network, when the session asked for one. The stack, its runtime and the policy it
    // enforces all live in this process, and all three have to outlive `enter` below — which
    // never returns, so the guard is held to the end of the function that does not end.
    let mut stack = match args.network {
        Network::Disabled => None,
        reach => Some(stack(reach, &args.host_ports)?),
    };
    if let Some(stack) = &mut stack {
        let (mac, backend) = stack.device();
        builder = builder.net(move |n| n.mac(mac).custom(backend));
    }

    // One device per tree the session named, each under its own tag, and all three the same
    // work: this has no opinion about what a tree is for, and the guest is told which is
    // which by the name it arrives under.
    let mut shares: Vec<(&str, String)> = Vec::new();
    for (flag, env, tag, at, path) in [
        (
            "--context",
            CONTEXT_ENV,
            CONTEXT_TAG,
            CONTEXT_PATH,
            args.context.as_deref(),
        ),
        (
            "--artifacts",
            ARTIFACTS_ENV,
            ARTIFACTS_TAG,
            ARTIFACTS_PATH,
            args.artifacts.as_deref(),
        ),
        (
            "--scratch",
            SCRATCH_ENV,
            SCRATCH_TAG,
            SCRATCH_PATH,
            args.scratch.as_deref(),
        ),
    ] {
        let Some(path) = path else { continue };
        // UTF-8 because it has to be written into that tree's env as `tag:path` and read back
        // by the guest — a directory with no string form is one the two ends could not agree
        // on, and this is the last place that can say so.
        anyhow::ensure!(
            path.is_absolute(),
            "{flag} has to be an absolute path, and is {}",
            path.display()
        );
        let path: &str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{flag} is not utf-8: {}", path.display()))?;

        builder = builder.fs(|fs| fs.tag(tag).path(Path::new(path)));
        shares.push((env, format!("{tag}:{at}")));
    }

    // A committable session's scratch. Shared rather than sent back over the channel: a layer
    // can be hundreds of megabytes, and that channel is what the protocol itself runs on.
    if let Some(out) = &args.commit_out {
        builder = builder.fs(|fs| fs.tag(COMMIT_TAG).path(out));
    }

    // What the stack wants the guest to know: its address, its gateway, its resolver. Passed
    // through as the stack spelled them.
    let guest_net = stack.as_ref().map(|s| s.guest_env()).unwrap_or_default();

    let vm = builder
        .exec(|e| {
            let e = e
                .path(GUEST_BIN_PATH)
                .env(LOWER_ENV, GUEST_LOWER_DEV)
                .env(UPPER_ENV, GUEST_UPPER_DEV);
            let e = shares.iter().fold(e, |e, (env, share)| e.env(*env, share));
            let e = match &args.abin {
                Some(_) => e.env(ABIN_ENV, GUEST_ABIN_DEV),
                None => e,
            };
            // Two values and not one: the first is read before `pivot_root` and the second
            // after, and a guest with only the second could write a layer it had no way to
            // see.
            let e = if args.committable {
                e.env(COMMITTABLE_ENV, "1")
            } else {
                e
            };
            let e = match &args.commit_out {
                Some(_) => e.env(COMMIT_ENV, COMMIT_PATH),
                None => e,
            };
            guest_net
                .iter()
                .fold(e, |e, (name, value)| e.env(name, value))
        })
        .build()?;

    Ok(vm.enter()?)
}

/// A running stack, and everything whose lifetime is the VM's.
///
/// The runtime is in here because dropping it would stop the poll loop the stack is driven by,
/// and `Vm::enter` never returns — so what holds this holds it until the process is gone.
struct Stack {
    /// Declared first so the poll loop is shut down before the runtime under it goes.
    stack: SmoltcpNetwork,
    _runtime: tokio::runtime::Runtime,
}

/// Bring a stack up under the policy `reach` and `host_ports` describe.
///
/// Never called for [`Network::Disabled`], which attaches no device at all: a policy that
/// refuses everything and a guest with no interface are not the same thing, and the second is
/// the one worth having when nothing was asked for.
fn stack(reach: Network, host_ports: &[u16]) -> anyhow::Result<Stack> {
    debug_assert!(reach != Network::Disabled);

    // Everything but the policy left as the stack's own default: the addresses, the MTU, the
    // DNS timeouts. This process has an opinion about what a sandbox may reach and none about
    // how the stack goes about it.
    let config = microsandbox_network::config::NetworkConfig {
        policy: policy(reach, host_ports),
        ..Default::default()
    };

    // Its own runtime rather than a handle from somewhere: this process has no other async
    // work, and the stack's poll loop wants threads that are not competing with a VM's vCPUs
    // for a place to run.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("a runtime for the network stack")?;

    // The slot decides which addresses the stack hands out, so several sandboxes on one host
    // do not share a subnet. One VM per boot process, so this one is always the first.
    let mut stack = SmoltcpNetwork::new(config, 0)
        .map_err(|e| anyhow::anyhow!("building the network stack: {e:?}"))?;
    stack.start(runtime.handle().clone());

    Ok(Stack {
        stack,
        _runtime: runtime,
    })
}

impl Stack {
    /// The MAC and the backend `VmBuilder::net` wants, which the stack assigned itself.
    ///
    /// Taken rather than borrowed: the device is handed to the VMM, and there is one of it.
    fn device(&mut self) -> ([u8; 6], Box<dyn NetBackend + Send>) {
        (self.stack.guest_mac(), self.stack.take_backend())
    }

    /// What the guest is told, as the stack spells it — `MSB_NET`, `MSB_NET_IPV4` and the rest.
    ///
    /// Passed on rather than translated. These are `microsandbox-network`'s own names for its
    /// own numbers, and the guest is the end that applies them: a boot that rewrote them into a
    /// spelling of its own would be a third party to an agreement between two.
    fn guest_env(&self) -> Vec<(String, String)> {
        self.stack.guest_env_vars()
    }
}

/// The policy a reach and a grant mean.
///
/// Deny by default in both directions, with the allowances added on top.
///
/// # Two axes, and why the host is the narrow one
///
/// `reach` says how far *out* a session goes; `host_ports` says which doors on **this machine**
/// are open to it. Neither implies the other, and that is deliberate: the convenient way to
/// spell host reach is `DestinationGroup::Host` with no ports on it, and a rule with no ports
/// matches every port. Written that way, a session granted one service on this machine would
/// have them all — and widening the outside would silently widen the inside.
///
/// So the host rules are built one port at a time, TCP only. A connection the guest opens to the
/// gateway is rewritten to the host's loopback when the stack dials it, which is what makes a
/// granted port an actual door onto whatever the operator is running there.
///
/// # `allow_dns` comes first and is load-bearing
///
/// It is the narrow rule (UDP and TCP :53, to the gateway only) that lets a name be resolved at
/// all. Under deny-by-default a policy without it refuses every lookup, and a caller who asked
/// for the public internet gets a sandbox that can open a connection to an address it has no way
/// to learn.
///
/// # What resolution costs
///
/// The stack forwards queries upstream, to whatever the host's own `/etc/resolv.conf` names. So
/// any reach that can resolve can also put bytes into a hostname and watch them leave. **The
/// only air gap here is [`Network::Disabled`]**, which attaches no device.
fn policy(reach: Network, host_ports: &[u16]) -> NetworkPolicy {
    if matches!(reach, Network::Full) {
        return NetworkPolicy::allow_all();
    }

    let mut rules = vec![Rule::allow_dns()];
    if matches!(reach, Network::Public) {
        rules.push(Rule::allow_egress(Destination::Group(
            DestinationGroup::Public,
        )));
    }
    rules.extend(host_ports.iter().map(|&port| Rule {
        direction: Direction::Egress,
        destination: Destination::Group(DestinationGroup::Host),
        protocols: vec![Protocol::Tcp],
        ports: vec![PortRange::single(port)],
        action: Action::Allow,
    }));

    NetworkPolicy {
        default_egress: Action::Deny,
        default_ingress: Action::Deny,
        rules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> BootArgs {
        BootArgs {
            kernel: "/opt/lib/libkrunfw.dylib".into(),
            boot_root: "/tmp/boot-root".into(),
            channel: "/tmp/sock".into(),
            base: "/cache/oci/vmdk/sha256_7e6269.vmdk".into(),
            base_format: BaseFormat::Vmdk,
            session: "/tmp/session.ext4".into(),
            console: Some("/tmp/console.log".into()),
            network: Network::Public,
            host_ports: vec![8080, 3000],
            context: Some("/Users/someone/project".into()),
            artifacts: Some("/Users/someone/out".into()),
            scratch: Some("/Users/someone/scratch".into()),
            abin: Some("/cache/layers/sha256_2b91c4.erofs".into()),
            committable: true,
            commit_out: Some("/tmp/cortex-uvm-commit".into()),
            vcpus: Some(4),
            memory_mib: Some(8192),
        }
    }

    /// A committable session says both things, and an ordinary one says neither.
    ///
    /// The pair matters. A kept root with nowhere to write is a guest that can see what the
    /// session wrote and cannot hand it over; somewhere to write with no kept root is the
    /// reverse.
    #[test]
    fn a_committable_boot_says_so_and_names_somewhere_to_write() {
        let args = args();
        let back = BootArgs::parse(args.to_args()).unwrap();
        assert!(back.committable);
        assert_eq!(back.commit_out, args.commit_out);
    }

    #[test]
    fn an_ordinary_boot_says_neither() {
        let plain = BootArgs {
            committable: false,
            commit_out: None,
            ..args()
        };
        let written = plain.to_args();
        assert!(
            !written
                .iter()
                .any(|arg| arg == "--committable" || arg == "--commit-out"),
            "{written:?}"
        );
        let back = BootArgs::parse(written).unwrap();
        assert!(!back.committable);
        assert_eq!(back.commit_out, None);
    }

    /// A session with no `/abin` says nothing about one, and reads back as having none.
    #[test]
    fn a_session_with_no_abin_says_nothing_about_one() {
        let bare = BootArgs {
            abin: None,
            ..args()
        };
        let written = bare.to_args();
        assert!(!written.iter().any(|arg| arg == "--abin"), "{written:?}");
        let back = BootArgs::parse(written).unwrap();
        assert_eq!(back.abin, None);
    }

    /// Everything this writes is something it reads. Both ends are this type, so the compiler
    /// catches a field added to one side — what it cannot catch is a field written under one
    /// name and read under another, which is what this is for.
    #[test]
    fn boot_arguments_round_trip() {
        let sent = args();
        assert_eq!(
            BootArgs::parse(sent.to_args()).expect("its own spelling"),
            sent
        );
    }

    /// The optional ones are absent rather than empty, and come back absent.
    ///
    /// A session with no tree is the case that matters: an empty `--context` would be a
    /// directory named by the empty string, and the boot would try to share it. The same goes
    /// for the two trees beside it, which are shared by exactly the same mechanism.
    #[test]
    fn what_was_not_said_is_not_sent() {
        let bare = BootArgs {
            context: None,
            artifacts: None,
            scratch: None,
            vcpus: None,
            memory_mib: None,
            ..args()
        };
        let spelled = bare.to_args();
        for flag in [
            "--context",
            "--artifacts",
            "--scratch",
            "--vcpus",
            "--memory-mib",
        ] {
            assert!(
                !spelled.iter().any(|arg| arg == flag),
                "{flag} was sent anyway"
            );
        }
        assert_eq!(BootArgs::parse(spelled).expect("its own spelling"), bare);
    }

    /// A boot is started by a console server and by nothing else, so anything missing or
    /// unrecognised is two halves of one binary disagreeing. Refused before a hypervisor is
    /// touched, which is the cheapest place it can be.
    #[test]
    fn a_boot_refuses_arguments_a_server_would_not_have_sent() {
        let mut short = args().to_args();
        short.truncate(2); // just `--kernel <path>`
        assert!(
            BootArgs::parse(short).is_err(),
            "a missing argument was accepted"
        );

        let mut unknown = args().to_args();
        unknown.push("--turbo".into());
        assert!(
            BootArgs::parse(unknown).is_err(),
            "an unknown argument was accepted"
        );

        let mut dangling = args().to_args();
        dangling.push("--vcpus".into());
        assert!(
            BootArgs::parse(dangling).is_err(),
            "a flag with no value was accepted"
        );

        let mut nonsense = args();
        nonsense.vcpus = None;
        let mut spelled = nonsense.to_args();
        spelled.extend(["--vcpus".into(), "several".into()]);
        assert!(
            BootArgs::parse(spelled).is_err(),
            "a vcpu count that is not one was accepted"
        );
    }

    /// Every spelling this writes is one it reads. The two halves are a boot apart — a server
    /// writes the name and a child in another process parses it — so a variant added with one
    /// of them updated fails at a mount, in a kernel log, seconds later.
    #[test]
    fn a_base_format_round_trips_through_its_name() {
        for format in [BaseFormat::Raw, BaseFormat::Vmdk] {
            assert_eq!(
                BaseFormat::parse(format.as_str()).expect("its own name"),
                format
            );
        }
    }

    #[test]
    fn an_unknown_base_format_is_refused() {
        assert!(BaseFormat::parse("qcow2").is_err());
        assert!(BaseFormat::parse("").is_err());
    }

    /// The same promise for a reach: a server writes the name and a binary in another process
    /// parses it, so a variant added with only one of them updated is a session that quietly
    /// gets a reach nobody asked for.
    #[test]
    fn a_reach_round_trips_through_its_name() {
        for reach in [
            Network::Disabled,
            Network::HostOnly,
            Network::Public,
            Network::Full,
        ] {
            assert_eq!(
                Network::parse(Some(reach.as_str())).expect("its own name"),
                reach
            );
        }
    }

    /// Silence is the default, and a misspelling is neither answer. `on`, `true` and `1` are all
    /// things a caller might type and none of them says how much reach is wanted.
    #[test]
    fn a_reach_that_is_not_one_is_refused() {
        assert_eq!(Network::parse(None).expect("unset"), Network::default());
        assert_eq!(Network::parse(Some("")).expect("empty"), Network::default());
        for typo in ["hsot", "on", "true", "1", "HOST"] {
            assert!(Network::parse(Some(typo)).is_err(), "{typo} was accepted");
        }
    }

    /// A grant round trips, and silence is no grant. Same reasoning as the reach above: the two
    /// halves are a boot apart, so a list that spelled one way and read another is a session
    /// whose doors are not the ones anybody named.
    #[test]
    fn a_host_port_grant_round_trips_and_defaults_to_nothing() {
        for ports in [vec![], vec![8080], vec![8080, 3000, 65535]] {
            let granted = BootArgs {
                host_ports: ports.clone(),
                ..args()
            };
            assert_eq!(
                BootArgs::parse(granted.to_args())
                    .expect("its own spelling")
                    .host_ports,
                ports,
            );
        }

        // No grant is no flag, which is what an empty list has to spell: `--host-port` with
        // nothing after it would be a port named by the empty string.
        let none = BootArgs {
            host_ports: Vec::new(),
            ..args()
        };
        assert!(!none.to_args().iter().any(|arg| arg == "--host-port"));

        // A port is a number and nothing else — 65536 is not one, and neither is a name.
        for typo in ["http", "65536", "-1"] {
            let mut spelled = none.to_args();
            spelled.extend(["--host-port".into(), typo.into()]);
            assert!(BootArgs::parse(spelled).is_err(), "{typo} was accepted");
        }
    }
}
