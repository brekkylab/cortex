//! Mount a cortex filesystem on the host and leave it up until interrupted.
//!
//! The host counterpart of `src/bin/apply_krun.rs`: same `WorkFs`, same `Posix`, a different
//! interface in front of it.
//!
//! One example for both host bindings, because the only difference between them
//! here is which guard type is constructed, and two copies of the same program
//! drift apart.
//!
//! ```sh
//! # FUSE-T (macOS, no kernel extension — `brew install --cask fuse-t`)
//! PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
//!     cargo run --features fuse-t --example mount_host -- /tmp/cortex
//!
//! # fuser (Linux natively; macOS needs macFUSE)
//! PKG_CONFIG_PATH=$PWD/contrib/pkgconfig:/usr/local/lib/pkgconfig \
//!     cargo run --features fuse --example mount_host -- /tmp/cortex
//! ```
//!
//! The fuser path needs a libfuse provider that speaks the kernel protocol over
//! the mount fd. See `tests/host_mount.rs` for why FUSE-T does not qualify for
//! that one and has its own binding instead.
//!
//! Deliberately no `required-features` in `Cargo.toml`: that key is AND, so it
//! cannot say "fuse or fuse-t" and naming either would lock the other out. The
//! no-feature build gets the `main` at the bottom instead.

#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
use cortex::fs::FuseMount as HostMount;
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
/// Whichever host binding this build has. FUSE-T wins a tie, needing no kernel extension.
///
/// A consumer cannot express this itself: features are a crate-level concept, so "whichever of
/// the two is enabled" has no spelling outside the crate that declares them.
#[cfg(feature = "fuse-t")]
use cortex::fs::FuseTMount as HostMount;

#[cfg(any(feature = "fuse", feature = "fuse-t"))]
fn main() {
    use std::path::Path;

    // `Mount` for `mountpoint` below: what a guard *is* comes from the trait, not from the
    // binding that made it.
    use cortex::fs::{FileSystem, InMemFs, Mount, WorkFs};

    let mountpoint = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: mount_host <mountpoint>   (the directory must already exist)");
        std::process::exit(2);
    });

    let vol = InMemFs::new();
    // The store's data plane is async; drive this one-off setup on a throwaway runtime before
    // mounting.
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let hello = Path::new("hello.txt");
        vol.create(hello).await.expect("fresh store");
        vol.write_at(hello, b"Hello from cortex!\n", 0)
            .await
            .unwrap();
        vol.mkdir(Path::new("sub")).await.unwrap();
    });

    let workspace = WorkFs::new()
        .try_with_mount("", vol)
        .expect("the empty mount path never escapes the workspace root");

    let mount = HostMount::try_new(workspace, Path::new(&mountpoint)).expect("mount");
    println!("mounted at {}", mount.mountpoint().display());
    println!("  ls {mountpoint}");
    println!("  cat {mountpoint}/hello.txt");
    println!("  echo written > {mountpoint}/new.txt && cat {mountpoint}/new.txt");
    println!("press ctrl-c to unmount");

    // The guard unmounts on drop, so park here and let ctrl-c end the process.
    loop {
        std::thread::park();
    }
}

#[cfg(not(any(feature = "fuse", feature = "fuse-t")))]
fn main() {
    eprintln!("build with --features fuse or --features fuse-t");
    std::process::exit(2);
}
