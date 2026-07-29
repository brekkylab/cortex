//! A real host mount, driven through the operating system.
//!
//! Every other test calls the bindings directly, which proves only that the code
//! answers correctly when *we* ask. This is the one place a kernel asks: real FUSE
//! opcodes over a real mount, in whatever order and with whatever flags the OS
//! chooses.
//!
//! `#[ignore]` because it needs a mount provider and touches the real
//! filesystem:
//!
//! ```sh
//! # macOS, no kernel extension (recommended):
//! PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
//!     cargo test --features fuse-t --test host_mount -- --ignored --nocapture
//!
//! # Linux, or macOS with macFUSE:
//! cargo test --features fuse --test host_mount -- --ignored --nocapture
//! ```
//!
//! # Which binding to use where
//!
//! | Platform | Binding | Feature |
//! |---|---|---|
//! | Linux | `fuser`, straight to `/dev/fuse` | `fuse` |
//! | macOS + FUSE-T | libfuse-t's own session loop | `fuse-t` |
//! | macOS + macFUSE | `fuser` (its macOS path targets macFUSE 4.x) | `fuse` |
//!
//! **`fuser` cannot drive FUSE-T** — see `adapter/fuse_t.rs`, which exists for
//! exactly that reason.
//!
//! A missing provider fails at *build* time (both features probe with
//! pkg-config), never by silently passing: a test that quietly does not run is
//! worse than one that fails.

#![cfg(any(feature = "fuse", feature = "fuse-t"))]

use std::fs;
use std::path::PathBuf;

use cortex::{InMemVolume, Mountable, OpenOptions, PosixFs};

// One set of test bodies for both host bindings: they expose the same call
// surface, so which is under test is a matter of which feature is on — making
// these tests evidence that the two behave *alike*, not just that each behaves.
// FUSE-T wins a tie, being the one that needs no kernel extension.
#[cfg(feature = "fuse-t")]
use cortex::FuseTMount as Mount;
#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
use cortex::CortexMount as Mount;

/// A mount point of our own. The guards do not create it — no mount does.
fn mountpoint(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("cortex-mount-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir is writable");
    dir
}

/// A volume with one file and one empty directory.
fn volume() -> InMemVolume {
    let vol = InMemVolume::new();
    let (file, _) = vol
        .open(
            std::path::Path::new("greeting.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    cortex::FileExt::write_all_at(&file, b"Hello from cortex!\n", 0).unwrap();
    vol.mkdir(std::path::Path::new("sub")).unwrap();
    vol
}

#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn the_operating_system_can_read_a_cortex_mount() {
    let mnt = mountpoint("read");
    let mount = Mount::spawn(PosixFs::new(volume()), &mnt).expect("mount");

    // `read_dir` is a real `readdir`, so this exercises the cursor protocol as
    // the kernel drives it rather than as our unit tests drive it.
    let mut names: Vec<_> = fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["greeting.txt", "sub"]);

    assert_eq!(
        fs::read_to_string(mnt.join("greeting.txt")).unwrap(),
        "Hello from cortex!\n"
    );

    // Attribute projection, as the OS reports it back.
    let meta = fs::metadata(mnt.join("greeting.txt")).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len(), 19);
    assert!(fs::metadata(mnt.join("sub")).unwrap().is_dir());

    mount.unmount().expect("unmount");
    fs::remove_dir_all(&mnt).ok();
}

#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn the_operating_system_can_write_to_a_cortex_mount() {
    let mnt = mountpoint("write");
    let mount = Mount::spawn(PosixFs::new(volume()), &mnt).expect("mount");

    // `fs::write` is create + write + close, so this covers the whole chain the
    // kernel actually sends: CREATE, WRITE, FLUSH, RELEASE.
    fs::write(mnt.join("new.txt"), b"written by the kernel").unwrap();
    assert_eq!(
        fs::read_to_string(mnt.join("new.txt")).unwrap(),
        "written by the kernel"
    );

    // Truncating an *existing* file: with `ATOMIC_O_TRUNC` negotiated `O_TRUNC`
    // rides the open, and dropping the flag leaves the old tail in place.
    fs::write(mnt.join("greeting.txt"), b"replaced").unwrap();
    assert_eq!(
        fs::read_to_string(mnt.join("greeting.txt")).unwrap(),
        "replaced"
    );

    // Explicit resize, which arrives as a `setattr` rather than on the open.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(mnt.join("new.txt"))
        .unwrap();
    file.set_len(7).unwrap();
    drop(file);
    assert_eq!(fs::read_to_string(mnt.join("new.txt")).unwrap(), "written");

    // The `unlink`/`rmdir` split as the kernel drives it — what `rm -r` becomes.
    fs::create_dir(mnt.join("made")).unwrap();
    fs::write(mnt.join("made/inside"), b"x").unwrap();
    assert!(
        fs::remove_dir(mnt.join("made")).is_err(),
        "a non-empty directory must not be removed"
    );
    fs::remove_file(mnt.join("made/inside")).unwrap();
    fs::remove_dir(mnt.join("made")).unwrap();
    assert!(!mnt.join("made").exists());

    fs::remove_file(mnt.join("new.txt")).unwrap();
    assert!(!mnt.join("new.txt").exists());

    mount.unmount().expect("unmount");
    fs::remove_dir_all(&mnt).ok();
}

/// `fuser`-only: mount options are part of its call surface, and FUSE-T's are a
/// different set. The behaviour under test is the kernel's, not ours.
#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn a_read_only_mount_is_enforced_by_the_kernel() {
    let mnt = mountpoint("readonly");
    let mount = Mount::spawn_with(
        PosixFs::new(volume()),
        &mnt,
        vec![
            cortex::MountOption::FSName("cortex".into()),
            cortex::MountOption::RO,
        ],
    )
    .expect("mount");

    // The write is refused by the *kernel*: the request never reaches a backend,
    // which is stronger than each backend answering `ReadOnly` by hand.
    assert!(fs::read_to_string(mnt.join("greeting.txt")).is_ok());
    assert!(fs::write(mnt.join("nope.txt"), b"x").is_err());

    mount.unmount().expect("unmount");
    fs::remove_dir_all(&mnt).ok();
}
