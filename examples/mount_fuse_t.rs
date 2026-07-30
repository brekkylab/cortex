//! Mount a cortex filesystem on macOS through FUSE-T — no kernel extension.
//!
//! ```sh
//! export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig
//! cargo run --features fuse-t --example mount_fuse_t -- /tmp/cortex
//! ```
use cortex::{FileExt, FuseTMount, InMemVolume, Mountable, OpenOptions, PosixFs, Workspace};
use std::path::Path;

fn main() {
    let mountpoint = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: mount_fuse_t <mountpoint>   (must already exist)");
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
        .expect("root mount");
    let mount = FuseTMount::spawn(PosixFs::new(workspace), &mountpoint).expect("mount");
    println!("mounted at {}", mount.mountpoint().display());
    println!("press ctrl-c to unmount");
    loop {
        std::thread::park();
    }
}
