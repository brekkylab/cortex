//! The public end: a console server to run commands in, over one channel.
//!
//! [`ConsoleClientBuilder`] describes the session; [`ConsoleClient`] runs commands in it.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use futures_core::future::BoxFuture;
use tokio::process::Command;

use crate::{
    default_server,
    fs::Mount,
    image::ImageSource,
    protocol::{
        Call, Client, ExecCall, ExecResp, Failure, InitCall, MountSpec, Notification, Port,
        ReadCall, ReadResp, Response, WriteCall, WriteResp, stdio::StdioClient,
    },
};

/// Whatever it takes to have a channel, deferred until [`build`](ConsoleClientBuilder::build).
///
/// [`Send`] so a builder can be built inside a spawned task.
pub(crate) type ClientFactory = Box<dyn FnOnce() -> anyhow::Result<Box<dyn Client>> + Send>;

/// Assembles a [`ConsoleClient`] from the parts it needs.
///
/// Every part is optional. Nothing starts until [`build`](Self::build).
///
/// The machine's shape ([`vcpus`](Self::vcpus), [`memory_mib`](Self::memory_mib),
/// [`gpu`](Self::gpu), [`gpu_memory_mib`](Self::gpu_memory_mib), [`disk_gib`](Self::disk_gib))
/// is given exactly or refused: one the server cannot give fails [`build`](Self::build) with
/// [`UNSUPPORTED_MACHINE`](crate::protocol::Error::UNSUPPORTED_MACHINE), never a smaller
/// machine or the CPU. Left out, the server picks.
pub struct ConsoleClientBuilder {
    /// A factory, not a client, because starting a program can fail; deferring that to
    /// [`build`](Self::build) keeps every setter infallible.
    ///
    /// `None` is the default server, `virtx-uvm` under the cache's `bin` directory, which
    /// [`build`](Self::build) fetches first if this host has none (the `ensure` feature).
    client_factory: Option<ClientFactory>,

    /// `None` leaves it to the server: a host backend needs none, and a VM backend refuses
    /// rather than guess a base.
    image: Option<ImageSource>,

    /// What a previous session wrote; `None` starts from scratch.
    snapshot: Option<Vec<u8>>,

    /// Every tree this console gives the session, in mount order. Empty means commands see
    /// only the server's own filesystem.
    mounts: Vec<Mounted>,

    /// On unless turned off.
    network: bool,

    /// Ports on the server's machine that lead into the session, in the order named.
    ports: Vec<Port>,

    vcpus: Option<u8>,

    memory_mib: Option<u32>,

    gpu: Option<bool>,

    gpu_memory_mib: Option<u32>,

    disk_gib: Option<u32>,
}

impl Default for ConsoleClientBuilder {
    fn default() -> Self {
        ConsoleClientBuilder {
            client_factory: None,
            image: None,
            snapshot: None,
            mounts: Vec::new(),
            network: true,
            ports: Vec::new(),
            vcpus: None,
            memory_mib: None,
            gpu: None,
            gpu_memory_mib: None,
            disk_gib: None,
        }
    }
}

impl ConsoleClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive the server over `client`.
    ///
    /// Any [`Client`]: a [`StdioClient`] over a server it
    /// started, a virtio port into a guest, or both ends in one process for a test. The
    /// client owns whatever the channel needs, including a process.
    pub fn client(mut self, client: impl Client + 'static) -> Self {
        self.client_factory = Some(Box::new(move || Ok(Box::new(client))));
        self
    }

    /// Drive a server this console starts itself: `cmd`, over its own pipes.
    ///
    /// `cmd` is a program and its arguments: `["virtx-local-console"]`,
    /// `["sh", "-c", "…"]`. For an environment or directory, build the [`Command`], pass it
    /// to [`StdioClient::new`], and hand the result to [`client`](Self::client).
    ///
    /// Started by [`build`](Self::build), which fails if it cannot be (including an empty
    /// `cmd`).
    pub fn cmd(mut self, cmd: &[impl AsRef<OsStr>]) -> Self {
        self.client_factory = Some(stdio_factory(cmd));
        self
    }

    /// Give the session a tree, mounted at `at`.
    ///
    /// `at` is where commands see the tree, and the path [`read`](ConsoleClient::read) and
    /// [`write`](ConsoleClient::write) join onto. It must be absolute, and the mount point must
    /// be expressible as a URL (see [`Mount::url`]), or [`build`](Self::build) fails.
    ///
    /// The mount is held by value so the tree stays up for the whole session (see
    /// [`Mount`]); pass an `Arc<..>` to share it. A plain [`PathBuf`] is a [`Mount`] for an
    /// existing host directory, with nothing to put up or take down.
    ///
    /// **Order is kept**: mount a nested tree after the tree containing it.
    ///
    /// Writable; see [`mount_readonly`](Self::mount_readonly).
    pub fn mount(mut self, mount: impl Mount + 'static, at: impl Into<PathBuf>) -> Self {
        self.mounts.push(Mounted {
            mount: Box::new(mount),
            at: at.into(),
            readonly: false,
        });
        self
    }

    /// Give the session a tree it may read and not write, at `at`.
    ///
    /// Otherwise as [`mount`](Self::mount). A [`write`](ConsoleClient::write) under it is
    /// refused, and a VM backend mounts it read-only so commands cannot write there either.
    pub fn mount_readonly(mut self, mount: impl Mount + 'static, at: impl Into<PathBuf>) -> Self {
        self.mounts.push(Mounted {
            mount: Box::new(mount),
            at: at.into(),
            readonly: true,
        });
        self
    }

    /// The base the session's commands run in.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # use virtx::image::Recipe;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .image(
    ///         Recipe::new("alpine:3.20")
    ///             .step("apk add --no-cache jq"),
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn image(mut self, image: impl Into<ImageSource>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Start this session on what a previous one wrote, rather than from scratch.
    pub fn snapshot(mut self, snapshot: Vec<u8>) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// Whether the session's commands reach a network at all; on unless turned off.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder().network(true).build().await?;
    /// # Ok(()) }
    /// ```
    ///
    /// **The server gives exactly this or refuses:** [`build`](Self::build) fails with
    /// [`UNSUPPORTED_NETWORK`](crate::protocol::Error::UNSUPPORTED_NETWORK). A host backend
    /// cannot take the network away, so it refuses `false`. What "on" reaches is on
    /// [`InitCall::network`](crate::protocol::InitCall::network).
    pub fn network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    /// Ports on the server's machine that lead into the session, as docker's `-p` spells
    /// them (`"8080:80"`, host first). Replaces what an earlier call said.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// // A VNC server in the session, at 127.0.0.1:5901 here.
    /// let console = ConsoleClient::builder()
    ///     .network(true)
    ///     .ports(["5901:5900".parse()?])
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Ports with [`network`](Self::network) off are refused. See [`Port`] for what a
    /// connection to one reaches.
    pub fn ports(mut self, ports: impl IntoIterator<Item = Port>) -> Self {
        self.ports = ports.into_iter().collect();
        self
    }

    /// How many vCPUs the session's machine gets.
    pub fn vcpus(mut self, vcpus: u8) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    /// How much memory the session's machine gets, in mebibytes.
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .vcpus(4)
    ///     .memory_mib(4096)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    /// Whether the session's commands get a GPU; `false` forbids one.
    ///
    /// **The image must bring the guest's half of Vulkan**: a loader (`libvulkan.so.1`) and
    /// the venus driver that reaches the host's GPU. The server attaches the device but cannot
    /// install libraries; without the loader nothing finds a device, and without venus Vulkan
    /// falls back to a CPU renderer.
    ///
    /// | | Alpine | Debian |
    /// |---|---|---|
    /// | loader | `vulkan-loader` | `libvulkan1` |
    /// | venus driver | `mesa-vulkan-virtio` | `mesa-vulkan-drivers`, trixie or later |
    ///
    /// ```no_run
    /// # use virtx::{console::ConsoleClient, image::Recipe};
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .image(
    ///         Recipe::new("alpine:latest")
    ///             .step("apk add --no-cache vulkan-loader mesa-vulkan-virtio vulkan-tools"),
    ///     )
    ///     .gpu(true)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn gpu(mut self, gpu: bool) -> Self {
        self.gpu = Some(gpu);
        self
    }

    /// How much memory the session's GPU may hold, in MiB, in addition to
    /// [`memory_mib`](Self::memory_mib).
    ///
    /// Commands see a device of this size. Only valid with a GPU (see
    /// [`InitCall::gpu_memory_mib`](crate::console::InitCall::gpu_memory_mib)).
    ///
    /// ```no_run
    /// # use virtx::{console::ConsoleClient, image::Recipe};
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     // A loader and the venus driver -- see `gpu`.
    ///     .image(
    ///         Recipe::new("alpine:latest").step("apk add --no-cache vulkan-loader mesa-vulkan-virtio"),
    ///     )
    ///     .gpu(true)
    ///     .gpu_memory_mib(8192)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn gpu_memory_mib(mut self, gpu_memory_mib: u32) -> Self {
        self.gpu_memory_mib = Some(gpu_memory_mib);
        self
    }

    /// How much the session's commands may write, in GiB, on top of what the image ships.
    ///
    /// A ceiling, not an allocation (see
    /// [`InitCall::disk_gib`](crate::console::InitCall::disk_gib)).
    ///
    /// ```no_run
    /// # use virtx::console::ConsoleClient;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = ConsoleClient::builder()
    ///     .disk_gib(32)
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn disk_gib(mut self, disk_gib: u32) -> Self {
        self.disk_gib = Some(disk_gib);
        self
    }

    /// Opens the channel (starting the server process over stdio) and sends `init`.
    ///
    /// On the default server, first fetches it if this host has none (see
    /// [`ensure_virtx`](crate::ensure_virtx), with the `ensure` feature).
    ///
    /// Panics outside a Tokio runtime when it starts a process (the default, or
    /// [`cmd`](Self::cmd)), since the runtime reaps the child.
    ///
    /// The server may have to build an [`image`](Self::image) before answering `init`.
    pub async fn build(self) -> anyhow::Result<ConsoleClient> {
        ConsoleClient::new(self).await
    }
}

/// One of a session's trees: the mount this end holds, and where the session sees it.
struct Tree {
    /// Never read: holding it keeps the tree up for the session's lifetime (see [`Mount`]).
    #[allow(dead_code)]
    mount: Box<dyn Mount>,

    /// Where the session sees it, as `init` named it; not necessarily the host mount point.
    path: PathBuf,
}

/// A console: something to run commands in.
///
/// An [`exec`](Self::exec) per command, and drop it to end the session. The session was
/// described at build, booting happens under the first command that needs it, and
/// everything goes away with the console. [`start`](Self::start) and [`stop`](Self::stop)
/// are optional resource management.
///
/// Every method that reaches the server is one message, [`exec`](Self::exec) included; only
/// [`start`](Self::start) and [`stop`](Self::stop) go unanswered. One console is not
/// concurrent: its methods take `&mut self` (see [`Client`]).
///
/// ```no_run
/// use virtx::console::ConsoleClient;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// // Starts the server and sends `init`; nothing boots yet.
/// let mut console = ConsoleClient::builder()
///     .build()
///     .await?;
///
/// let result = console.exec(["sh", "-c", "echo hi"], None).await?;
/// assert_eq!(result.stdout, b"hi\n");
/// # Ok(())
/// # }
/// ```
pub struct ConsoleClient {
    /// Always present; ending replaces it rather than emptying it — see
    /// [`ConsoleClient::drop`](ConsoleClient#impl-Drop-for-ConsoleClient).
    client: Box<dyn Client>,

    /// Every tree this session was given, in the order named.
    mounts: Vec<Tree>,
}

impl ConsoleClient {
    pub fn builder() -> ConsoleClientBuilder {
        ConsoleClientBuilder::default()
    }

    /// Open a channel and send `init` on it.
    ///
    /// `init` is sent here, not by the caller, because there is no useful console between
    /// having a channel and describing the session: a `ConsoleClient` that exists is one the
    /// server has answered. Boots and mounts nothing.
    ///
    /// On failure the client is dropped without `quit`; over stdio the server sees its
    /// input close and exits.
    pub async fn new(builder: ConsoleClientBuilder) -> anyhow::Result<Self> {
        let ConsoleClientBuilder {
            client_factory,
            image,
            mounts,
            network,
            ports,
            snapshot,
            vcpus,
            memory_mib,
            gpu,
            gpu_memory_mib,
            disk_gib,
        } = builder;

        let client_factory = match client_factory {
            Some(factory) => factory,
            None => stdio_factory(&[default_server().await?]),
        };
        let mut client = client_factory()?;

        // Name every tree before sending anything: an unnameable mount is this end's own
        // mistake, not something to hear back from a server.
        let mut specs = Vec::with_capacity(mounts.len());
        let mut held = Vec::with_capacity(mounts.len());
        for mounted in mounts {
            specs.push(named(&mounted)?);
            held.push(Tree {
                mount: mounted.mount,
                path: mounted.at,
            });
        }

        let session = InitCall {
            mounts: specs,
            image,
            network,
            ports,
            snapshot,
            vcpus,
            memory_mib,
            gpu,
            gpu_memory_mib,
            disk_gib,
        };

        client.init(session).await?;

        Ok(ConsoleClient {
            client,
            mounts: held,
        })
    }

    /// Where each of this session's trees appears, in the order named.
    ///
    /// Join [`read`](Self::read) and [`write`](Self::write) paths onto these, not onto the
    /// host mount point, which differs on a guest backend. Empty when nothing is mounted.
    pub fn mounts(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.mounts.iter().map(|tree| tree.path.as_path())
    }

    /// Boot the far end now, to hide the cold start.
    ///
    /// Optional: [`exec`](Self::exec), [`read`](Self::read) and [`write`](Self::write) boot
    /// on demand. Sending this early lets the boot overlap with the caller's other work.
    ///
    /// `Ok` means the message went out, not that the boot succeeded; a failed boot surfaces
    /// as [`BOOT_FAILED`](crate::protocol::Error::BOOT_FAILED) on the next call that needs one.
    pub async fn start(&mut self) -> Result<(), Failure> {
        self.client.start().await
    }

    /// Release what booting took (guest memory, socket, scratch directory) while idle.
    ///
    /// Not an ending and not owed: dropping the console releases everything. The next
    /// command pays a cold start again, so use it before long idle stretches only.
    ///
    /// `Ok` means the message went out; nothing answers it.
    pub async fn stop(&mut self) -> Result<(), Failure> {
        self.client.stop().await
    }

    /// Run one command, and return everything it produced.
    ///
    /// The command is an argv (`["echo", "hi"]`) with no shell; for shell semantics send
    /// `["sh", "-c", ".."]`.
    ///
    /// It runs in the session's current directory, kept by the server; to run elsewhere,
    /// say so in the command (`sh -c 'cd there && ..'`).
    ///
    /// `timeout_ms` is the only way to bound it; `None` may run forever. Dropping this
    /// future instead leaves the command running and an answer pending on the channel.
    pub async fn exec(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecResp, Failure> {
        let exec = ExecCall {
            cmd: cmd.into_iter().map(|s| s.as_ref().to_string()).collect(),
            timeout_ms,
        };

        self.client.exec(exec).await
    }

    /// Read part of a file where commands run.
    ///
    /// The path is in the session's filesystem, under one of [`mounts`](Self::mounts): the
    /// file a command would open by that name.
    ///
    /// `offset` defaults to the beginning and `len` to the rest. A file too large for one
    /// message arrives in pieces; compare [`ReadResp::size`] with what arrived.
    pub async fn read(
        &mut self,
        path: impl AsRef<str>,
        offset: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadResp, Failure> {
        let read = ReadCall {
            path: path.as_ref().to_string(),
            offset,
            len,
        };
        self.client.read(read).await
    }

    /// Take everything this session has written, as a blob another session can start on.
    ///
    /// Pass the bytes to [`ConsoleClientBuilder::snapshot`] to carry the session on: a
    /// layer tar of the session's writes, kept as bytes by the caller.
    ///
    /// Cumulative since the session began, not since the last snapshot.
    pub async fn snapshot(&mut self) -> Result<Vec<u8>, Failure> {
        self.client.snapshot().await.map(|answer| answer.blob)
    }

    /// Put bytes in a file where commands run; returns its size afterwards.
    ///
    /// The path is under one of [`mounts`](Self::mounts), as for [`read`](Self::read).
    ///
    /// `None` makes the file exactly `data` (created or truncated); `Some(0)` writes the
    /// same bytes but keeps whatever lay past them.
    pub async fn write(
        &mut self,
        path: impl AsRef<str>,
        data: impl Into<Vec<u8>>,
        offset: Option<u64>,
    ) -> Result<WriteResp, Failure> {
        let write = WriteCall {
            path: path.as_ref().to_string(),
            data: data.into(),
            offset,
        };
        self.client.write(write).await
    }
}

impl Drop for ConsoleClient {
    /// Say `quit`, so the server exits gracefully; it also releases what a
    /// [`stop`](ConsoleClient::stop) would, so no `stop` is needed. The outcome (over stdio,
    /// the exit status) is discarded, since no caller is left to act on it.
    ///
    /// Owed exactly once, when the console goes away, so it is a `Drop` rather than a
    /// method to remember.
    ///
    /// The mounts go after the client: the server serves them until it hears the ending (a
    /// guest's virtio-fs share holds files open), and a binding's guard unmounts then waits
    /// for every holder, which dropped here would block the task's thread, deadlocking a
    /// current-thread runtime.
    fn drop(&mut self) {
        hang_up(&mut self.client, std::mem::take(&mut self.mounts));
    }
}

/// Start `cmd` as a console server, over its own pipes, once the factory is run.
pub(crate) fn stdio_factory(cmd: &[impl AsRef<OsStr>]) -> ClientFactory {
    // Owned, since the closure outlives this borrow.
    let cmd: Vec<OsString> = cmd.iter().map(|s| s.as_ref().to_owned()).collect();

    Box::new(move || {
        let (program, args) = cmd
            .split_first()
            .context("a console server needs a program to run")?;

        let mut server = Command::new(program);
        server.args(args);

        let client = StdioClient::new(server).context("starting the console server")?;
        Ok(Box::new(client))
    })
}

/// Say `quit` on a task, leaving a client behind that answers nothing, and drop `keep`
/// only after the client.
///
/// Swapping (not taking) lets a type with a destructor hand its client to a task, and keeps
/// the field a plain `Box` rather than an `Option` every method must check. Off a runtime,
/// or if the task never runs, the client is dropped without `quit`, closing the server's input.
///
/// `keep` is what the server may still be using (a console's mounts).
pub(crate) fn hang_up<K: Send + 'static>(client: &mut Box<dyn Client>, keep: K) {
    /// Stands in once the session has ended.
    struct Spent;

    impl Client for Spent {
        fn call(&mut self, _: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            Box::pin(async { Err(Failure::broken("the session has ended")) })
        }

        fn notify(&mut self, _: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            Box::pin(async { Err(Failure::broken("the session has ended")) })
        }
    }

    // Zero-sized, so boxing it allocates nothing.
    let client = std::mem::replace(client, Box::new(Spent));

    // A tuple drops its fields in order, so however it is dropped (off a runtime, as an
    // unpolled task, or at the task's end) the client goes before what it was serving.
    let ending = (client, keep);
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let mut ending = ending;
            let _ = ending.0.quit().await;
        });
    }
}

/// A tree the caller handed over, held by the builder until `init`.
struct Mounted {
    mount: Box<dyn Mount>,
    at: PathBuf,
    readonly: bool,
}

/// A mount this end holds, as the spec string the server is told to realize.
fn named(mounted: &Mounted) -> anyhow::Result<MountSpec> {
    let url = mounted.mount.url().with_context(|| {
        format!(
            "a mount point that is not an absolute UTF-8 path cannot be named: {:?}",
            mounted.mount.mountpoint()
        )
    })?;

    let at = mounted.at.to_str().with_context(|| {
        format!(
            "a tree can only be mounted at a UTF-8 path: {:?}",
            mounted.at
        )
    })?;

    // The error already names the spec.
    let spec = MountSpec::new(url, at)?;

    Ok(if mounted.readonly {
        spec.read_only()
    } else {
        spec
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_core::future::BoxFuture;

    use super::*;
    use crate::protocol::{Call, Error, InitResp, Method, Notification, Response};

    /// What a [`Recorder`] was handed, readable while it is lent out.
    ///
    /// `methods` is every message in order, notifications included; `calls` holds only the
    /// calls' contents.
    #[derive(Clone)]
    struct Log {
        methods: Arc<Mutex<Vec<Method>>>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl Log {
        fn methods(&self) -> Vec<Method> {
            self.methods.lock().unwrap().clone()
        }

        /// The `n`th call, counted over calls alone.
        fn call(&self, n: usize) -> Call {
            self.calls.lock().unwrap()[n].clone()
        }
    }

    /// A client over canned answers, recording everything it was handed.
    struct Recorder {
        answers: Vec<Response>,
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            self.log.methods.lock().unwrap().push(call.method());
            self.log.calls.lock().unwrap().push(call);
            let answer = if self.answers.is_empty() {
                // Not a panic, so tests need not script an answer for the ending.
                Err(Failure::broken("nothing left to answer with"))
            } else {
                match self.answers.remove(0) {
                    Response::Error(error) => Err(Failure::Refused(error)),
                    answer => Ok(answer),
                }
            };
            Box::pin(async move { answer })
        }

        fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            self.log.methods.lock().unwrap().push(notification.method());
            Box::pin(async { Ok(()) })
        }
    }

    fn recorder(answers: Vec<Response>) -> (Recorder, Log) {
        let log = Log {
            methods: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        (
            Recorder {
                answers,
                log: log.clone(),
            },
            log,
        )
    }

    /// A default `init` response.
    fn initialized() -> Response {
        Response::Init(InitResp::default())
    }

    /// A successful `exec` response.
    fn ran(stdout: &[u8]) -> Response {
        Response::Exec(ExecResp {
            code: 0,
            stdout: stdout.to_vec(),
            ..ExecResp::default()
        })
    }

    /// Every channel failure surfaces at build, saying which one it was.
    #[tokio::test]
    async fn a_stdio_console_starts_its_server_when_it_is_built() {
        let Err(e) = ConsoleClient::builder()
            .cmd(&["virtx-no-such-console"])
            .build()
            .await
        else {
            panic!("a console over a program that does not exist should not build");
        };
        assert!(e.to_string().contains("starting the console server"), "{e}");

        // An empty command also fails at build.
        let Err(e) = ConsoleClient::builder().cmd(&[] as &[&str]).build().await else {
            panic!("a console over no program at all should not build");
        };
        assert!(e.to_string().contains("needs a program to run"), "{e}");

        // So does a program that does not speak the protocol, since build awaits `init`:
        // `cat` echoes the request back, which a client cannot answer. Arguments reach the
        // program as given.
        let Err(e) = ConsoleClient::builder().cmd(&["cat", "-u"]).build().await else {
            panic!("a console over a program that cannot answer should not build");
        };
        assert!(e.to_string().contains("cannot answer"), "{e}");
    }

    /// After `init`, each caller method is exactly one message, in order.
    #[tokio::test]
    async fn a_session_is_the_servers_to_keep() {
        let (client, log) = recorder(vec![initialized(), ran(b"hi\n")]);
        let mut console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        // Optional and unanswered; only moves when the boot happens.
        console.start().await.unwrap();
        assert_eq!(
            console.exec(["echo", "hi"], None).await.unwrap().stdout,
            b"hi\n"
        );
        // Releases boot resources; the session stays open.
        console.stop().await.unwrap();

        // Dropping sends one `quit` and no extra `stop`.
        drop(console);
        tokio::task::yield_now().await;

        assert_eq!(
            log.methods(),
            [
                Method::Init,
                Method::Start,
                Method::Exec,
                Method::Stop,
                Method::Quit
            ]
        );

        // `init` goes first, and an unconfigured console sends a default one.
        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(init, InitCall::default());

        // The command goes out as written, with nothing added.
        let Call::Exec(exec) = log.call(1) else {
            panic!("{:?} is not an exec", log.call(1));
        };
        assert_eq!(
            exec,
            ExecCall {
                cmd: vec!["echo".into(), "hi".into()],
                timeout_ms: None,
            }
        );
    }

    /// A refused `init` yields no console, and so no `quit`.
    #[tokio::test]
    async fn a_console_the_server_will_not_have_does_not_exist() {
        let (client, log) = recorder(vec![Response::Error(Error::new(
            Error::UNSUPPORTED_MOUNT,
            "s3: this server realizes file:// and nothing else",
        ))]);
        let Err(e) = ConsoleClient::builder().client(client).build().await else {
            panic!("a console whose init was refused should not build");
        };
        assert!(e.to_string().contains("file://"), "{e}");

        // No `quit`: there is no `ConsoleClient` to owe one.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init]);
    }

    /// A never-started console still sends `quit` on drop: the server exists regardless of
    /// boot.
    #[tokio::test]
    async fn dropping_a_console_ends_its_session() {
        let (client, log) = recorder(vec![initialized()]);
        let console = ConsoleClient::builder()
            .client(client)
            .build()
            .await
            .unwrap();

        drop(console);

        // `quit` runs on a spawned task; let it.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init, Method::Quit]);
    }

    /// `init` names each mount as one spec string.
    ///
    /// Mount point and guest path differ on purpose, to show paths are spelled in the guest
    /// path.
    #[tokio::test]
    async fn a_console_names_its_mounts_and_where_it_put_them() {
        let (client, log) = recorder(vec![initialized()]);
        let console = ConsoleClient::builder()
            .client(client)
            .mount_readonly(PathBuf::from("/mnt/project"), "/work")
            .mount(PathBuf::from("/mnt/collected"), "/work/out")
            .build()
            .await
            .unwrap();

        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(
            init.mounts
                .iter()
                .map(MountSpec::to_string)
                .collect::<Vec<_>>(),
            [
                "file:///mnt/project:/work:ro",
                "file:///mnt/collected:/work/out"
            ]
        );

        // `mounts()` yields the guest paths, in order.
        assert_eq!(
            console.mounts().collect::<Vec<_>>(),
            [Path::new("/work"), Path::new("/work/out")]
        );
    }

    /// An unnameable mount fails the build before anything is sent.
    #[tokio::test]
    async fn a_mount_this_end_cannot_name_is_not_a_session() {
        let (client, log) = recorder(vec![initialized()]);
        let Err(e) = ConsoleClient::builder()
            .client(client)
            .mount(PathBuf::from("relative/here"), "/work")
            .build()
            .await
        else {
            panic!("a console over a mount point with no URL should not build");
        };
        assert!(e.to_string().contains("cannot be named"), "{e}");
        assert_eq!(log.methods(), [] as [Method; 0]);

        // A relative guest path fails the same way.
        let (client, _) = recorder(vec![initialized()]);
        let Err(e) = ConsoleClient::builder()
            .client(client)
            .mount(PathBuf::from("/mnt/here"), "work")
            .build()
            .await
        else {
            panic!("a console over a relative guest path should not build");
        };
        assert!(e.to_string().contains("guest path is absolute"), "{e}");
    }
}
