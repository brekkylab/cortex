//! End-to-end over the real binary: a [`Console`] spawns it, it boots a micro-VM, and the
//! commands run in there.
//!
//! Which is the only way to test any of it. Nothing about this backend is exercised by
//! calling into it: what it does is start a process, which signs a copy of itself, which
//! brings up a kernel, which overlays two block devices, which execs a static binary,
//! which opens a virtio port that the first process is holding the other end of. The parts
//! are individually uninteresting and the composition is the whole thing.
//!
//! `#[ignore]`, because a boot is not a unit of work a test suite should do by default: it
//! needs libkrunfw installed, `codesign` on macOS, a hypervisor the OS will let this
//! process create, and — on a cold cache — a rootfs download.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test guest -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **`--test-threads=1`** is not a preference. Each test boots a VM with two vCPUs and two
//! gibibytes of address space; several at once is a machine deciding which of them to
//! swap.
//!
//! # Why there are few tests and each asserts a lot
//!
//! A boot is seconds. [`cortex-local-console`]'s suite is a test per property because a
//! test there costs a `symlink(2)`; here the same shape would be a minute of kernels coming
//! up to check things that share every line of code that could break. So each test below
//! is one session, and what it asserts is a sequence a client would actually perform.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

use std::process::Stdio;

use cortex::BoxFuture;
use cortex::console::{Console, ExecResult, ReadResult};
use cortex::executable::{ExecCall, ExecResult as ExecOutput, Executable, ExecutableSet};
use tokio::process::Command;

/// Reports everything it was told, so a round trip can be told from a coincidence.
struct Report;

impl Executable for Report {
    fn exec<'a>(&'a self, call: &'a ExecCall) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move { ExecOutput::ok(format!("{}|{}\n", call.name, call.args.join(","))) })
    }
}

/// A delegated name whose output is not text and not something a shell would survive
/// re-encoding.
struct RawBytes;

impl Executable for RawBytes {
    fn exec<'a>(&'a self, _call: &'a ExecCall) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move { ExecOutput::ok([0xff, 0xfe, 0x00, b'\n'].as_slice()) })
    }
}

/// A console over the real binary, with a session that delegates two names.
struct Fixture {
    console: Console,
}

impl Fixture {
    async fn new() -> Fixture {
        Fixture::with_env(&[]).await
    }

    /// `env` is what a caller would set to configure the backend — a workspace to project,
    /// a base image to use. It goes to the server process and not to this one: a console
    /// server is configured by its environment, which is the only thing about it a client
    /// does not say over the channel.
    async fn with_env(env: &[(&str, &str)]) -> Fixture {
        // A `Command` and not `stdio_client`'s argv, because this test wants the server's
        // stderr on ours — a boot that fails says why on it, and the guest's kernel log
        // arrives there too. Only that is ours to place: the client owns the two
        // descriptors the protocol runs on, and starts the process it drives.
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        for (key, value) in env {
            server.env(key, value);
        }

        let client =
            cortex::console::stdio::StdioClient::new(server).expect("starting the console server");

        // Building announces the session, so a fixture that exists is one the server has
        // answered. Nothing is booted by it — the first command below pays for that.
        let console = Console::builder()
            .client(client)
            .executables(
                ExecutableSet::new()
                    .register("report", Report)
                    .register("rawbytes", RawBytes),
            )
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    async fn output(&mut self, script: &str) -> ExecResult {
        self.console
            .exec(["sh", "-c", script], None)
            .await
            .expect("running the command")
    }

    async fn write(&mut self, path: &str, data: &[u8]) -> u64 {
        self.console
            .write(path, data, None)
            .await
            .expect("writing")
            .size
    }

    async fn read(&mut self, path: &str) -> ReadResult {
        self.console.read(path, None, None).await.expect("reading")
    }
}

/// A session, from the outside: commands run somewhere that is not this host, on a
/// filesystem that is this session's, and the file plane names what the commands name.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn commands_run_in_a_guest_of_their_own() {
    let mut fx = Fixture::new().await;

    // Somewhere else, and somewhere Linux. The host this runs on is not, which is the
    // whole reason for the crate.
    let out = fx.output("uname -s").await;
    assert_eq!(
        out.stdout,
        b"Linux\n",
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.code, 0);

    // A command's own status is a result and not a failure of the call.
    assert_eq!(fx.output("exit 7").await.code, 7);

    // Output is bytes. Nothing between the guest and here is text.
    assert_eq!(
        fx.output(r"printf '\377\376\n'").await.stdout,
        [0xff, 0xfe, b'\n']
    );

    // stdout and stderr stay apart.
    let out = fx.output("echo out; echo err >&2").await;
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");

    // The root is writable — which it would not be without the overlay, since the base
    // image is a read-only block device.
    assert_eq!(
        fx.output("echo hi > /root/f && cat /root/f").await.stdout,
        b"hi\n"
    );

    // And a write survives the command that made it: one guest serves the session, so the
    // next `exec` is a new process on the same filesystem.
    assert_eq!(fx.output("cat /root/f").await.stdout, b"hi\n");

    // The two planes name the same file, which is the property that makes a `write` useful
    // for setting up a command's input.
    assert_eq!(fx.write("/root/raw", &[0xff, 0x00, b'\n']).await, 3);
    assert_eq!(fx.read("/root/raw").await.data, [0xff, 0x00, b'\n']);

    fx.output("printf 'from the command' > /root/shared").await;
    let out = fx.read("/root/shared").await;
    assert_eq!(out.data, b"from the command");
    assert_eq!(out.size, 16);
}

/// The part that is not a remote `exec`: a name whose behaviour lives in *this* process is
/// runnable by a command inside the guest, and composes with the shell like a program.
///
/// Which is four processes, two filesystems and a hypervisor: the guest's `sh` runs a
/// symlink, the symlink re-enters the guest binary as a shim, the shim dials a socket in
/// the guest, the agent answers the console request with a `Delegated`, this process runs
/// the closure, and the bytes come back out of the shim's own stdout.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_delegated_name_is_answered_on_the_host() {
    let mut fx = Fixture::new().await;

    let out = fx.output("report one two").await;
    assert_eq!(out.stdout, b"report|one,two\n");
    assert_eq!(out.code, 0);

    // A program as far as the shell is concerned.
    assert_eq!(
        fx.output("report a | tr a-z A-Z").await.stdout,
        b"REPORT|A\n"
    );

    // Twice in one command, so the chain is a loop and not a single extra step.
    assert_eq!(
        fx.output("report x; report y").await.stdout,
        b"report|x\nreport|y\n"
    );

    // Bytes through all of it: a socket, a virtio port, a pipe and a shell.
    assert_eq!(
        fx.output("rawbytes").await.stdout,
        [0xff, 0xfe, 0x00, b'\n']
    );
}

/// A cortex volume, served into the guest out of the console server's own address space —
/// no host mount, no daemon, and no copy of anything.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_workspace_is_the_same_tree_on_both_sides() {
    let dir = tempfile::tempdir().expect("a temp directory");
    std::fs::write(dir.path().join("from-the-host"), b"hello from the host\n")
        .expect("writing a file for the guest to read");

    let mut fx = Fixture::with_env(&[(
        "CORTEX_UVM_WORKSPACE",
        &format!("{}:/workspace", dir.path().display()),
    )])
    .await;

    // The guest reads what the host wrote.
    assert_eq!(
        fx.output("cat /workspace/from-the-host").await.stdout,
        b"hello from the host\n"
    );

    // And the working directory is the workspace, so a relative path in a command means
    // what it means on the host side of the same tree.
    assert_eq!(fx.output("pwd").await.stdout, b"/workspace\n");
    assert_eq!(
        fx.output("cat from-the-host").await.stdout,
        b"hello from the host\n"
    );

    // The guest writes, and the bytes are on the host's disk rather than in the session's
    // image — the volume is the destination, not a cache of one.
    fx.output("echo 'hello from the guest' > from-the-guest")
        .await;
    assert_eq!(
        std::fs::read(dir.path().join("from-the-guest")).expect("the guest's file, on the host"),
        b"hello from the guest\n"
    );
}

/// A console server that is killed outright takes its guest with it.
///
/// Which is not covered by anything above. A session that ends the way it is meant to ends
/// with a `quit`, and the server's own `Drop` kills the VMM — so the interesting case is the
/// one where no destructor runs at all, and what has to reach the guest instead is the
/// channel closing: the agent reads EOF, returns, and the process it is stops being the
/// guest's init.
///
/// Worth a test because the alternative failure is silent and expensive — a micro-VM holding
/// a couple of gibibytes with nothing left to talk to it.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_killed_server_takes_its_guest_with_it() {
    let mut fx = Fixture::new().await;
    // Anything at all, to pay for the boot: there is no guest to orphan until something has
    // needed one.
    assert_eq!(fx.output("true").await.code, 0);

    let vmm = pids("boot-helper/boot-");
    assert_eq!(vmm.len(), 1, "expected exactly one VMM, found {vmm:?}");

    for pid in pids("target/debug/cortex-uvm-console") {
        kill(pid);
    }

    // The chain is a kernel shutting down, so it is not instant.
    for _ in 0..100 {
        if pids("boot-helper/boot-").is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the VMM {vmm:?} outlived the server that started it");
}

/// Every pid whose command line contains `pattern`.
fn pids(pattern: &str) -> Vec<i32> {
    let out = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .expect("running pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn kill(pid: i32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}
