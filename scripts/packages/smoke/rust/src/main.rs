//! The crate as a user takes it, on the platform this runs on -- as `node.mjs` and
//! `python.py` are for the bindings, and told what this platform can do the same way:
//!
//!   SMOKE_MOUNT   1 if a FUSE provider is installed here, else 0
//!   SMOKE_VM      1 if this machine can boot one (KVM or HVF), else 0
use virtx::console::ConsoleClient;
use virtx::fs::{Directory, mount_support};
use virtx::image::{ImageClient, Recipe};

#[cfg(windows)]
use virtx::fs::DokanMount as HostMount;
#[cfg(target_os = "linux")]
use virtx::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use virtx::fs::FuseTMount as HostMount;

fn want(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut failures = Vec::new();
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failures.push(what.to_string());
        }
    };

    let fuse = match mount_support() {
        Ok(()) => true,
        Err(e) => {
            println!("  mount_support: {e}");
            false
        }
    };
    check(fuse == want("SMOKE_MOUNT"), &format!("mount_support says {}", if fuse { "yes" } else { "no" }));

    // A mount, whether or not this host can make one: it links the provider's code into this
    // binary, which is what a delay-load is for. Reaching here at all is the binary starting
    // without the provider -- on Windows, the `/DELAYLOAD` in `build.rs` -- and without one, a
    // mount has to be an error rather than a crash.
    let point = std::env::temp_dir().join(format!("virtx-smoke-rust-mount-{}", std::process::id()));
    std::fs::create_dir_all(&point)?;
    let tree = Directory::new().with_file("a.txt", "hi".as_bytes())?;
    match HostMount::try_new(tree, &point) {
        Ok(mount) => {
            check(fuse, "a HostMount is made only where mount_support says yes");
            let read = std::fs::read_to_string(point.join("a.txt")).unwrap_or_default();
            check(read == "hi", "HostMount serves its tree");
            drop(mount);
            check(!point.join("a.txt").exists(), "dropping the HostMount takes it down");
        }
        Err(e) => {
            println!("  HostMount: {e}");
            check(!fuse, "a HostMount without a provider is an error, not a crash");
        }
    }
    let _ = std::fs::remove_dir_all(&point);

    // The release this crate was packed with, unless the environment says otherwise -- see
    // `build.rs`.
    let bin = virtx::ensure_virtx().await?;
    println!("  ensure_virtx: {}", bin.display());
    let exe = if cfg!(windows) { ".exe" } else { "" };
    check(bin.join(format!("virtx-uvm{exe}")).is_file(), "ensure_virtx fetched the server");

    let mut images = ImageClient::try_new().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let version = images.version().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    drop(images);
    check(!version.is_empty(), &format!("the server answers (protocol {version})"));

    if want("SMOKE_VM") {
        let host = std::env::temp_dir().join(format!("virtx-smoke-rust-{}", std::process::id()));
        std::fs::create_dir_all(&host)?;
        std::fs::write(host.join("from-host.txt"), "by path")?;
        let mut console = ConsoleClient::builder()
            .image(Recipe::new("alpine:latest"))
            .mount(host.clone(), "/host")
            .build()
            .await?;
        let r = console
            .exec(["sh", "-c", "uname -m; cat /host/from-host.txt; echo written > /host/from-vm.txt"], None)
            .await?;
        let out = String::from_utf8_lossy(&r.stdout);
        println!("  vm: {}", out.trim().replace('\n', " | "));
        check(r.code == 0 && out.contains("by path"), "a VM session reads its mount");
        let wrote = std::fs::read_to_string(host.join("from-vm.txt")).unwrap_or_default();
        check(wrote.trim() == "written", "the host sees the VM's write");
        drop(console);
        let _ = std::fs::remove_dir_all(&host);
    }

    if failures.is_empty() {
        println!("ALL PASS");
        Ok(())
    } else {
        anyhow::bail!("FAILED: {}", failures.join("; "))
    }
}
