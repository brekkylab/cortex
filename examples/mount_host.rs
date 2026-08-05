//! Mount a cortex filesystem on the host and leave it up until interrupted.
//!
//! The host counterpart of `src/bin/apply_krun.rs`: same `Workspace`, same
//! `PosixFs`, a different interface in front of it.
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

#[cfg(any(feature = "fuse", feature = "fuse-t"))]
use cortex::HostMount;

#[cfg(any(feature = "fuse", feature = "fuse-t"))]
fn main() {
    use std::path::Path;

    use cortex::{FileExt, InMemVolume, Mountable, OpenOptions, Workspace};

    let mountpoint = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: mount_host <mountpoint>   (the directory must already exist)");
        std::process::exit(2);
    });

    let vol = InMemVolume::new();
    let (file, _) = vol
        .open(Path::new("hello.txt"), OpenOptions::create_new())
        .expect("fresh volume");
    file.write_all_at(b"Hello from cortex!\n", 0).unwrap();
    vol.mkdir(Path::new("sub")).unwrap();

    let workspace = Workspace::new()
        .try_with_mount("", vol)
        .expect("the empty mount path never escapes the workspace root");

    let mount = HostMount::spawn(workspace, &mountpoint).expect("mount");
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
