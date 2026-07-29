//! A drop-in `msb sandbox` process host that also serves cortex filesystem
//! backends.
//!
//! microsandbox's SDK launches the sandbox process named by `$MSB_PATH` as
//! `<bin> sandbox --config-fd N ...`. Point `MSB_PATH` at this binary and it
//! behaves exactly like `msb sandbox`, except it first registers cortex's
//! fs-backend factories — so a launch config carrying an
//! `FsBackendSpec { backend_type: "cortex-s3", .. }` is resolved into a live
//! virtio-fs mount during `build_vm`.
//!
//! Only the `sandbox` subcommand is implemented: that is the only thing
//! `MSB_PATH` is used to launch (everything else the SDK does in-process). This
//! mirrors the `Commands::Sandbox` arm of the real `msb` binary.
//!
//! Wiring it up (the minimal ailoy integration):
//!   1. Build this binary and set `MSB_PATH=/path/to/msb_cortex` in the process
//!      that drives the microsandbox SDK.
//!   2. Have the SDK add an `FsBackendSpec` to the launch config's `fs_backends`
//!      (see the ailoy `VolumeMount::FsBackend` scaffold).

use clap::{Parser, Subcommand};
use microsandbox_cli::{
    log_args,
    sandbox_cmd::{self, SandboxArgs},
    ui,
};
use microsandbox_runtime::logging::LogLevel;

#[derive(Parser)]
#[command(name = "msb-cortex", about = "msb sandbox host with cortex fs-backends")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sandbox process entry point — invoked by the SDK via `$MSB_PATH`.
    Sandbox(Box<SandboxArgs>),
}

fn main() {
    ui::install_panic_hook();

    // Point MSB_PATH at ourselves so any nested sandbox spawn re-invokes this
    // cortex-aware binary rather than a stock `msb`.
    // Safety: single-threaded at this point (before clap/tokio).
    if std::env::var("MSB_PATH").is_err() {
        if let Ok(exe) = std::env::current_exe() {
            unsafe { std::env::set_var("MSB_PATH", &exe) };
        }
    }

    // Register cortex fs-backends before dispatching, so the runtime can
    // resolve `fs_backends` specs of type "cortex-s3" when this process boots
    // the VM.
    cortex::msb::register_s3_backend();

    match Cli::parse().command {
        // Mirrors the real `msb` Sandbox arm: default to info-level tracing
        // (captured into runtime.log) with ANSI off, then hand off to the VMM.
        Command::Sandbox(args) => {
            let mut args = *args;
            let level = args.log_level.or(Some(LogLevel::Info));
            args.log_level = level;
            log_args::init_tracing(level, false);
            sandbox_cmd::run(args); // -> !
        }
    }
}
