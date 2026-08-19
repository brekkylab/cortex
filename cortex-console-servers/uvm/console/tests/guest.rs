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

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use cortex::{
    BoxFuture,
    console::{Console, ExecResult, ReadResult},
    exec::{ExecCall, ExecResult as ExecOutput, Executable, ExecutableSet},
    fs::Mount,
};
use tokio::process::Command;

/// A directory standing in for a mounted tree.
///
/// Not a mount this test made: what is under test is what the *backend* does with the path
/// it is given, and a plain directory answers one the way a mount point does.
struct Mounted(PathBuf);

impl Mount for Mounted {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

/// Reports everything it was told, so a round trip can be told from a coincidence.
struct Report;

impl Executable for Report {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        _mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move { ExecOutput::ok(format!("{}|{}\n", call.name, call.args.join(","))) })
    }
}

/// Answers with the contents of the file it was named, opened through the mount it was
/// handed — the host's half of one file name meaning one file across a hypervisor.
///
/// Byte-for-byte what `cortex-local-console`'s own suite uses, and that is the claim: the
/// client's half of a delegated call is the same on both backends, because the directory the
/// guest reports is a path this host can open either way.
struct CatHere;

impl Executable for CatHere {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move {
            let Some(mount) = mount else {
                return ExecOutput::failed(1, "nothing is mounted");
            };
            let path = match call.resolve(&call.args[0]) {
                Ok(path) => mount.host_path(&path),
                Err(e) => return ExecOutput::failed(1, format!("{}: {e}", call.args[0])),
            };
            match std::fs::read(&path) {
                Ok(bytes) => ExecOutput::ok(bytes),
                Err(e) => ExecOutput::failed(1, format!("{}: {e}", path.display())),
            }
        })
    }
}

/// A delegated name whose output is not text and not something a shell would survive
/// re-encoding.
struct RawBytes;

impl Executable for RawBytes {
    fn exec<'a>(
        &'a self,
        _call: &'a ExecCall,
        _mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecOutput> {
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
                    .register("report", "report the call it was made with", Report)
                    .register("rawbytes", "answer bytes that are not text", RawBytes)
                    .register("cat-here", "read a file where the command stood", CatHere),
            )
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    /// A fixture whose session works in `root`, which is a directory on this host.
    ///
    /// Over the channel and not through the server's environment: what a client says is a
    /// tree it has, and the server answers where it put it. Here the answer is the same
    /// path, because a `file://` tree is shared into the guest where the host has it.
    async fn with_tree(root: &Path) -> Fixture {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        let client =
            cortex::console::stdio::StdioClient::new(server).expect("starting the console server");

        let console = Console::builder()
            .client(client)
            .mount(Mounted(root.to_path_buf()))
            .executables(
                ExecutableSet::new()
                    .register("report", "report the call it was made with", Report)
                    .register("rawbytes", "answer bytes that are not text", RawBytes)
                    .register("cat-here", "read a file where the command stood", CatHere),
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

/// A tree that is not on this host is refused **before a VM is worth starting**, with the
/// code that says the environment is wrong rather than the session.
///
/// The same claim `cortex-local-console`'s suite makes, and it has to be: a client cannot
/// tell which backend answered it, so `-32009` has to mean the same thing on both. The
/// distinction is easy to lose here — everything about a boot happens in a child process
/// that writes its failures to its own stderr and exits, which reaches a client as one
/// undifferentiated closed channel — so what this asserts is that the check happens on this
/// side, where an answer can still be given.
///
/// **Not** `#[ignore]`d, because nothing here boots. If this ever needs libkrunfw, the check
/// moved.
#[tokio::test]
async fn a_tree_that_is_not_there_is_refused_before_a_vm_is_started() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let mut fx = Fixture::with_tree(&dir.path().join("not-created")).await;

    let err = fx
        .console
        .exec(["true"], None)
        .await
        .expect_err("no session can run in a directory that is not there");
    assert_eq!(
        err.code(),
        Some(cortex::console::Error::MOUNT_FAILED),
        "answered {err:?} — a tree that is missing is not a backend that would not come up"
    );
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

/// An OCI image off a registry is a base like any other, and what it says about running a
/// process in it reaches the process.
///
/// Two claims, and the second is the one worth the boot. That `python3` exists proves the
/// image really is the root — no rootfs tarball has it. That `PYTHON_VERSION` is set proves
/// the image's `ENV` was replayed: it is declared nowhere but the image config, so a shell
/// that can read it read what the config said.
///
/// The values asserted are the ones this image's config actually carries, which is four
/// variables and no more — an image that states a `LANG` or a `WORKDIR` is common enough to
/// assume and this one does neither.
///
/// Debian-based on purpose. `mount(2)` is called directly precisely so a base whose `mount`
/// binary is util-linux's works, and this is the test that would fail if that ever became a
/// spawned command again. The guest binary being musl-static is the other half — it runs on a
/// glibc image without sharing a libc with it.
///
/// `-slim` for the download, not for the coverage: it is the same Debian userland as the full
/// tag with a few hundred megabytes less to pull the first time.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and a registry pull"]
async fn an_oci_image_is_a_base_and_its_environment_is_the_command_s() {
    let mut fx = Fixture::with_env(&[("CORTEX_UVM_IMAGE", "python:3.13-slim")]).await;

    let out = fx.output("python3 -c 'print(1 + 1)'").await;
    assert_eq!(
        out.stdout,
        b"2\n",
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = fx.output(r#"printf '%s' "$PYTHON_VERSION""#).await;
    assert!(
        out.stdout.starts_with(b"3.13."),
        "PYTHON_VERSION came out as {:?} — the image's ENV did not reach the command",
        String::from_utf8_lossy(&out.stdout)
    );

    // The delegated names went on the end of whatever `PATH` was in force, rather than in place
    // of it.
    let path = fx.output(r#"printf '%s' "$PATH""#).await.stdout;
    let path = String::from_utf8(path).expect("a PATH that is text");
    assert!(
        path.contains("/cortex-console-"),
        "PATH is {path:?} — the delegated names are not on it"
    );

    // Which is what being on it is for: a name this process answers is callable from inside an
    // image that knows nothing about it.
    assert_eq!(
        fx.output("report one two").await.stdout,
        b"report|one,two\n"
    );

    // And `python3` is still the image's, not something the appended directory shadowed.
    assert_eq!(
        fx.output("command -v python3").await.stdout,
        b"/usr/local/bin/python3\n"
    );
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

/// A tree the host has, in front of a guest — **at the same path on both sides**.
///
/// Which is the whole of what makes this backend usable through the protocol: the client is
/// told where the tree is, the guest stands in a directory of that name, and neither end
/// rewrites a path to talk to the other. A constant of this crate's choosing would have made
/// every path crossing the boundary two paths.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_tree_is_the_same_path_on_both_sides() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let root = dir.path().canonicalize().expect("a real directory");
    std::fs::write(root.join("from-the-host"), b"hello from the host\n")
        .expect("writing a file for the guest to read");

    let mut fx = Fixture::with_tree(&root).await;

    // What the server answered is the path this host has, and the session stands in it.
    assert_eq!(fx.console.workfs_path(), Some(root.as_path()));
    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", root.display()).into_bytes(),
        "the guest stands where the host says the tree is"
    );

    // The guest reads what the host wrote, by the name the host would use and by a
    // relative one from where it stands.
    assert_eq!(
        fx.output(&format!("cat {}/from-the-host", root.display()))
            .await
            .stdout,
        b"hello from the host\n"
    );
    assert_eq!(
        fx.output("cat from-the-host").await.stdout,
        b"hello from the host\n"
    );

    // The guest writes, and the bytes are on the host's disk rather than in the session's
    // image — the tree is the destination, not a cache of one.
    fx.output("echo 'hello from the guest' > from-the-guest")
        .await;
    assert_eq!(
        std::fs::read(root.join("from-the-guest")).expect("the guest's file, on the host"),
        b"hello from the guest\n"
    );

    // And the file plane names what a command names, through the same path.
    assert_eq!(
        fx.read(root.join("from-the-guest").to_str().unwrap())
            .await
            .data,
        b"hello from the guest\n"
    );

    // `cd` moves the session, and the next command runs where it left off — the guest
    // agent's state machine, reported back in the path the client already knows.
    std::fs::create_dir(root.join("work")).expect("a subdirectory");
    let moved = fx.console.exec(["cd", "work"], None).await.expect("cd");
    assert_eq!(moved.cwd.as_deref(), root.join("work").to_str());
    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", root.join("work").display()).into_bytes()
    );
}

/// The whole point, end to end, across the VM boundary: a delegated executable running on
/// the **host** opens the same file the guest command meant.
///
/// The guest reports the directory it stood in, which is a path this host has because the
/// tree is shared at its own name; the host's executable resolves the argument against it
/// and opens the file. Four processes and a hypervisor, and one name meaning one thing
/// throughout — with nothing in the middle translating.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_delegated_call_opens_what_the_guest_command_meant() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let root = dir.path().canonicalize().expect("a real directory");
    std::fs::create_dir(root.join("sub")).expect("a subdirectory");
    std::fs::write(root.join("sub/report.md"), b"# from the tree\n").expect("writing a file");

    let mut fx = Fixture::with_tree(&root).await;

    let out = fx.output("cd sub && cat-here report.md").await;
    assert_eq!(
        out.stdout,
        b"# from the tree\n",
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
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
