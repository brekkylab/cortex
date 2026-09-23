//! Which console server a console runs — see [`Backend`].

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use cortex_console_embed::Embedded;
use tokio::process::Command;

use crate::{
    console::{base::Client, stdio::StdioClient},
    exe::{self, Installed},
};

/// Which console server a [`Console`](crate::console::Console) runs, named rather than located.
///
/// A caller says *what kind* of somewhere its commands run — this host, a micro-VM — and this
/// crate supplies the server: it is built with this crate and carried inside whatever binary
/// links it, so there is no program to install, find, or keep at the same version as the
/// library. Each backend is a cargo feature, and one that is off does not exist here at all:
///
/// | feature | built from | server |
/// |---|---|---|
/// | `local` | `Backend::local()` | `cortex-local-console` — commands run on this host |
/// | `uvm` | `Backend::uvm()` | `cortex-uvm-console` — commands run in a micro-VM |
///
/// and what either builds is handed to
/// [`ConsoleBuilder::backend`](crate::console::ConsoleBuilder::backend). `local` is a default
/// feature, and a console that names no backend runs on `Backend::local()` as it comes.
///
/// # Where the server comes from
///
/// In order, the first that is there:
///
/// 1. `program(path)`, named by the caller.
/// 2. `CORTEX_LOCAL_CONSOLE_BIN` or `CORTEX_UVM_CONSOLE_BIN`, named by whoever runs it.
/// 3. The one this crate carries, written out under `<home>/bin` the first time it is needed —
///    `home(path)`, else `$CORTEX_HOME`, else `~/.cortex`.
///
/// `PATH` is not searched: a server found there is one whose version nothing ties to this
/// library's.
///
/// # What a server is told
///
/// Its environment is this process's, with what the backend's options say on top — a server
/// reads its configuration from its environment, and these options are the typed spelling of
/// it. So a variable a caller exported still reaches it, and an option set here wins over one.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Backend {
    /// Commands run on this host.
    #[cfg(feature = "local")]
    Local(LocalBackend),
    /// Commands run in a micro-VM.
    #[cfg(feature = "uvm")]
    Uvm(UvmBackend),
}

impl Backend {
    /// A server whose commands run on this host, in the host's own filesystem.
    #[cfg(feature = "local")]
    pub fn local() -> LocalBackend {
        LocalBackend::default()
    }

    /// A server whose commands run in a Linux micro-VM, on a kernel of its own.
    ///
    /// Needs a hypervisor — Hypervisor.framework on macOS, KVM on Linux — and a libkrunfw,
    /// which the server fetches the first time it needs one unless
    /// [`kernel`](UvmBackend::kernel) names it.
    #[cfg(feature = "uvm")]
    pub fn uvm() -> UvmBackend {
        UvmBackend::default()
    }

    /// Resolve the server and start it. Blocking: it may write a program out.
    pub(crate) fn start(self) -> anyhow::Result<Box<dyn Client>> {
        let (embedded, override_env, common, extra): (_, _, _, Vec<(OsString, OsString)>) =
            match self {
                #[cfg(feature = "local")]
                Backend::Local(local) => (
                    cortex_console_embed::LOCAL,
                    "CORTEX_LOCAL_CONSOLE_BIN",
                    local.common,
                    Vec::new(),
                ),
                #[cfg(feature = "uvm")]
                Backend::Uvm(uvm) => {
                    let extra = uvm.server_env();
                    (
                        cortex_console_embed::UVM,
                        "CORTEX_UVM_CONSOLE_BIN",
                        uvm.common,
                        extra,
                    )
                }
            };

        let home = match &common.home {
            Some(home) => home.clone(),
            None => default_home()?,
        };

        // Held until the server has started, so that nothing removes the file in between —
        // see `exe`. Only ever `Some` for the program this crate carries: one a caller named
        // is theirs to keep in place.
        let mut installed: Option<Installed> = None;
        let program = match (&common.program, std::env::var_os(override_env)) {
            (Some(program), _) => program.clone(),
            (None, Some(program)) => PathBuf::from(program),
            (None, None) => {
                let at = write_out(&embedded, &home.join("bin"))
                    .with_context(|| format!("writing out {}", embedded.name))?;
                let path = at.path().to_path_buf();
                installed = Some(at);
                path
            }
        };

        let mut server = Command::new(&program);
        if common.home.is_some() {
            // Both spellings: a `uvm` server reads the second, and the first is where anything
            // it starts would look.
            server
                .env("CORTEX_HOME", &home)
                .env("CORTEX_UVM_HOME", home.join("uvm"));
        }
        server.envs(extra);
        server.envs(common.env);

        let client = StdioClient::new(server)
            .with_context(|| format!("starting the console server {}", program.display()))?;
        drop(installed);
        Ok(Box::new(client))
    }
}

/// What every backend takes.
#[derive(Debug, Clone, Default)]
struct Common {
    program: Option<PathBuf>,
    home: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}

/// The setters every backend has, documented once.
macro_rules! common_setters {
    () => {
        /// Run this program as the server instead of the one this crate carries.
        ///
        /// For a server built some other way — a debug build of it, one under a debugger. It
        /// has to speak the protocol this crate does, which nothing here checks: the one this
        /// crate carries is the one that is guaranteed to.
        pub fn program(mut self, program: impl Into<PathBuf>) -> Self {
            self.common.program = Some(program.into());
            self
        }

        /// The directory cortex keeps what it writes in, instead of `$CORTEX_HOME` or
        /// `~/.cortex`.
        ///
        /// The server this crate carries is written under `<home>/bin`, and a `uvm` server
        /// keeps its images, layers and kernel under `<home>/uvm`. An application that keeps
        /// its data somewhere of its own points this there.
        pub fn home(mut self, home: impl Into<PathBuf>) -> Self {
            self.common.home = Some(home.into());
            self
        }

        /// Set a variable in the server's environment, on top of everything else it is given.
        ///
        /// The server's environment is this process's to begin with, so this is only for
        /// something that should reach the server and not this process — including a variable
        /// the typed options here would otherwise set, which this then overrides.
        pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
            self.common.env.push((key.into(), value.into()));
            self
        }
    };
}

/// A server that runs commands on this host — see [`Backend::local`].
#[cfg(feature = "local")]
#[derive(Debug, Clone, Default)]
pub struct LocalBackend {
    common: Common,
}

#[cfg(feature = "local")]
impl LocalBackend {
    common_setters!();
}

#[cfg(feature = "local")]
impl From<LocalBackend> for Backend {
    fn from(local: LocalBackend) -> Backend {
        Backend::Local(local)
    }
}

/// A server that runs commands in a micro-VM — see [`Backend::uvm`].
#[cfg(feature = "uvm")]
#[derive(Debug, Clone, Default)]
pub struct UvmBackend {
    common: Common,
    vcpus: Option<u8>,
    memory_mib: Option<u32>,
    kernel: Option<PathBuf>,
}

#[cfg(feature = "uvm")]
impl UvmBackend {
    common_setters!();

    /// How many vCPUs the guest gets, instead of the boot's own default.
    pub fn vcpus(mut self, vcpus: u8) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    /// How much memory the guest gets, in MiB, instead of the boot's own default.
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    /// The libkrunfw to boot, instead of the one the server fetches and keeps.
    pub fn kernel(mut self, kernel: impl Into<PathBuf>) -> Self {
        self.kernel = Some(kernel.into());
        self
    }

    /// The options, as the server reads them.
    fn server_env(&self) -> Vec<(OsString, OsString)> {
        let mut env = Vec::new();
        if let Some(vcpus) = self.vcpus {
            env.push(("CORTEX_UVM_VCPUS".into(), vcpus.to_string().into()));
        }
        if let Some(memory_mib) = self.memory_mib {
            env.push((
                "CORTEX_UVM_MEMORY_MIB".into(),
                memory_mib.to_string().into(),
            ));
        }
        if let Some(kernel) = &self.kernel {
            env.push(("CORTEX_UVM_KERNEL".into(), kernel.clone().into_os_string()));
        }
        env
    }
}

#[cfg(feature = "uvm")]
impl From<UvmBackend> for Backend {
    fn from(uvm: UvmBackend) -> Backend {
        Backend::Uvm(uvm)
    }
}

/// `$CORTEX_HOME`, else `$HOME/.cortex`.
fn default_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("CORTEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cortex")))
        .context("neither CORTEX_HOME nor HOME is set, so there is nowhere to write the server")
}

/// Write `embedded` out into `dir`, unless it is there already.
fn write_out(embedded: &Embedded, dir: &Path) -> io::Result<Installed> {
    // Sixteen hex digits of the digest: a name, not a check. The check is below, on the bytes.
    exe::install(dir, embedded.name, &embedded.digest[..16], |path| {
        decompress(embedded, path)
    })
}

/// Decompress `embedded` to `path`, and refuse bytes that do not hash to what was built.
///
/// The check costs a few tens of milliseconds, once per machine per build, and what it guards
/// against is a program that would otherwise be run: a decoder bug, a truncated write.
fn decompress(embedded: &Embedded, path: &Path) -> io::Result<()> {
    use sha2::{Digest as _, Sha256};
    use std::io::Write as _;

    let mut decoder = ruzstd::decoding::StreamingDecoder::new(embedded.zstd)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    /// Hashes what it writes, so the program is read once.
    struct Hashing<W> {
        inner: W,
        hasher: Sha256,
        len: u64,
    }
    impl<W: io::Write> io::Write for Hashing<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = self.inner.write(buf)?;
            self.hasher.update(&buf[..n]);
            self.len += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    let mut out = Hashing {
        inner: io::BufWriter::new(std::fs::File::create(path)?),
        hasher: Sha256::new(),
        len: 0,
    };
    io::copy(&mut decoder, &mut out)?;
    out.flush()?;

    let digest = format!("{:x}", out.hasher.finalize());
    if out.len != embedded.len || digest != embedded.digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} decompressed to {} bytes hashing to {digest}, not the {} bytes hashing to {} \
                 it was built as",
                embedded.name, out.len, embedded.len, embedded.digest
            ),
        ));
    }
    out.inner
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()
}
