//! Baseline for the shared-operation migration.
//!
//! `DynFileSystem` returns `io::Result`, so the binding can be driven here
//! with no VM and no mount — unlike the `fuser` side, whose reply objects
//! cannot be built outside that crate. These tests pin the behaviour that
//! must survive being rewritten on top of `PosixFs`'s shared operations:
//! which inode a name resolves to, what the guest reads back, and which
//! Linux errno a failure carries.
//!
//! Attribute *values* are pinned in `posix.rs`, where the shared policy lives.
//! What is pinned here is the projection onto the guest's `stat64` — that the
//! numbers reach the right fields, including those once left zeroed.

use super::*;
// Named explicitly: `msb_krun::backends::fs::OpenOptions` is a different
// type with the same name, and `super::*` brings the module into scope.
use crate::mountable::{FileExt, OpenOptions};
use crate::{InMemVolume, PosixFs};
use msb_krun::backends::fs::Extensions;
use msb_krun::backends::fs::ZeroCopyWriter;
use std::fs::File;
use std::path::Path;

const CONTENT: &[u8] = b"Hello from cortex!\n";
const ROOT: u64 = 1;

/// Collects what the filesystem hands back, standing in for the guest's
/// descriptor. `ZeroCopyWriter` copies out of a file descriptor rather than
/// a slice, which is why the binding stages reads through a temp file.
struct Collected(Vec<u8>);

impl ZeroCopyWriter for Collected {
    fn write_from(&mut self, f: &File, count: usize, off: u64) -> std::io::Result<usize> {
        let mut buf = vec![0u8; count];
        let n = std::os::unix::fs::FileExt::read_at(f, &mut buf, off)?;
        self.0.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

fn ctx() -> Context {
    Context {
        uid: 0,
        gid: 0,
        pid: 0,
    }
}

/// `greeting.txt` at the root plus an empty `sub/`.
fn fs() -> PosixFs<InMemVolume> {
    let vol = InMemVolume::new();
    let (file, _) = vol
        .open(
            Path::new("greeting.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    file.write_all_at(CONTENT, 0).unwrap();
    vol.mkdir(Path::new("sub")).unwrap();
    PosixFs::new(vol)
}

/// The errno behind a call that must fail, for asserting against the Linux
/// numbers the guest kernel expects (which are not the host's).
///
/// Matches rather than using `unwrap_err`, which would need `Entry` and
/// `stat64` to implement `Debug`; neither does.
fn expect_errno<T>(result: std::io::Result<T>) -> i32 {
    match result {
        Ok(_) => panic!("expected this call to fail"),
        Err(err) => err
            .raw_os_error()
            .expect("filesystem errors carry an errno"),
    }
}

/// Guest-side open flags, in the guest's (Linux) numbering.
const GUEST_O_WRONLY: u32 = 1;
const GUEST_O_TRUNC: u32 = 0o1000;

/// Feeds bytes from a file descriptor — the mirror of [`Collected`].
struct Supplied(Vec<u8>);

impl msb_krun::backends::fs::ZeroCopyReader for Supplied {
    fn read_to(&mut self, f: &File, count: usize, off: u64) -> std::io::Result<usize> {
        let n = count.min(self.0.len());
        std::os::unix::fs::FileExt::write_at(f, &self.0[..n], off)?;
        Ok(n)
    }
}

#[test]
fn the_guest_can_create_write_and_read_back() {
    let fs = fs();

    // CREATE: one reply carrying entry, handle, and attributes.
    let (entry, handle, _) = fs
        .create(
            ctx(),
            ROOT,
            c"fresh.txt",
            0o644,
            false,
            GUEST_O_WRONLY,
            0,
            Extensions::default(),
        )
        .unwrap();
    let handle = handle.expect("create hands back a file handle");
    assert_eq!(entry.attr.st_size, 0);

    // WRITE through the ZeroCopy path: the guest delivers via a file
    // descriptor, not a slice, which is why this binding stages.
    let mut source = Supplied(b"written".to_vec());
    let n = fs
        .write(
            ctx(),
            entry.inode,
            handle,
            &mut source,
            7,
            0,
            None,
            false,
            false,
            0,
        )
        .unwrap();
    assert_eq!(n, 7);

    // The new size is visible through the same inode.
    let (attr, _) = fs.getattr(ctx(), entry.inode, Some(handle)).unwrap();
    assert_eq!(attr.st_size, 7);

    // And the bytes read back.
    let mut sink = Collected(Vec::new());
    fs.read(ctx(), entry.inode, handle, &mut sink, 7, 0, None, 0)
        .unwrap();
    assert_eq!(sink.0, b"written");

    // The name resolves to the same inode a `lookup` would give.
    assert_eq!(
        fs.lookup(ctx(), ROOT, c"fresh.txt").unwrap().inode,
        entry.inode
    );

    // FLUSH is repeatable and non-finalizing; RELEASE ends the handle.
    fs.flush(ctx(), entry.inode, handle, 0).unwrap();
    fs.flush(ctx(), entry.inode, handle, 0).unwrap();
    fs.release(ctx(), entry.inode, 0, handle, false, false, None)
        .unwrap();
    assert_eq!(
        expect_errno(fs.flush(ctx(), entry.inode, handle, 0)),
        LINUX_EBADF
    );
}

#[test]
fn the_guest_can_make_and_remove_directories() {
    let fs = fs();
    let dir = fs
        .mkdir(ctx(), ROOT, c"made", 0o755, 0, Extensions::default())
        .unwrap();
    assert_eq!(dir.attr.st_nlink as u32, 2);

    // A file inside blocks removal and `unlink` refuses the directory, so the
    // guest has to decompose `rm -rf` itself.
    let (child, child_fh, _) = fs
        .create(
            ctx(),
            dir.inode,
            c"inside",
            0o644,
            false,
            GUEST_O_WRONLY,
            0,
            Extensions::default(),
        )
        .unwrap();
    fs.release(ctx(), child.inode, 0, child_fh.unwrap(), false, false, None)
        .unwrap();
    assert_eq!(
        expect_errno(fs.rmdir(ctx(), ROOT, c"made")),
        LINUX_ENOTEMPTY
    );
    assert_eq!(expect_errno(fs.unlink(ctx(), ROOT, c"made")), LINUX_EISDIR);

    fs.unlink(ctx(), dir.inode, c"inside").unwrap();
    fs.rmdir(ctx(), ROOT, c"made").unwrap();
    assert_eq!(expect_errno(fs.lookup(ctx(), ROOT, c"made")), LINUX_ENOENT);
}

#[test]
fn an_explicit_ftruncate_arrives_as_setattr() {
    use msb_krun::backends::fs::SetattrValid;
    let fs = fs();
    let entry = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();

    // `O_TRUNC` rides the open, but an explicit `ftruncate` still comes
    // through here — why `setattr` cannot stay ENOSYS.
    let mut want: stat64 = unsafe { std::mem::zeroed() };
    want.st_size = 5;
    let (attr, _) = fs
        .setattr(ctx(), entry.inode, want, None, SetattrValid::SIZE)
        .unwrap();
    assert_eq!(attr.st_size, 5);
}

#[test]
fn the_guest_stat64_carries_the_whole_shared_policy() {
    let fs = fs();
    let dir = fs.lookup(ctx(), ROOT, c"sub").unwrap();
    let file = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();

    // A directory's link count is 2 (`.` and `..`).
    assert_eq!(dir.attr.st_nlink as u32, 2);
    assert_eq!(dir.attr.st_mode as u32, 0o040000 | 0o755);
    assert_eq!(file.attr.st_nlink as u32, 1);
    assert_eq!(file.attr.st_mode as u32, 0o100000 | 0o644);

    // Block accounting, which `du` and friends read.
    assert_eq!(file.attr.st_blksize as u64, BLOCK_SIZE);
    assert_eq!(
        file.attr.st_blocks as u64,
        (CONTENT.len() as u64).div_ceil(BLOCK_SIZE)
    );

    // 0 on purpose: the guest runs as root. The one attribute the bindings
    // deliberately do not share.
    assert_eq!(file.attr.st_uid, 0);
    assert_eq!(file.attr.st_gid, 0);
}

#[test]
fn open_honours_the_guests_truncate_flag() {
    let fs = fs();
    let entry = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();

    // A plain open leaves the bytes alone.
    let (handle, _) = fs.open(ctx(), entry.inode, false, GUEST_O_WRONLY).unwrap();
    let (attr, _) = fs.getattr(ctx(), entry.inode, handle).unwrap();
    assert_eq!(attr.st_size as u64, CONTENT.len() as u64);

    // With `O_TRUNC` it must be empty by the time the handle exists.
    //
    // Sent on the *open*, not as a separate `setattr`: the server's enabled
    // set is a union with its own `supported`, so `init` cannot turn
    // `ATOMIC_O_TRUNC` off. Dropping the flag would let `echo >` keep the old
    // tail with no error anywhere.
    let (handle, _) = fs
        .open(ctx(), entry.inode, false, GUEST_O_WRONLY | GUEST_O_TRUNC)
        .unwrap();
    let (attr, _) = fs.getattr(ctx(), entry.inode, handle).unwrap();
    assert_eq!(attr.st_size, 0);
}

#[test]
fn an_impossible_access_mode_is_rejected() {
    let fs = fs();
    let entry = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();
    // `O_ACCMODE` is a two-bit field and 3 is not a valid value in it. This
    // is the case a `flags & O_WRONLY` test would wave through.
    assert_eq!(
        expect_errno(fs.open(ctx(), entry.inode, false, 3)),
        LINUX_EINVAL
    );
}

#[test]
fn lookup_reports_size_and_kind() {
    let fs = fs();

    let file = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();
    assert_eq!(file.attr.st_size as usize, CONTENT.len());
    assert_eq!(
        file.attr.st_mode as u32 & libc::S_IFREG as u32,
        libc::S_IFREG as u32
    );
    assert_eq!(file.attr.st_ino as u64, file.inode);

    let dir = fs.lookup(ctx(), ROOT, c"sub").unwrap();
    assert_eq!(
        dir.attr.st_mode as u32 & libc::S_IFDIR as u32,
        libc::S_IFDIR as u32
    );
    assert_ne!(dir.inode, file.inode);
}

#[test]
fn getattr_matches_lookup() {
    let fs = fs();
    let entry = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();

    let (attr, _ttl) = fs.getattr(ctx(), entry.inode, None).unwrap();
    assert_eq!(attr.st_ino, entry.attr.st_ino);
    assert_eq!(attr.st_size, entry.attr.st_size);
    assert_eq!(attr.st_mode, entry.attr.st_mode);
}

#[test]
fn read_returns_the_stored_bytes() {
    let fs = fs();
    let entry = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();
    let (handle, _) = fs.open(ctx(), entry.inode, false, 0).unwrap();
    let handle = handle.expect("open hands out a file handle");

    let mut out = Collected(Vec::new());
    let n = fs
        .read(
            ctx(),
            entry.inode,
            handle,
            &mut out,
            CONTENT.len() as u32,
            0,
            None,
            0,
        )
        .unwrap();
    assert_eq!(n, CONTENT.len());
    assert_eq!(out.0, CONTENT);

    // A window past the end is a short read, which is how the guest learns
    // where the file stops.
    let mut tail = Collected(Vec::new());
    let n = fs
        .read(
            ctx(),
            entry.inode,
            handle,
            &mut tail,
            4096,
            CONTENT.len() as u64,
            None,
            0,
        )
        .unwrap();
    assert_eq!(n, 0);
    assert!(tail.0.is_empty());

    // Offsets address bytes, not blocks.
    let mut mid = Collected(Vec::new());
    fs.read(ctx(), entry.inode, handle, &mut mid, 4, 6, None, 0)
        .unwrap();
    assert_eq!(mid.0, b"from");

    fs.release(ctx(), entry.inode, 0, handle, false, false, None)
        .unwrap();
}

#[test]
fn readdir_streams_dots_then_children() {
    let fs = fs();
    let mut seen: Vec<(String, u32, u64, u64)> = Vec::new();
    let mut collect = |e: DirEntry<'_>| -> std::io::Result<usize> {
        seen.push((
            String::from_utf8_lossy(e.name).into_owned(),
            e.type_,
            e.offset,
            e.ino,
        ));
        Ok(1)
    };
    fs.readdir_for_each(ctx(), ROOT, 0, 4096, 0, &mut collect)
        .unwrap();

    // `.` and `..` lead, both pointing back at this directory, and offsets
    // are 1-based so `0` can keep meaning "from the beginning".
    assert_eq!(seen[0], (".".to_string(), libc::DT_DIR as u32, 1, ROOT));
    assert_eq!(seen[1], ("..".to_string(), libc::DT_DIR as u32, 2, ROOT));

    let names: std::collections::HashMap<_, _> = seen[2..]
        .iter()
        .map(|(n, t, _, i)| (n.clone(), (*t, *i)))
        .collect();
    assert_eq!(names["greeting.txt"].0, libc::DT_REG as u32);
    assert_eq!(names["sub"].0, libc::DT_DIR as u32);

    // A number readdir hands out must be the one a later lookup confirms,
    // or the guest would see two inodes for one file.
    let looked_up = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();
    assert_eq!(names["greeting.txt"].1, looked_up.inode);
}

#[test]
fn readdir_resumes_from_the_quoted_offset() {
    let fs = fs();
    let mut seen = Vec::new();
    let mut collect = |e: DirEntry<'_>| -> std::io::Result<usize> {
        seen.push(String::from_utf8_lossy(e.name).into_owned());
        Ok(1)
    };
    // Offset 2 means the kernel already consumed `.` and `..`.
    fs.readdir_for_each(ctx(), ROOT, 0, 4096, 2, &mut collect)
        .unwrap();
    assert!(!seen.contains(&".".to_string()));
    assert!(!seen.contains(&"..".to_string()));
    assert_eq!(seen.len(), 2);
}

#[test]
fn failures_carry_linux_errno() {
    let fs = fs();

    // The guest kernel is always Linux, so these numbers are Linux's even
    // when the host's differ.
    assert_eq!(expect_errno(fs.lookup(ctx(), ROOT, c"nope")), LINUX_ENOENT);
    assert_eq!(expect_errno(fs.getattr(ctx(), 9999, None)), LINUX_ENOENT);

    let mut sink = Collected(Vec::new());
    let closed = fs.read(ctx(), ROOT, 4242, &mut sink, 16, 0, None, 0);
    assert_eq!(expect_errno(closed), LINUX_EBADF);
}

#[test]
fn every_backend_error_has_its_own_linux_errno() {
    // Drift in an errno table is silent — a wrong number is still a valid
    // number — so every variant is pinned. Adding one to `CortexError` makes
    // this match fail to compile, so never add a `_` arm. And nothing but `Io`
    // may land on EIO: a guest cannot tell "disk full" from "denied" from a
    // real fault.
    for (err, expected) in [
        (CortexError::NotFound, LINUX_ENOENT),
        (CortexError::NotADirectory, LINUX_ENOTDIR),
        (CortexError::IsADirectory, LINUX_EISDIR),
        (CortexError::AlreadyExists, LINUX_EEXIST),
        (CortexError::NotEmpty, LINUX_ENOTEMPTY),
        (CortexError::InvalidName, LINUX_EINVAL),
        (CortexError::InvalidArgument, LINUX_EINVAL),
        (CortexError::FileTooLarge, LINUX_EFBIG),
        (CortexError::BadHandle, LINUX_EBADF),
        (CortexError::PermissionDenied, LINUX_EACCES),
        (CortexError::NoSpace, LINUX_ENOSPC),
        (CortexError::ReadOnly, LINUX_EROFS),
        (CortexError::CrossDevice, LINUX_EXDEV),
        (CortexError::Unsupported, LINUX_ENOSYS),
    ] {
        assert_eq!(to_errno(err).raw_os_error(), Some(expected));
    }
}

#[test]
fn forget_releases_the_inode() {
    let fs = fs();
    let entry = fs.lookup(ctx(), ROOT, c"greeting.txt").unwrap();
    assert!(fs.getattr(ctx(), entry.inode, None).is_ok());

    fs.forget(ctx(), entry.inode, 1);
    assert_eq!(
        expect_errno(fs.getattr(ctx(), entry.inode, None)),
        LINUX_ENOENT
    );
}
