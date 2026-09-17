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

use std::{path::Path, process::Stdio};

use cortex::console::{Console, ExecResp, ImageSource, NetworkAccess, ReadResp};
use tokio::process::Command;

/// A console over the real binary.
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
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    /// A fixture whose session names its base over the channel, the way a client does.
    ///
    /// Over the channel and not through the server's environment, which is the point: what a
    /// client declares in its `init` is what the session runs on, and the server's own setting
    /// is only the answer when nothing was declared.
    async fn asking_for(image: impl Into<ImageSource>) -> anyhow::Result<Fixture> {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        let client = cortex::console::stdio::StdioClient::new(server)?;

        let console = Console::builder()
            .client(client)
            .image(image)
            .build()
            .await?;

        Ok(Fixture { console })
    }

    /// A fixture whose session asks for `reach` over the channel, the way a client does.
    ///
    /// The same shape as [`asking_for`](Self::asking_for) and for the same reason: what a
    /// client declares in its `init` is what the session gets, and the server's own setting is
    /// only the answer when nothing was declared.
    async fn asking_for_reach(reach: NetworkAccess) -> anyhow::Result<Fixture> {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        let client = cortex::console::stdio::StdioClient::new(server)?;

        let console = Console::builder()
            .client(client)
            .network(reach)
            .build()
            .await?;

        Ok(Fixture { console })
    }

    /// A fixture whose session works in `root`, which is a directory on this host.
    ///
    /// Over the channel and not through the server's environment: what a client says is a
    /// tree it has, and the server answers where it put it — which here is `/context`, the
    /// guest's name for that role, and not the host path this was handed.
    async fn with_tree(root: &Path) -> Fixture {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        let client =
            cortex::console::stdio::StdioClient::new(server).expect("starting the console server");

        let console = Console::builder()
            .client(client)
            .context(root.to_path_buf())
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    /// A fixture whose one tree is a **scratch**, standing in `root`.
    ///
    /// What it buys is a tree the session may write in: the context is mounted read-only, so
    /// a session given only that one has nowhere to put a file — which is what the tree means
    /// rather than something to work around here.
    async fn with_scratch(root: &Path) -> Fixture {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        let client =
            cortex::console::stdio::StdioClient::new(server).expect("starting the console server");

        let console = Console::builder()
            .client(client)
            .scratch(root.to_path_buf())
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    /// A fixture with all three trees, each a directory on this host.
    ///
    /// The same exchange `with_tree` makes, three times over: what a client says is a tree it
    /// has, and the server answers where it put each one.
    async fn with_trees(context: &Path, artifacts: &Path, scratch: &Path) -> Fixture {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
        server.stderr(Stdio::inherit());
        let client =
            cortex::console::stdio::StdioClient::new(server).expect("starting the console server");

        let console = Console::builder()
            .client(client)
            .context(context.to_path_buf())
            .artifacts(artifacts.to_path_buf())
            .scratch(scratch.to_path_buf())
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    async fn output(&mut self, script: &str) -> ExecResp {
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

    async fn read(&mut self, path: &str) -> ReadResp {
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

/// Every tree is checked before a VM is started, not just the context — a client will send
/// paths into all of them, and a missing artifacts directory would otherwise fail at the
/// first write into it rather than at the boot that could not be made.
///
/// **Not** `#[ignore]`d, for the reason above: nothing here boots.
#[tokio::test]
async fn an_artifacts_tree_that_is_not_there_is_refused_before_a_vm_is_started() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let context = dir.path().join("project");
    std::fs::create_dir(&context).unwrap();

    let mut fx = Fixture::with_trees(
        &context,
        &dir.path().join("not-created"),
        &dir.path().join("scratch-not-created"),
    )
    .await;

    let err = fx
        .console
        .exec(["true"], None)
        .await
        .expect_err("no session can leave output in a directory that is not there");
    assert_eq!(
        err.code(),
        Some(cortex::console::Error::MOUNT_FAILED),
        "answered {err:?} — a tree that is missing is not a backend that would not come up"
    );
}

/// The three trees are three shares, each at the host's own path for it, and **the session
/// stands in the scratch** — so a relative path a command writes lands there rather than in
/// the tree the client was working on.
///
/// And the context is mounted read-only, which is the one thing the guest does differently
/// with the three. Asserted here rather than in a boot of its own for the reason at the top
/// of this file: it is the same session a client performs, one step further along.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_guest_mounts_every_tree_and_stands_in_the_scratch() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let (context, artifacts, scratch) = (
        dir.path().join("project"),
        dir.path().join("out"),
        dir.path().join("scratch"),
    );
    for at in [&context, &artifacts, &scratch] {
        std::fs::create_dir(at).unwrap();
    }
    std::fs::write(context.join("given.txt"), b"from the client\n").unwrap();

    let mut fx = Fixture::with_trees(&context, &artifacts, &scratch).await;

    // Each tree is answered by the guest's name for its role, which is what the client
    // sends paths in from here on. The host directory behind each stays the caller's own —
    // it is the mount point this test handed over, and nothing in the guest is named after
    // it.
    assert_eq!(fx.console.context_path(), Some(Path::new("/context")));
    assert_eq!(fx.console.artifacts_path(), Some(Path::new("/artifacts")));
    assert_eq!(fx.console.scratch_path(), Some(Path::new("/scratch")));

    // The guest stands in the scratch, which nothing on the `exec` frame said.
    assert_eq!(
        String::from_utf8_lossy(&fx.output("pwd").await.stdout).trim_end(),
        "/scratch"
    );

    // The context is readable at the path the client was told it is at...
    assert_eq!(
        String::from_utf8_lossy(&fx.output("cat /context/given.txt").await.stdout),
        "from the client\n"
    );

    // ...a relative write lands in the scratch...
    fx.output("echo working > note.txt").await;
    assert_eq!(
        std::fs::read_to_string(scratch.join("note.txt")).unwrap(),
        "working\n"
    );

    // ...and what the session leaves in the artifacts tree is on this host, which is the
    // whole reason for the tree: the client collects it without a `read`.
    fx.output("echo kept > /artifacts/report.txt").await;
    assert_eq!(
        std::fs::read_to_string(artifacts.join("report.txt")).unwrap(),
        "kept\n"
    );

    // The context is read-only, and here that is the guest kernel's answer rather than a
    // rule the protocol applies: the share went up `MS_RDONLY`, so the command's own
    // redirection fails and the client's project is as it was.
    let refused = fx.output("echo edited > /context/given.txt").await;
    assert_ne!(refused.code, 0, "a command wrote into a read-only context");
    assert_eq!(
        std::fs::read_to_string(context.join("given.txt")).unwrap(),
        "from the client\n",
        "the tree the client gave the session came back edited"
    );

    // And the file plane says the same, with the code a read-only filesystem refusing a
    // write comes back as — which is what `cortex-local-console` answers for the same call,
    // where there is no mount to be read-only and the server checks the path itself.
    let refused = fx
        .console
        .write("/context/given.txt", &b"edited"[..], None)
        .await
        .expect_err("writing into the context");
    assert_eq!(refused.code(), Some(cortex::console::Error::IO_FAILED));
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

/// `timeout_ms` is a kill in the guest too, and the kill takes the command's process group.
///
/// The same claim `cortex-local-console`'s `exec_timeout` suite makes, in one session
/// because each of these is a boot. The three shapes are the ones that differ in what the
/// agent has to get right: a foreground command still running when the timeout fires; a
/// shell that has already exited while a background child of its own holds the pipes, so
/// the group's leader is a zombie when `killpg` runs; and a command that fits, which the
/// timeout must not touch.
///
/// Whether the background `sleep` died is asked of the guest itself, through `/proc` rather
/// than `pgrep`: the rootfs is not promised to have one, and `/proc` is what the agent
/// mounted for exactly this kind of question.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_command_past_its_timeout_is_killed_in_the_guest() {
    use std::time::{Duration, Instant};

    use cortex::console::Error;

    let mut fx = Fixture::new().await;
    // Boot first, so the timings below measure the kill and not the kernel.
    assert_eq!(fx.output("true").await.code, 0);

    // A foreground command.
    let started = Instant::now();
    let err = fx
        .console
        .exec(["sh", "-c", "sleep 10"], Some(300))
        .await
        .expect_err("a 10s sleep under a 300ms timeout must be refused");
    assert_eq!(err.code(), Some(Error::TIMED_OUT), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the kill must not wait for the command: {:?}",
        started.elapsed()
    );

    // A shell that exits at once, leaving a child that holds both pipes; then a shell that
    // stays. Distinct durations, so each `sleep` can be told apart in `/proc` afterwards.
    for script in ["sleep 37.31 & exit 0", "sleep 41.13 & sleep 41.13"] {
        let err = fx
            .console
            .exec(["sh", "-c", script], Some(300))
            .await
            .expect_err("a backgrounded sleep under a 300ms timeout must be refused");
        assert_eq!(err.code(), Some(Error::TIMED_OUT), "{script}: {err:?}");
    }

    // The signal and the exits are not synchronised with this end, so ask a few times —
    // but for far less than the sleeps would otherwise serve.
    const STILL_RUNNING: &str = r"for p in /proc/[0-9]*; do tr '\0' ' ' < $p/cmdline 2>/dev/null; echo; done | grep -c 'sleep [34][71]\.[13][13]'";
    let started = Instant::now();
    let mut left = String::new();
    while started.elapsed() < Duration::from_secs(3) {
        left = String::from_utf8_lossy(&fx.output(STILL_RUNNING).await.stdout)
            .trim()
            .to_string();
        if left == "0" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        left, "0",
        "background sleeps outlived the timeout kill — their group was not signalled"
    );

    // A command that fits is answered as if no timeout had been named, and the session has
    // survived three kills.
    let ok = fx
        .console
        .exec(["sh", "-c", "sleep 0.1; echo done"], Some(5_000))
        .await
        .expect("a command within its timeout");
    assert_eq!(ok.code, 0);
    assert_eq!(ok.stdout, b"done\n");
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
    let mut fx = Fixture::asking_for("python:3.13-slim")
        .await
        .expect("a session on an image it named");

    // **The answer is this server's spelling, not an echo.** A bare reference names a default
    // registry, and which one it was is the thing the client could not have worked out.
    assert_eq!(
        fx.console.image().map(|image| image.reference.as_str()),
        Some("docker.io/library/python:3.13-slim"),
        "the server did not say which image the session got"
    );

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

    // And the `PATH` a command runs against is the image's own, which is what makes
    // `python3` the one the image installed rather than whatever the guest's default path
    // happened to find first.
    let path = fx.output(r#"printf '%s' "$PATH""#).await.stdout;
    let path = String::from_utf8(path).expect("a PATH that is text");
    assert!(
        path.contains("/usr/local/bin"),
        "PATH is {path:?} — the image's own did not reach the command"
    );
    assert_eq!(
        fx.output("command -v python3").await.stdout,
        b"/usr/local/bin/python3\n"
    );
}

/// What a client names beats what this server was configured with, and both are answered.
///
/// **Not** `#[ignore]`d, because nothing boots: naming a base settles which image a guest will
/// be built on, and settling it is `init`'s. The pull it implies is the first boot's.
#[tokio::test]
async fn the_base_in_force_is_answered_whoever_chose_it() {
    // Nothing declared, so this server's own setting stands — and is answered, which is how a
    // client that declared nothing finds out what it got.
    let quiet = Fixture::with_env(&[("CORTEX_UVM_IMAGE", "alpine:3.21")]).await;
    assert_eq!(
        quiet.console.image().map(|image| image.reference.as_str()),
        Some("docker.io/library/alpine:3.21"),
    );
    drop(quiet);

    // Declared, and it wins: the environment above says something else entirely.
    let asked = Fixture::asking_for("python:3.13-slim")
        .await
        .expect("a session on an image it named");
    assert_eq!(
        asked.console.image().map(|image| image.reference.as_str()),
        Some("docker.io/library/python:3.13-slim"),
    );
    drop(asked);

    // And a session on the pinned rootfs has no reference to give, which is not the same as a
    // server declining to answer but reads the same way to a client.
    let bare = Fixture::new().await;
    assert_eq!(bare.console.image(), None);
}

/// A reference that is not one is refused at `init`, before a VM is worth starting.
///
/// The line this draws is between a name that cannot be parsed and a name that cannot be
/// fetched. The first is the client's mistake and is knowable from the frame; the second is a
/// registry's answer and takes a pull, so it belongs to a boot.
#[tokio::test]
async fn a_reference_that_is_not_one_is_refused_before_a_vm_is_started() {
    let err = match Fixture::asking_for("nota reference").await {
        Err(e) => e,
        Ok(_) => panic!("a session was opened on something that is not a reference"),
    };

    let err = err
        .downcast_ref::<cortex::console::Failure>()
        .expect("a protocol failure");
    assert_eq!(
        err.code(),
        Some(cortex::console::Error::INVALID_PARAMS),
        "answered {err:?} — a malformed reference is not a backend that would not come up"
    );
}

/// A listener on this host, and the port it is on.
///
/// One canned HTTP response per connection, and as many connections as are asked for — `wget`
/// is what asks, so it has to be HTTP rather than bytes. The thread is returned so a caller can
/// hold it for the length of the test; nothing joins it, because a reach that was refused is a
/// connection that never came.
fn host_listener() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener on this host");
    let port = listener.local_addr().expect("its address").port();
    let served = std::thread::spawn(move || {
        while let Ok((mut connection, _)) = listener.accept() {
            use std::io::Write as _;
            let _ = connection.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 7\r\n\r\nreached");
        }
    });
    (port, served)
}

/// The same fetch, by the name the stack answers for the machine it runs beside.
///
/// `host.microsandbox.internal` is synthesized by the stack's own resolver rather than looked
/// up anywhere, which is what spares a command from digging the gateway out of `resolv.conf`:
/// the addresses are assigned per sandbox slot, so nothing writing a command can know them, and
/// this name is a constant.
fn fetch_by_host_name(port: u16) -> String {
    format!("wget -q -T 5 -O - http://host.microsandbox.internal:{port}/")
}

/// A guest command that fetches `port` on the gateway — which is this host, one rewrite later.
///
/// The address comes out of the guest's own `resolv.conf` because that is where the gateway is
/// written down: the stack names itself as the resolver, so the nameserver line *is* the
/// gateway. Nothing in the guest was told this host's real address, and nothing could be.
fn fetch_from_gateway(port: u16) -> String {
    format!(
        "wget -q -T 5 -O - \
         http://$(awk '/^nameserver/{{print $2; exit}}' /etc/resolv.conf):{port}/"
    )
}

/// A session that asked for nothing has no interface to configure, which is the default and the
/// thing most worth keeping true.
///
/// Not "the interface is down" — there is no device, so there is nothing to bring up. What the
/// guest does have is a loopback and the `dummy0` its kernel makes on its own, and neither goes
/// anywhere: what this looks for is a default route, which is what a configured device would
/// have left behind.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_session_is_air_gapped_unless_a_network_was_asked_for() {
    let mut fx = Fixture::with_env(&[("CORTEX_UVM_NETWORK", "none")]).await;

    let routes = fx.output("cat /proc/net/route").await.stdout;
    let routes = String::from_utf8(routes).expect("a route table that is text");
    assert!(
        !routes
            .lines()
            .skip(1)
            .any(|line| line.split_whitespace().nth(1) == Some("00000000")),
        "a session that asked for no network has a default route:\n{routes}"
    );

    // And nothing wrote a resolver, so a name has nowhere to be looked up either.
    assert_eq!(fx.output("cat /etc/resolv.conf").await.code, 1);
}

/// The default posture: names resolve, granted doors open, everything else refused.
///
/// Four claims about one device. The interface came up and a lookup was answered — by the stack
/// in the console server process, since there is nothing else on that network to answer it. A
/// listener on *this host* is reached, which is what makes `host` a reach rather than an error
/// message. **A second listener on this host is not**, which is the property the grant exists
/// for: a door is a port and never the machine. And a public address is refused.
///
/// Both listeners are plain loopback on the test's own side. Nothing tells the guest where they
/// are: it dials the gateway, and the stack rewrites that to the host's loopback when it dials
/// out — the mechanism under test as much as the rules are.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and the internet"]
async fn a_host_session_reaches_the_doors_it_was_granted() {
    let (granted, _open) = host_listener();
    let (ungranted, _shut) = host_listener();

    let mut fx = Fixture::asking_for_reach(NetworkAccess::host().with_host_ports([granted]))
        .await
        .expect("a session with a granted port");

    // What the server says it gave, which is both halves of the answer.
    let answered = fx.console.network().expect("a server that says").clone();
    assert_eq!(answered.reach, "host");
    assert_eq!(answered.host_ports, [granted]);

    // The interface the stack assigned, configured by the guest with no `ip` binary in sight.
    let routes = fx.output("cat /proc/net/route").await.stdout;
    let routes = String::from_utf8(routes).expect("a route table that is text");
    assert!(
        routes
            .lines()
            .skip(1)
            .any(|line| line.split_whitespace().nth(1) == Some("00000000")),
        "no default route, so the interface was never configured:\n{routes}"
    );
    assert!(
        fx.output("cat /etc/resolv.conf")
            .await
            .stdout
            .starts_with(b"nameserver "),
        "no resolver was written"
    );

    // `getent hosts` rather than a ping: ICMP is a different permission from a name, and what
    // is under test here is the resolver.
    let out = fx.output("getent hosts example.com").await;
    assert_eq!(
        out.code,
        0,
        "a name did not resolve: {:?} {:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The door that was granted.
    let out = fx.output(&fetch_from_gateway(granted)).await;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "reached",
        "the granted port was not reached: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And the one beside it, which was not. The whole reason ports are named one at a time.
    let out = fx.output(&fetch_from_gateway(ungranted)).await;
    assert_ne!(
        out.code,
        0,
        "a port nobody granted was reached, so the grant is the machine and not a door: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The same door by name, which is how a command would actually be written: the gateway's
    // address is assigned per sandbox and this is the constant that stands for it.
    let out = fx.output(&fetch_by_host_name(granted)).await;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "reached",
        "the granted port was not reachable by name: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // **Resolving the name grants nothing.** It answers for every port on this machine and the
    // policy is still what decides which of them reply.
    let out = fx.output(&fetch_by_host_name(ungranted)).await;
    assert_ne!(
        out.code,
        0,
        "the name reached a port nobody granted, so resolving it is a grant: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // …and that is all it gets. `wget` is busybox's here and exits non-zero on a refusal; what
    // matters is that it does not succeed.
    let out = fx
        .output("wget -q -T 5 -O /dev/null http://example.com/")
        .await;
    assert_ne!(
        out.code, 0,
        "the default posture reached the internet, which is what it exists not to do"
    );
}

/// `public` reaches the internet, and *not* this machine's ports.
///
/// The second half is the one worth a boot: **widening what a session reaches outside must not
/// widen what it reaches in here.** A session that asked for the internet to fetch a package did
/// not thereby ask to talk to whatever else this host is listening on, and the only thing that
/// opens those is a grant it did not make.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and the internet"]
async fn a_public_session_reaches_the_internet_and_not_this_hosts_ports() {
    let (ungranted, _shut) = host_listener();
    let mut fx = Fixture::with_env(&[("CORTEX_UVM_NETWORK", "public")]).await;

    let out = fx.output(&fetch_from_gateway(ungranted)).await;
    assert_ne!(
        out.code,
        0,
        "the internet came with a door onto this host: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = fx
        .output("wget -q -T 10 -O /dev/null http://example.com/")
        .await;
    assert_eq!(
        out.code,
        0,
        "a session that asked for the internet could not reach it: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And over TLS, which is a different path through the stack: a stream it does not read.
    let out = fx
        .output("wget -q -T 10 -O /dev/null https://example.com/")
        .await;
    assert_eq!(
        out.code,
        0,
        "plain HTTP reached the internet and HTTPS did not: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A door onto a machine the guest cannot send a packet to is two settings that cannot both
/// have been meant, and is said at `init` like any other.
///
/// **Not** `#[ignore]`d: nothing boots, which is the property being asserted.
#[tokio::test]
async fn a_granted_port_without_a_network_is_refused_before_a_vm_is_started() {
    let asked = NetworkAccess::none().with_host_ports([8080]);
    let err = match Fixture::asking_for_reach(asked).await {
        Err(e) => e,
        Ok(_) => panic!("a session was opened with doors onto a network it does not have"),
    };

    let err = err
        .downcast_ref::<cortex::console::Failure>()
        .expect("a protocol failure");
    assert_eq!(
        err.code(),
        Some(cortex::console::Error::INVALID_PARAMS),
        "answered {err:?} — a contradiction is not a name nobody defined"
    );
}

/// A reach nobody defined is refused at `init`, before a VM is worth starting.
///
/// **Not** `#[ignore]`d, because nothing here boots — which is the property being asserted. A
/// name the server cannot answer is knowable from the frame, so a client hears about it while
/// it can still ask for something else.
#[tokio::test]
async fn a_reach_that_is_not_a_reach_is_refused_before_a_vm_is_started() {
    let err = match Fixture::asking_for_reach(NetworkAccess::new("sort-of")).await {
        Err(e) => e,
        Ok(_) => panic!("a session was opened with a reach nobody defined"),
    };

    let err = err
        .downcast_ref::<cortex::console::Failure>()
        .expect("a protocol failure");
    assert_eq!(
        err.code(),
        Some(cortex::console::Error::UNSUPPORTED_NETWORK),
        "answered {err:?} — a name nobody defined is not a backend that would not come up"
    );
}

/// What the client asks for is what the session gets, and the server says so.
///
/// The `init` declaration is the whole subject here: nothing sets `CORTEX_UVM_NETWORK`, so a
/// guest that reaches the internet reached it because the client asked over the channel.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and the internet"]
async fn a_client_asks_for_its_reach_over_the_channel() {
    // Asked for nothing, so the answer is the server's own — which is `host` by default, and
    // the only way a client could have learnt that.
    let quiet = Fixture::new().await;
    assert_eq!(
        quiet.console.network().map(|n| n.reach.as_str()),
        Some("host"),
        "a server that chose the reach did not say which"
    );
    drop(quiet);

    // Asked for none: no device, so no default route for the guest to have.
    let mut off = Fixture::asking_for_reach(NetworkAccess::none())
        .await
        .expect("a session with no network");
    assert_eq!(
        off.console.network().map(|n| n.reach.as_str()),
        Some("none")
    );
    let routes = String::from_utf8(off.output("cat /proc/net/route").await.stdout)
        .expect("a route table that is text");
    assert!(
        !routes
            .lines()
            .skip(1)
            .any(|line| line.split_whitespace().nth(1) == Some("00000000")),
        "a session that asked for no network has a default route:\n{routes}"
    );
    drop(off);

    // And asked for the internet: reached, with nothing in the server's environment saying so.
    let mut open = Fixture::asking_for_reach(NetworkAccess::public())
        .await
        .expect("a session with the internet");
    assert_eq!(
        open.console.network().map(|n| n.reach.as_str()),
        Some("public")
    );
    let out = open
        .output("wget -q -T 10 -O /dev/null http://example.com/")
        .await;
    assert_eq!(
        out.code,
        0,
        "a session that asked for the internet could not reach it: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A tree the host has, in front of a guest — **named by its role on the guest's side**.
///
/// Which is the whole of what makes this backend usable through the protocol: the client is
/// told where the tree is, the guest stands in a directory of that name, and neither end
/// rewrites a path to talk to the other. The two names for one directory never meet, because
/// only one of them is ever in a frame — the host's own is the caller's mount point, which is
/// what this test uses to check the bytes and never sends.
///
/// Over a scratch, because half of what this asserts is writing: the context is read-only,
/// and a session given only one has no tree the protocol will write in.
#[tokio::test]
#[ignore = "boots a micro-VM: needs libkrunfw, a hypervisor, and possibly a download"]
async fn a_tree_is_named_by_its_role_on_the_guests_side() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let root = dir.path().canonicalize().expect("a real directory");
    std::fs::write(root.join("from-the-host"), b"hello from the host\n")
        .expect("writing a file for the guest to read");

    let mut fx = Fixture::with_scratch(&root).await;

    // What the server answered is the guest's name for the role, and the session stands in
    // it — the host's own path for the same directory is not in the answer at all.
    assert_eq!(fx.console.scratch_path(), Some(Path::new("/scratch")));
    assert_eq!(
        fx.output("pwd").await.stdout,
        b"/scratch\n",
        "the guest stands in the tree it was given, by the name it has for it"
    );
    assert_ne!(
        root.to_str(),
        Some("/scratch"),
        "the host path and the guest path are different strings, which is the point"
    );

    // The guest reads what the host wrote, by the name it was told and by a relative one
    // from where it stands.
    assert_eq!(
        fx.output("cat /scratch/from-the-host").await.stdout,
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
        fx.read("/scratch/from-the-guest").await.data,
        b"hello from the guest\n"
    );

    // `cd` moves the session, and the next command runs where it left off — the guest
    // agent's state machine, in the path the client already knows. Nothing reports the
    // move: `pwd` is how it is observed, the way it is at a terminal.
    std::fs::create_dir(root.join("work")).expect("a subdirectory");
    assert_eq!(
        fx.console
            .exec(["cd", "work"], None)
            .await
            .expect("cd")
            .code,
        0
    );
    assert_eq!(fx.output("pwd").await.stdout, b"/scratch/work\n");
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

/// A credential the guest never holds is on the request that leaves — end to end, against the
/// real service.
///
/// The proof is two halves a client can see. Inside the guest the environment variable holds a
/// placeholder and not the key, so the value never crossed into the VM. Then a request built
/// from that placeholder is *accepted* by KOSIS: its `err:20` is "required parameters missing",
/// the answer to a recognised key, where an un-substituted placeholder earns `err:11`, "invalid
/// key". So the substitution happened on the way out, in the stack, where the key actually lives.
///
/// KOSIS and not OpenDART: OpenDART's server offers only RSA key-exchange TLS, which the
/// interception stack's rustls upstream refuses, so that request never completes. KOSIS speaks
/// TLS 1.3, which it does. The session is configured with the one via `CORTEX_UVM_SECRETS`.
///
/// `python:3.13-slim` because its `ssl` honours `SSL_CERT_FILE`, which is how the guest comes to
/// trust the interception CA the boot installs; the pinned rootfs ships no HTTPS client worth the
/// name. The key is the runner's, read from `KOSIS_API_KEY` — the boot inherits the same
/// environment and reads the real value there, so it is never on a command line or in the guest.
#[tokio::test]
#[ignore = "boots a micro-VM and calls KOSIS: needs libkrunfw, a hypervisor, the internet, and KOSIS_API_KEY"]
async fn a_credential_is_injected_outside_the_guest() {
    let key = std::env::var("KOSIS_API_KEY")
        .expect("set KOSIS_API_KEY to a real KOSIS key to run this test");

    // The placeholder the guest is handed, derived from the variable name by `SecretSpec`.
    const PLACEHOLDER: &str = "MSB_KOSIS_API_KEY";

    let mut fx = Fixture::with_env(&[
        ("CORTEX_UVM_NETWORK", "public"),
        ("CORTEX_UVM_IMAGE", "python:3.13-slim"),
        // What a deployment configures: the key goes to KOSIS, in the query. The value itself is
        // read by the boot from the environment below, never from this line.
        ("CORTEX_UVM_SECRETS", "KOSIS_API_KEY@kosis.kr:query"),
        ("KOSIS_API_KEY", &key),
    ])
    .await;

    // The guest holds the placeholder, and not the key. Compared here on the host: naming the
    // real key inside a guest command would be the one way to actually put it in there.
    let seen = fx.output(r#"printf '%s' "$KOSIS_API_KEY""#).await.stdout;
    let seen = String::from_utf8(seen).expect("an environment value that is text");
    assert_eq!(seen, PLACEHOLDER, "the guest did not get the placeholder");
    assert_ne!(seen, key, "the real key reached the guest's environment");

    // A request the guest builds from the placeholder, and the code KOSIS answers it with. `20`
    // is a recognised key with the rest of the query missing; `11` is the code for a key it does
    // not know, which is what an un-substituted placeholder would earn.
    let script = r#"python3 - <<'PY'
import json, os, urllib.request
ph = os.environ["KOSIS_API_KEY"]
url = "https://kosis.kr/openapi/statisticsData.do?method=getList&format=json&jsonVD=Y&apiKey=%s" % ph
body = urllib.request.urlopen(url, timeout=20).read().decode()
print("ERR", json.loads(body).get("err"), json.loads(body).get("errMsg"))
PY"#;
    let out = fx.output(script).await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("guest saw KOSIS_API_KEY={seen}");
    eprintln!("KOSIS answered: {}", stdout.trim());
    eprintln!(
        "probe stderr: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    assert_eq!(out.code, 0, "the probe did not run to completion");

    let err = stdout
        .split_whitespace()
        .nth(1)
        .expect("an ERR line from the probe");
    assert_ne!(
        err, "11",
        "KOSIS answered err:11 (invalid key): the placeholder was not substituted on the way out"
    );
}
