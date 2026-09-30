//! A real host mount, driven through the operating system.
//!
//! Here the kernel asks, not the test: real requests over a real mount, in whatever order and
//! with whatever flags the OS chooses.
//!
//! ```sh
//! cargo test --test host_mount -- --ignored --nocapture
//! ```
//!
//! The same bodies run through whichever binding the target has (`FuseMount` on Linux,
//! `FuseTMount` on macOS, `DokanMount` on Windows). The Windows one reaches `FileSystem`
//! directly while the FUSE ones go through `Posix`, so this checks that a cortex tree behaves
//! the same whichever kernel asks.
//!
//! **A missing provider fails, never silently skips.** On macOS FUSE-T is probed with
//! pkg-config and fails at build time; Dokany's driver is a runtime fact, so there `try_new`
//! fails with `mount_support`'s install instructions.

#![cfg(all(feature = "mount", any(unix, windows)))]

use std::{fs, path::PathBuf};

// Every binding exposes the same call surface, so the target picks which is under test.
#[cfg(windows)]
use cortex::fs::DokanMount as HostMount;
#[cfg(all(unix, not(target_os = "macos")))]
use cortex::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use cortex::fs::FuseTMount as HostMount;
use cortex::fs::{Directory, FileSystem, InMemFs};

/// A fresh mount point; no guard creates one.
fn mountpoint(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("cortex-mount-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir is writable");
    dir
}

/// A volume with one file and one empty directory.
///
/// Seeded on a throwaway runtime, since the crate's `block_on` is `pub(crate)`; the test
/// bodies are synchronous because a real kernel drives the mount.
fn volume() -> InMemFs {
    let vol = InMemFs::new();
    let rt = tokio::runtime::Runtime::new().expect("build a runtime for volume setup");
    rt.block_on(async {
        let greeting = std::path::Path::new("greeting.txt");
        vol.create(greeting).await.expect("fresh store");
        vol.write_at(greeting, b"Hello from cortex!\n", 0)
            .await
            .expect("write the greeting");
        vol.mkdir(std::path::Path::new("sub")).await.unwrap();
    });
    vol
}

#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn the_operating_system_can_read_a_cortex_mount() {
    let mnt = mountpoint("read");
    let mount = HostMount::try_new(volume(), &mnt).expect("mount");

    // A real `readdir`, so the kernel drives the cursor protocol.
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

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

/// Host directories side by side over the tree's in-memory root, the workspace shape.
///
/// The only test where a real kernel drives the tree's mount table; the rest mount one store.
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn the_operating_system_can_read_a_multi_source_workspace() {
    let mnt = mountpoint("workspace");
    let (project, notes) = (host_dir(), host_dir());
    let mut ws = Directory::new();
    ws.mount("project", project.path()).expect("a fresh path");
    ws.mount("deep/notes", notes.path()).expect("a fresh path");
    ws.add_file("readme.md", "in memory\n".as_bytes())
        .expect("outside every mount");
    let mount = HostMount::try_new(ws, &mnt).expect("mount");

    // The mount points show up beside the in-memory file, and the directory leading to a
    // deeper one is there too.
    let mut top: Vec<_> = fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    assert_eq!(top, ["deep", "project", "readme.md"]);
    assert!(fs::metadata(mnt.join("deep/notes")).unwrap().is_dir());
    assert_eq!(
        fs::read_to_string(mnt.join("readme.md")).unwrap(),
        "in memory\n"
    );

    // ...and the kernel can walk into one and read through to the host.
    assert_eq!(
        fs::read_to_string(mnt.join("project/greeting.txt")).unwrap(),
        "Hello from cortex!\n"
    );
    assert!(fs::metadata(mnt.join("deep/notes/sub")).unwrap().is_dir());

    // A write under a mount lands in its host directory and nowhere else.
    fs::write(mnt.join("deep/notes/fresh.txt"), b"only here").unwrap();
    assert!(notes.path().join("fresh.txt").exists());
    assert!(!mnt.join("project/fresh.txt").exists());

    // What lands beside the mount points is kept in memory and reaches neither of them.
    fs::create_dir(mnt.join("scratch")).unwrap();
    assert!(fs::metadata(mnt.join("scratch")).unwrap().is_dir());
    assert!(!project.path().join("scratch").exists());

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

/// A host directory with the same one file and one empty directory as [`volume`].
fn host_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir is writable");
    fs::write(dir.path().join("greeting.txt"), "Hello from cortex!\n").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    dir
}

/// Timestamps as the operating system reports them back.
///
/// A frozen `mtime` is not cosmetic: a guest negotiating `AUTO_INVAL_DATA` decides from
/// `mtime` alone when to drop cached pages, and `find -newer`, `make` and `rsync` read it.
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn the_operating_system_sees_real_timestamps() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let mnt = mountpoint("times");
    let started = SystemTime::now();
    let mount = HostMount::try_new(volume(), &mnt).expect("mount");

    let modified = |name: &str| {
        fs::metadata(mnt.join(name))
            .unwrap()
            .modified()
            .expect("the OS reports a modification time")
    };

    for name in ["greeting.txt", "sub"] {
        let m = modified(name);
        assert_ne!(m, UNIX_EPOCH, "{name} is stuck at the epoch");
        // A second of slack: some kernel paths report whole seconds.
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

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

/// Editing an existing file, which is a rename and not a write.
///
/// Editors write a temporary beside the target and rename it over, so a crash cannot leave a
/// half-written file. Without `rename`, libfuse-t's default answers `EACCES` and the edit is
/// stranded in the `.tmp`.
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn an_editor_can_save_over_a_file_on_a_cortex_mount() {
    let mnt = mountpoint("rename");
    let mount = HostMount::try_new(volume(), &mnt).expect("mount");

    // Write-temp-then-rename, as an editor does.
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

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn the_operating_system_can_write_to_a_cortex_mount() {
    let mnt = mountpoint("write");
    let mount = HostMount::try_new(volume(), &mnt).expect("mount");

    // `fs::write` makes the kernel send CREATE, WRITE, FLUSH, RELEASE.
    fs::write(mnt.join("new.txt"), b"written by the kernel").unwrap();
    assert_eq!(
        fs::read_to_string(mnt.join("new.txt")).unwrap(),
        "written by the kernel"
    );

    // Truncating an *existing* file: with `ATOMIC_O_TRUNC`, `O_TRUNC` rides the open, and
    // dropping the flag would leave the old tail.
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

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

/// `>>` through a real mount. The contract has no append flag: the kernel resolves `O_APPEND`
/// and sends the absolute end offset. The only write here whose offset the test does not
/// choose.
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn the_operating_system_can_append_to_a_cortex_mount() {
    use std::io::Write;

    let mnt = mountpoint("append");
    let mount = HostMount::try_new(volume(), &mnt).expect("mount");

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(mnt.join("greeting.txt"))
        .unwrap();
    file.write_all(b"and again\n").unwrap();
    drop(file);

    assert_eq!(
        fs::read_to_string(mnt.join("greeting.txt")).unwrap(),
        "Hello from cortex!\nand again\n"
    );

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

/// A write-protected Dokany volume refuses writes in the driver, before any store answers.
#[cfg(windows)]
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn a_write_protected_volume_is_enforced_by_the_driver() {
    let mnt = mountpoint("readonly");
    let mount = HostMount::try_new_with(volume(), &mnt, cortex::fs::MountFlags::WRITE_PROTECT)
        .expect("mount");

    // Refused by the *driver*; no store sees the request.
    assert!(fs::read_to_string(mnt.join("greeting.txt")).is_ok());
    assert!(fs::write(mnt.join("nope.txt"), b"x").is_err());

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}

/// A mount point is handed back the way it was taken, so the same directory can be mounted
/// again.
///
/// The second mount is the assertion: anything left at the mount point would make a directory
/// mountable once only.
///
/// On Windows `DokanRemoveMountPoint` leaves a reparse point onto the gone volume: absent from
/// its parent's listing, unopenable, and refused with `ERROR_ALREADY_EXISTS` by the next
/// `create_dir`. Needs no fork or signal, so it runs on every platform that mounts.
///
/// Checked by listing the *parent*, since the leftover answers `metadata` with "not found",
/// the same as a clean unmount.
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn a_mount_point_can_be_mounted_again_after_the_guard_is_dropped() {
    let mnt = mountpoint("reuse");

    let mount = HostMount::try_new(volume(), &mnt).expect("first mount");
    assert!(fs::read_to_string(mnt.join("greeting.txt")).is_ok());
    drop(mount);

    // Still listed by its parent, and empty.
    let named = mnt.file_name().expect("the mount point is named");
    let parent = mnt.parent().expect("the mount point has a parent");
    assert!(
        fs::read_dir(parent)
            .expect("list the mount point's parent")
            .any(|e| e.expect("read the entry").file_name() == named),
        "{} is gone from its parent after the unmount",
        mnt.display()
    );
    assert_eq!(
        fs::read_dir(&mnt)
            .expect("the unmounted mount point is a directory again")
            .count(),
        0,
        "{} still has something in it after the unmount",
        mnt.display()
    );

    // It takes a mount again.
    let mount = HostMount::try_new(volume(), &mnt).expect("second mount on the same path");
    assert!(fs::read_to_string(mnt.join("greeting.txt")).is_ok());

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}
/// `fuser`-only, since mount options are part of its call surface. The kernel's behaviour is
/// under test, not ours.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
#[ignore = "needs a mount provider and mounts a real filesystem"]
fn a_read_only_mount_is_enforced_by_the_kernel() {
    let mnt = mountpoint("readonly");
    let mount = HostMount::try_new_with(
        volume(),
        &mnt,
        vec![
            cortex::fs::MountOption::FSName("cortex".into()),
            cortex::fs::MountOption::RO,
        ],
    )
    .expect("mount");

    // Refused by the *kernel*; no store sees the request.
    assert!(fs::read_to_string(mnt.join("greeting.txt")).is_ok());
    assert!(fs::write(mnt.join("nope.txt"), b"x").is_err());

    drop(mount);
    fs::remove_dir_all(&mnt).ok();
}
