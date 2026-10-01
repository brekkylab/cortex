//! Binds a [`Posix`] to `fuser`'s host-side [`Filesystem`].
//!
//! **Testing needs a real mount**: a callback answers by consuming a `Reply*` only `fuser` can
//! construct.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

use super::super::{
    claim::{Claim, claim, reclaim_abandoned},
    table::{resolved, unmount_under},
};
use crate::fs::{
    DirentKind, FileSystem, Mount, Posix, SetAttr, Stat,
    filesystem::posix::{
        BLOCK_SIZE, NAME_MAX, OpenFlagBits, TOTAL_BLOCKS, TOTAL_INODES, TTL, attr_for,
        decode_open_flags, host_errno,
    },
};

/// Open flags in the host's numbering: this reply goes to this host's kernel.
const HOST_OPEN_FLAGS: OpenFlagBits = OpenFlagBits {
    truncate: libc::O_TRUNC,
    create: libc::O_CREAT,
    create_new: libc::O_EXCL,
};

/// The mount's source name, as `df` shows it.
const FSNAME: &str = "virtx";

/// A live mount on the kernel's own FUSE: constructing one mounts, dropping it unmounts.
///
/// The binding for every unix but macOS. `fuser` speaks the kernel FUSE protocol over the
/// mount fd itself and opens `/dev/fuse`.
///
/// Not generic in the store: `fuser`'s session owns the filesystem.
pub struct FuseMount {
    /// `Option` because both of `fuser`'s exits consume the session and `Drop` has only
    /// `&mut self`. `None` once [`join`](Self::join) or `Drop` took it.
    session: Option<fuser::BackgroundSession>,

    mountpoint: PathBuf,

    /// Names the mount point to [`unmount_on_signal`](crate::fs::unmount_on_signal) and, if
    /// this process is killed, to a later run's
    /// [`reclaim_abandoned`](crate::fs::reclaim_abandoned).
    _claim: Claim,
}

impl FuseMount {
    /// Mount `fs` at `mountpoint` and serve it from a background thread.
    ///
    /// `mountpoint` must already exist. On Linux only a non-root user needs anything installed
    /// (`fusermount3`).
    ///
    /// The mount syscall completes before this returns, so the path is already a mount point.
    ///
    /// `'static` because the store is served from that thread for the mount's lifetime.
    pub fn try_new<T: FileSystem + 'static>(fs: T, mountpoint: &Path) -> io::Result<Self> {
        Self::try_new_with(fs, mountpoint, vec![MountOption::FSName(FSNAME.into())])
    }

    /// [`try_new`](Self::try_new) with the mount options spelled out.
    ///
    /// [`MountOption::RO`] is the useful one: the *kernel* rejects writes before any store
    /// sees them, including stores that would have answered `Ok`.
    pub fn try_new_with<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        options: Vec<MountOption>,
    ) -> io::Result<Self> {
        crate::fs::mount_support()?;

        // Clears mounts a `SIGKILL`ed run left; live siblings' mounts are untouched.
        reclaim_abandoned();

        // `Config` is `#[non_exhaustive]`, so no struct literal.
        let mut config = Config::default();
        config.mount_options = options;
        let session = fuser::spawn_mount2(Posix::new(fs), mountpoint, &config)?;
        Ok(FuseMount {
            session: Some(session),
            mountpoint: mountpoint.to_path_buf(),
            _claim: claim(mountpoint),
        })
    }

    /// Serve until something else ends the mount (`umount`, `fusermount -u`, or the kernel
    /// dropping the connection). **It does not unmount**; to end the mount, drop the guard.
    ///
    /// `Err` if the serving thread failed or panicked.
    pub fn join(mut self) -> io::Result<()> {
        match self.session.take() {
            Some(session) => session.join(),
            None => Ok(()),
        }
    }
}

impl Mount for FuseMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for FuseMount {
    /// `fuser`'s own unmount, then the operating system's if that was refused.
    ///
    /// `umount_and_join` returns before joining when the unmount is refused (`EBUSY` whenever
    /// the mount has readers), which would leave it up. Dropping the guard declares the mount
    /// over, so a refusal escalates: a bounded child per attempt, ending in a lazy detach.
    ///
    /// Never panics: a panic mid-unwind aborts, hiding a failing test's real assertion.
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let Err(refused) = session.umount_and_join() else {
            return;
        };

        if unmount_under(&resolved(&self.mountpoint)) {
            return;
        }
        // Left for someone to clear by hand, so say so, with `fuser`'s reason.
        eprintln!(
            "virtx: unmounting {} failed: {refused}",
            self.mountpoint.display()
        );
    }
}

/// The host-errno table in `fuser`'s newtype, since replies go to this host's kernel.
fn to_errno(err: io::Error) -> Errno {
    Errno::from_i32(host_errno(&err))
}

/// The shared attributes in `fuser`'s struct, which splits `st_mode` into `kind` and `perm`.
/// Owned by the *mounting user*, since a mount whose files belong to someone else cannot be
/// traversed.
fn to_file_attr(inode: u64, stat: &Stat) -> FileAttr {
    let attr = attr_for(stat);
    FileAttr {
        ino: INodeNo(inode),
        size: attr.size,
        blocks: attr.blocks,
        atime: attr.atime,
        mtime: attr.mtime,
        ctime: attr.ctime,
        crtime: attr.crtime,
        kind: to_file_type(stat.kind),
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        // SAFETY: `getuid`/`getgid` read process-global ids and cannot fail.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: attr.blksize,
        flags: 0,
    }
}

fn to_file_type(kind: DirentKind) -> FileType {
    match kind {
        DirentKind::Dir => FileType::Directory,
        DirentKind::File => FileType::RegularFile,
    }
}

/// Symlinks and hard links stay on `fuser`'s `EPERM` defaults, extended attributes on its
/// `ENOSYS` ones; the store contract has no notion of them.
///
/// The `'static` bound is `fuser`'s: a mounted session outlives the mount call, so the
/// filesystem may not borrow.
impl<T: FileSystem + 'static> Filesystem for Posix<T> {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match super::block_on(self.lookup_child(parent.0, name)) {
            Ok((inode, stat)) => reply.entry(&TTL, &to_file_attr(inode, &stat), Generation(0)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.forget_inode(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match super::block_on(self.stat_inode(ino.0)) {
            Ok(stat) => reply.attr(&TTL, &to_file_attr(ino.0, &stat)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let options = match decode_open_flags(flags.0, &HOST_OPEN_FLAGS) {
            Ok(options) => options,
            Err(err) => return reply.error(to_errno(err)),
        };
        match super::block_on(self.open_inode(ino.0, options)) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match super::block_on(self.read_handle(fh.0, offset, size)) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match super::block_on(self.release_handle(fh.0)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        // `add` returning true (buffer full) is the `stop` flag.
        let streamed =
            super::block_on(
                self.for_each_dirent(ino.0, offset, |child_inode, child, cursor| {
                    Ok(reply.add(
                        INodeNo(child_inode),
                        cursor,
                        to_file_type(child.kind),
                        &child.name,
                    ))
                }),
            );
        match streamed {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        // The opcode means "make it if absent" whatever the flags word says.
        let options = match decode_open_flags(flags, &HOST_OPEN_FLAGS) {
            Ok(options) => options.create(true),
            Err(err) => return reply.error(to_errno(err)),
        };

        match super::block_on(self.create_child(parent.0, name, options)) {
            Ok((inode, stat, fh)) => reply.created(
                &TTL,
                &to_file_attr(inode, &stat),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        match super::block_on(self.write_handle(fh.0, offset, data)) {
            Ok(written) => reply.written(written as u32),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // Not `release_handle`: this arrives on every `close()`.
        match super::block_on(self.flush_handle(fh.0)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match super::block_on(self.flush_handle(fh.0)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match super::block_on(self.mkdir_child(parent.0, name)) {
            Ok((inode, stat)) => reply.entry(&TTL, &to_file_attr(inode, &stat), Generation(0)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match super::block_on(self.unlink_child(parent.0, name)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match super::block_on(self.rmdir_child(parent.0, name)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        // `RENAME_NOREPLACE`/`RENAME_EXCHANGE` are outside the shared contract; EINVAL is
        // Linux's answer for an unimplemented rename flag.
        if !flags.is_empty() {
            reply.error(Errno::from_i32(libc::EINVAL));
            return;
        }
        match super::block_on(self.rename_child(parent.0, name, newparent.0, newname)) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Any file handle is ignored: the inode already names the path to resize.
        let want = SetAttr {
            size,
            mode,
            uid,
            gid,
            atime: None,
            mtime: None,
        };
        match super::block_on(self.setattr_inode(ino.0, want)) {
            Ok(stat) => reply.attr(&TTL, &to_file_attr(ino.0, &stat)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Synthetic capacity; see `TOTAL_BLOCKS`.
        reply.statfs(
            TOTAL_BLOCKS,
            TOTAL_BLOCKS,
            TOTAL_BLOCKS,
            TOTAL_INODES,
            TOTAL_INODES,
            BLOCK_SIZE as u32,
            NAME_MAX,
            BLOCK_SIZE as u32,
        );
    }
}
