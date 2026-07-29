//! Mount a cortex filesystem on the host and leave it up until interrupted.
//!
//! The host-FUSE counterpart of `src/bin/apply_krun.rs`: same `Workspace`, same
//! `PosixFs`, a different interface in front of it.
//!
//! ```sh
//! export PKG_CONFIG_PATH=$PWD/contrib/pkgconfig:/usr/local/lib/pkgconfig
//! cargo run --features fuse --example mount_fuse -- /tmp/cortex
//! ```
//!
//! Needs a libfuse provider that speaks the kernel protocol over the mount fd —
//! in practice macFUSE on macOS, or nothing extra on Linux. See
//! `tests/host_mount.rs` for why FUSE-T does not qualify.

use std::path::Path;

use cortex::{CortexMount, FileExt, InMemVolume, Mountable, OpenOptions, PosixFs, Workspace};

fn main() {
    let mountpoint = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: mount_fuse <mountpoint>   (the directory must already exist)");
        std::process::exit(2);
    });

    let vol = InMemVolume::new();
    let (file, _) = vol
        .open(
            Path::new("hello.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .expect("fresh volume");
    file.write_all_at(b"Hello from cortex!\n", 0).unwrap();
    vol.mkdir(Path::new("sub")).unwrap();

    let workspace = Workspace::new()
        .try_with_mount("", vol)
        .expect("the empty mount path never escapes the workspace root");

    let mount = CortexMount::spawn(PosixFs::new(workspace), &mountpoint).expect("mount");
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
