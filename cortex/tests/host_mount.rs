//! A real host mount, driven through the operating system.
//!
//! Every other test calls the bindings directly, which proves only that the code
//! answers correctly when *we* ask. This is the one place a kernel asks: real FUSE
//! opcodes over a real mount, in whatever order and with whatever flags the OS
//! chooses.
//!
//! `#[ignore]` because it needs a mount provider and touches the real
//! filesystem. **`--test-threads=1` is required**, not a preference — see below:
//!
//! ```sh
//! # macOS, no kernel extension (recommended). The pkg-config shim in
//! # contrib/ is what lets fuser's macOS branch find FUSE-T.
//! PKG_CONFIG_PATH="$PWD/contrib/pkgconfig:/usr/local/lib/pkgconfig" \
//!     cargo test --features fuse-t --test host_mount \
//!     -- --ignored --nocapture --test-threads=1
//!
//! # Linux, or macOS with macFUSE:
//! cargo test --features fuse --test host_mount -- --ignored --test-threads=1
//! ```
//!
//! # Why the tests must not run in parallel
//!
//! Each body mounts a real filesystem, and FUSE-T serves it through a `go-nfsv4`
//! helper. Three of these coming up at once wedges: the run hangs with the mounts
//! half-established and has to be killed and `umount`ed by hand. Two happened to
//! survive, which is exactly the kind of margin that makes this look like a code
//! bug the first time someone adds a third test.
//!
//! `cargo test` uses a thread per test by default, so the flag is not optional.
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

use cortex::volume::{InMemVolume, Mountable, OpenOptions, Workspace};

// One set of test bodies for both host bindings: they expose the same call
// surface, so which is under test is a matter of which feature is on — making
// these tests evidence that the two behave *alike*, not just that each behaves.
use cortex::volume::HostMount;

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
            OpenOptions::create_new(),
        )
        .unwrap();
    cortex::volume::FileExt::write_all_at(&file, b"Hello from cortex!\n", 0).unwrap();
    vol.mkdir(std::path::Path::new("sub")).unwrap();
    vol
}

#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn the_operating_system_can_read_a_cortex_mount() {
    let mnt = mountpoint("read");
    let mount = HostMount::spawn(volume(), &mnt).expect("mount");

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

/// Several sources side by side with nothing mounted at the root — the shape a
/// workspace exists for, and the one that could not be mounted at all before the
/// directories leading to a mount point were synthesized: the kernel's opening
/// `getattr` on the root failed, so `mount` never returned a usable filesystem.
///
/// Also the only place a real kernel drives `Workspace`'s erased handle
/// (`Handle = Box<dyn FileHandle>`); every other test here mounts a backend whose
/// handle type is concrete.
#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn the_operating_system_can_read_a_multi_source_workspace() {
    let mnt = mountpoint("workspace");
    let ws = Workspace::new()
        .try_with_mount("s3-like", volume())
        .expect("mount path stays inside the workspace")
        .try_with_mount("notes", volume())
        .expect("mount path stays inside the workspace");
    let mount = HostMount::spawn(ws, &mnt).expect("mount");

    // The mount points show up as directories even though no backend serves the
    // directory that holds them.
    let mut top: Vec<_> = fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    assert_eq!(top, ["notes", "s3-like"]);
    assert!(fs::metadata(&mnt).unwrap().is_dir());
    assert!(fs::metadata(mnt.join("notes")).unwrap().is_dir());

    // ...and the kernel can walk into one and read through it.
    assert_eq!(
        fs::read_to_string(mnt.join("s3-like/greeting.txt")).unwrap(),
        "Hello from cortex!\n"
    );
    assert!(fs::metadata(mnt.join("notes/sub")).unwrap().is_dir());

    // Each mount is its own namespace: writing under one leaves the other alone.
    fs::write(mnt.join("notes/fresh.txt"), b"only here").unwrap();
    assert!(!mnt.join("s3-like/fresh.txt").exists());

    // The synthesized root is not writable — nothing is mounted there to hold it.
    assert!(fs::create_dir(mnt.join("nope")).is_err());

    mount.unmount().expect("unmount");
    fs::remove_dir_all(&mnt).ok();
}

/// Timestamps as the operating system reports them back.
///
/// A backend that never advances `mtime` looks frozen at the UNIX epoch, which is
/// not cosmetic: a guest negotiating `AUTO_INVAL_DATA` decides from `mtime` alone
/// when to drop cached pages, and `find -newer`, `make` and `rsync` all read it.
#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn the_operating_system_sees_real_timestamps() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let mnt = mountpoint("times");
    let started = SystemTime::now();
    let mount = HostMount::spawn(volume(), &mnt).expect("mount");

    let modified = |name: &str| {
        fs::metadata(mnt.join(name))
            .unwrap()
            .modified()
            .expect("the OS reports a modification time")
    };

    for name in ["greeting.txt", "sub"] {
        let m = modified(name);
        assert_ne!(m, UNIX_EPOCH, "{name} is stuck at the epoch");
        // A second of slack: the mount round-trips through the kernel, which
        // reports whole-second granularity on some paths.
        assert!(
            m + Duration::from_secs(1) >= started,
            "{name} predates the volume"
        );
    }

    // A write through the mount has to move the file's timestamp forward.
    let before = modified("greeting.txt");
    fs::write(mnt.join("greeting.txt"), b"rewritten").unwrap();
    assert!(
        modified("greeting.txt") >= before,
        "a write must not leave mtime behind"
    );

    // ...and adding a name has to move the directory's.
    let dir_before = modified("sub");
    fs::write(mnt.join("sub/fresh.txt"), b"new name").unwrap();
    assert!(
        modified("sub") >= dir_before,
        "creating an entry modifies its directory"
    );

    mount.unmount().expect("unmount");
    fs::remove_dir_all(&mnt).ok();
}

/// Editing an existing file, which is a rename and not a write.
///
/// Editors do not overwrite in place — they write a temporary beside the target
/// and rename it over, so the replacement is atomic and a crash cannot leave a
/// half-written file. Before `rename` was wired this failed with `EACCES`
/// ("Permission denied", from libfuse-t's default), leaving the edit stranded in
/// the `.tmp` and the original untouched.
#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn an_editor_can_save_over_a_file_on_a_cortex_mount() {
    let mnt = mountpoint("rename");
    let mount = HostMount::spawn(volume(), &mnt).expect("mount");

    // Write-temp-then-rename, exactly as an editor does.
    fs::write(mnt.join("greeting.txt.tmp"), b"edited by an editor\n").unwrap();
    fs::rename(mnt.join("greeting.txt.tmp"), mnt.join("greeting.txt")).expect("atomic replace");

    assert_eq!(
        fs::read_to_string(mnt.join("greeting.txt")).unwrap(),
        "edited by an editor\n"
    );
    assert!(
        !mnt.join("greeting.txt.tmp").exists(),
        "the temporary must be gone, not left as litter"
    );

    // A plain rename, and one that moves a whole directory with its contents.
    fs::rename(mnt.join("greeting.txt"), mnt.join("renamed.txt")).unwrap();
    assert!(fs::metadata(mnt.join("renamed.txt")).unwrap().is_file());
    assert!(!mnt.join("greeting.txt").exists());

    fs::write(mnt.join("sub/inner.txt"), b"nested").unwrap();
    fs::rename(mnt.join("sub"), mnt.join("moved")).expect("directory rename");
    assert_eq!(
        fs::read_to_string(mnt.join("moved/inner.txt")).unwrap(),
        "nested",
        "a descendant is still readable through the new name"
    );

    // The mismatched pairs the kernel asks about, answered by the backend.
    fs::create_dir(mnt.join("busy")).unwrap();
    fs::write(mnt.join("busy/occupied"), b"x").unwrap();
    assert!(
        fs::rename(mnt.join("moved"), mnt.join("busy")).is_err(),
        "a directory must not replace a non-empty one"
    );
    assert!(
        fs::rename(mnt.join("renamed.txt"), mnt.join("moved")).is_err(),
        "a file must not replace a directory"
    );

    mount.unmount().expect("unmount");
    fs::remove_dir_all(&mnt).ok();
}

#[test]
#[ignore = "needs a libfuse provider and mounts a real filesystem"]
fn the_operating_system_can_write_to_a_cortex_mount() {
    let mnt = mountpoint("write");
    let mount = HostMount::spawn(volume(), &mnt).expect("mount");

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
    let mount = HostMount::spawn_with(
        volume(),
        &mnt,
        vec![
            cortex::volume::MountOption::FSName("cortex".into()),
            cortex::volume::MountOption::RO,
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
