//! `Console`, its builder, and what its calls answer with.
//!
//! Every call that waits returns a `Promise`, settled on the tokio runtime napi keeps: a
//! stdio client spawns its server and reads from it, and both need a reactor under them.
//!
//! # Why not `async fn`
//!
//! napi's `async fn` and `spawn_future` reject with a [`Status`](napi::Status), which
//! becomes the error's `code` — and the codes a caller acts on are cortex's (`TIMED_OUT`,
//! `CONSOLE_BROKEN`), not napi's. So [`promise`] runs the future to a plain `Result` and
//! turns an `Err` into the JavaScript error once it is back on the main thread, where one
//! can be made. It also means the synchronous part of a call happens when the call is made:
//! `build()` takes the builder then, not whenever the runtime gets to the future.
//!
//! # Where the console lives
//!
//! A promise's future has to be `'static`, so it cannot borrow the object it was started
//! from. The console is therefore behind an `Arc<Mutex<..>>` that each call clones into its
//! future — which also makes calls take turns, as `&mut self` does in Rust: one channel
//! carries one call at a time, and a second `exec` started alongside the first waits for it
//! rather than interleaving with it.
//!
//! # How it ends
//!
//! [`Console`]'s `Drop` says `quit` on whatever runtime it is dropped on, and says nothing
//! off one — which is where a garbage-collection finalizer runs. So [`JsConsole`] keeps the
//! handle of the runtime the console was built on and enters it before letting go, and a
//! console that is collected ends the same way as one that is closed. `close()` is there for
//! a caller who wants that moment to be a line in their program rather than whenever the
//! collector gets to it.
//!
//! # Who else holds it
//!
//! The slot is an `Arc<Mutex<Option<Console>>>` rather than a type of this module's own,
//! because that is the shape an agent holds its console in — ailoy's `AgentState::console` —
//! and a binding that links this crate hands [`JsConsole::slot`] to one. The two then share
//! one session: calls from either side take turns on the lock, and `close()` ends it for both.
//! Whichever lets go last ends it, so a holder other than this one owes the same runtime
//! entry on drop — [`JsConsole::runtime`] is the handle to enter.

use std::{future::Future, sync::Arc};

use cortex::console::{Console, ConsoleBuilder, ExecResp, NetworkAccess, ReadResp};
use napi::{
    Env, JsError,
    bindgen_prelude::{Buffer, PromiseRaw, This, ToNapiValue},
};
use napi_derive::napi;
use tokio::{runtime::Handle, sync::Mutex};

use crate::{
    error::{self, Result, unsigned},
    fs::{Content, MountLike, bytes, into_mount},
    image::JsImage,
};

/// Run `fut` on napi's runtime, rejecting with the error it answers — `code` and all.
///
/// Public for a binding that links this crate, whose errors want the same treatment.
pub fn promise<'env, T, F>(env: &'env Env, fut: F) -> napi::Result<PromiseRaw<'env, T>>
where
    T: ToNapiValue + Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    env.spawn_future_with_callback(async move { Ok(fut.await) }, |env, result| {
        result.map_err(|e| napi::Error::from(JsError::from(e).into_unknown(*env)))
    })
}

/// What a synchronous error of ours is on a path that answers in napi's own.
pub fn thrown(env: &Env, error: napi::Error<String>) -> napi::Error {
    napi::Error::from(JsError::from(error).into_unknown(*env))
}

#[napi(js_name = "NetworkAccess")]
#[derive(Clone)]
pub struct JsNetworkAccess(NetworkAccess);

#[napi]
impl JsNetworkAccess {
    #[napi(constructor)]
    pub fn new(reach: String) -> Self {
        JsNetworkAccess(NetworkAccess::new(reach))
    }

    #[napi(factory)]
    pub fn none() -> Self {
        JsNetworkAccess(NetworkAccess::none())
    }

    #[napi(factory)]
    pub fn host() -> Self {
        JsNetworkAccess(NetworkAccess::host())
    }

    #[napi(factory)]
    pub fn public() -> Self {
        JsNetworkAccess(NetworkAccess::public())
    }

    #[napi(factory)]
    pub fn full() -> Self {
        JsNetworkAccess(NetworkAccess::full())
    }

    #[napi]
    pub fn with_host_ports(&self, ports: Vec<u16>) -> JsNetworkAccess {
        JsNetworkAccess(self.0.clone().with_host_ports(ports))
    }

    #[napi(getter)]
    pub fn reach(&self) -> String {
        self.0.reach.clone()
    }

    #[napi(getter)]
    pub fn host_ports(&self) -> Vec<u16> {
        self.0.host_ports.clone()
    }
}

/// A [`ConsoleBuilder`], filled in place and emptied by `build()`.
///
/// In place rather than by value, unlike `Image`: the Rust builder is consumed by each call
/// and is not `Clone`, so there is exactly one of it to hand along. Each method returns the
/// same object so calls chain as they do in Rust.
#[napi(js_name = "ConsoleBuilder")]
pub struct JsConsoleBuilder(Option<ConsoleBuilder>);

impl JsConsoleBuilder {
    fn update<'env>(
        &mut self,
        this: This<'env>,
        f: impl FnOnce(ConsoleBuilder) -> Result<ConsoleBuilder>,
    ) -> Result<This<'env>> {
        let builder = self.0.take().ok_or_else(built)?;
        self.0 = Some(f(builder)?);
        Ok(this)
    }
}

fn built() -> napi::Error<String> {
    error::invalid("this ConsoleBuilder has already been built")
}

#[napi]
impl JsConsoleBuilder {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        JsConsoleBuilder(Some(ConsoleBuilder::new()))
    }

    #[napi]
    pub fn stdio_client<'env>(&mut self, this: This<'env>, cmd: Vec<String>) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.stdio_client(&cmd)))
    }

    #[napi]
    pub fn mount<'env>(
        &mut self,
        this: This<'env>,
        #[napi(ts_arg_type = "HostMount | string")] mount: MountLike,
        at: String,
    ) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.mount(into_mount(mount)?, at)))
    }

    #[napi]
    pub fn mount_readonly<'env>(
        &mut self,
        this: This<'env>,
        #[napi(ts_arg_type = "HostMount | string")] mount: MountLike,
        at: String,
    ) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.mount_readonly(into_mount(mount)?, at)))
    }

    #[napi]
    pub fn image<'env>(
        &mut self,
        this: This<'env>,
        #[napi(ts_arg_type = "Image")] image: &JsImage,
    ) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.image(image.0.clone())))
    }

    #[napi]
    pub fn snapshot<'env>(&mut self, this: This<'env>, snapshot: Buffer) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.snapshot(snapshot.to_vec())))
    }

    #[napi]
    pub fn network<'env>(
        &mut self,
        this: This<'env>,
        #[napi(ts_arg_type = "NetworkAccess")] network: &JsNetworkAccess,
    ) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.network(network.0.clone())))
    }

    #[napi]
    pub fn vcpus<'env>(&mut self, this: This<'env>, vcpus: u8) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.vcpus(vcpus)))
    }

    #[napi]
    pub fn memory_mib<'env>(&mut self, this: This<'env>, memory_mib: u32) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.memory_mib(memory_mib)))
    }

    #[napi]
    pub fn gpu<'env>(&mut self, this: This<'env>, gpu: bool) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.gpu(gpu)))
    }

    #[napi]
    pub fn gpu_memory_mib<'env>(
        &mut self,
        this: This<'env>,
        gpu_memory_mib: u32,
    ) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.gpu_memory_mib(gpu_memory_mib)))
    }

    #[napi]
    pub fn disk_gib<'env>(&mut self, this: This<'env>, disk_gib: u32) -> Result<This<'env>> {
        self.update(this, |b| Ok(b.disk_gib(disk_gib)))
    }

    /// Announce the session, and settle with the `Console` the server answered.
    #[napi(ts_return_type = "Promise<Console>")]
    pub fn build<'env>(&mut self, env: &'env Env) -> napi::Result<PromiseRaw<'env, JsConsole>> {
        let builder = self
            .0
            .take()
            .ok_or_else(built)
            .map_err(|e| thrown(env, e))?;
        promise(env, async move {
            let console = builder.build().await.map_err(error::anyhow)?;
            Ok(JsConsole::new(console))
        })
    }
}

/// The console a slot holds, or the error for one that has been closed.
fn held(slot: &mut Option<Console>) -> Result<&mut Console> {
    slot.as_mut().ok_or_else(|| {
        napi::Error::new("CORTEX_ERROR".to_string(), "this console has been closed")
    })
}

/// A console slot: the console, or nothing once it has been closed.
pub type Slot = Arc<Mutex<Option<Console>>>;

#[napi(js_name = "Console")]
pub struct JsConsole {
    console: Slot,

    /// The runtime the console was built on, which is where its `quit` has to go out.
    runtime: Handle,

    /// [`Console::mounts`], read once: the paths are fixed when the session is announced, and
    /// a getter that had to await the lock for them would make them a promise.
    mounts: Vec<String>,
}

impl JsConsole {
    /// Built inside the future `build()` runs, so the current runtime is the one to keep.
    fn new(console: Console) -> Self {
        JsConsole {
            mounts: console
                .mounts()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            console: Arc::new(Mutex::new(Some(console))),
            runtime: Handle::current(),
        }
    }

    /// The slot this console is held in, for a holder that shares it — see the module docs.
    pub fn slot(&self) -> Slot {
        self.console.clone()
    }

    /// The runtime a holder of [`slot`](Self::slot) enters to let go of it.
    pub fn runtime(&self) -> Handle {
        self.runtime.clone()
    }
}

/// What makes dropping the last holder say `quit`: the console is let go of on the runtime.
/// A holder that is not the last leaves it to whichever is.
impl Drop for JsConsole {
    fn drop(&mut self) {
        if let Some(slot) = Arc::get_mut(&mut self.console) {
            let _entered = self.runtime.enter();
            slot.get_mut().take();
        }
    }
}

#[napi(object)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: Buffer,
    pub stderr: Buffer,
    pub truncated: bool,
}

impl From<ExecResp> for ExecResult {
    fn from(resp: ExecResp) -> Self {
        ExecResult {
            code: resp.code,
            stdout: resp.stdout.into(),
            stderr: resp.stderr.into(),
            truncated: resp.truncated,
        }
    }
}

#[napi(object)]
pub struct ReadResult {
    pub data: Buffer,
    /// A JavaScript number, so exact up to 2^53 bytes.
    pub size: i64,
}

impl From<ReadResp> for ReadResult {
    fn from(resp: ReadResp) -> Self {
        ReadResult {
            data: resp.data.into(),
            size: resp.size as i64,
        }
    }
}

#[napi]
impl JsConsole {
    #[napi]
    pub fn builder() -> JsConsoleBuilder {
        JsConsoleBuilder::new()
    }

    #[napi(getter)]
    pub fn mounts(&self) -> Vec<String> {
        self.mounts.clone()
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn start<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, ()>> {
        let console = self.console.clone();
        promise(env, async move {
            let mut slot = console.lock().await;
            held(&mut slot)?.start().await.map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn stop<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, ()>> {
        let console = self.console.clone();
        promise(env, async move {
            let mut slot = console.lock().await;
            held(&mut slot)?.stop().await.map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<ExecResult>")]
    pub fn exec<'env>(
        &self,
        env: &'env Env,
        cmd: Vec<String>,
        timeout_ms: Option<i64>,
    ) -> napi::Result<PromiseRaw<'env, ExecResult>> {
        let console = self.console.clone();
        promise(env, async move {
            let timeout_ms = unsigned(timeout_ms, "timeoutMs")?;
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.exec(cmd, timeout_ms).await;
            resp.map(ExecResult::from).map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<ReadResult>")]
    pub fn read<'env>(
        &self,
        env: &'env Env,
        path: String,
        offset: Option<i64>,
        len: Option<i64>,
    ) -> napi::Result<PromiseRaw<'env, ReadResult>> {
        let console = self.console.clone();
        promise(env, async move {
            let (offset, len) = (unsigned(offset, "offset")?, unsigned(len, "len")?);
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.read(path, offset, len).await;
            resp.map(ReadResult::from).map_err(error::failure)
        })
    }

    /// Put `data` in a file, settling with the file's size afterwards.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn write<'env>(
        &self,
        env: &'env Env,
        path: String,
        #[napi(ts_arg_type = "Buffer | string")] data: Content,
        offset: Option<i64>,
    ) -> napi::Result<PromiseRaw<'env, i64>> {
        let console = self.console.clone();
        let data = bytes(data);
        promise(env, async move {
            let offset = unsigned(offset, "offset")?;
            let mut slot = console.lock().await;
            let resp = held(&mut slot)?.write(path, data, offset).await;
            resp.map(|w| w.size as i64).map_err(error::failure)
        })
    }

    #[napi(ts_return_type = "Promise<Buffer>")]
    pub fn snapshot<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, Buffer>> {
        let console = self.console.clone();
        promise(env, async move {
            let mut slot = console.lock().await;
            let blob = held(&mut slot)?.snapshot().await;
            blob.map(Buffer::from).map_err(error::failure)
        })
    }

    /// End the session now. Closing twice is the same as closing once.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn close<'env>(&self, env: &'env Env) -> napi::Result<PromiseRaw<'env, ()>> {
        let console = self.console.clone();
        promise(env, async move {
            // Dropped here, on the runtime, which is what lets `quit` go out.
            console.lock().await.take();
            Ok(())
        })
    }
}
