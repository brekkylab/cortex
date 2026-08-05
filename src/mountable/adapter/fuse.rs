//! Binds a [`PosixFs`] to `fuser`'s host-side [`Filesystem`].
//!
//! Sibling of [`super::krun`]: the same [`PosixFs`] operations through a different
//! interface, so everything here is translation. What differs from the krun side
//! is the errno numbering (host, not guest-Linux) and the attribute type
//! ([`FileAttr`], not `stat64`). Data transfer is simpler too — `fuser` passes
//! byte slices where `msb_krun` passes file descriptors, so nothing is staged
//! through a temp file.
//!
//! What stays on `fuser`'s `ENOSYS` defaults is what the backend contract has no
//! notion of — symlinks, hard links — matching the krun stubs.
//!
//! **No unit tests, unlike its two siblings**, and the reason is structural: a
//! callback here answers by consuming a `Reply*` object that only `fuser` can
//! construct, so there is nothing a test can hand it and nothing it hands back.
//! The krun binding returns `io::Result`, and the FUSE-T one fills caller-owned
//! out-params, so both are driven directly in their `*_tests.rs`. This one is
//! covered only by `tests/host_mount.rs`, which is `#[ignore]`d because it needs a
//! real mount — so a plain `cargo test` exercises none of it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

use crate::mountable::Mountable;
use crate::mountable::PosixFs;
use crate::mountable::posix::{
    BLOCK_SIZE, NAME_MAX, OpenFlagBits, TOTAL_BLOCKS, TOTAL_INODES, TTL, attr_for,
    decode_open_flags, host_errno,
};
use crate::{CortexError, DirentKind, Result, SetAttr, Stat};

/// Host numbering, where the krun binding's is Linux's — `O_TRUNC` is not the
/// same number on the two.
const HOST_OPEN_FLAGS: OpenFlagBits = OpenFlagBits {
    truncate: libc::O_TRUNC,
    create: libc::O_CREAT,
    create_new: libc::O_EXCL,
};

/// A live host mount, unmounted when this guard is dropped.
///
/// This binding's *call surface* — the counterpart of `.fs(..).custom(..)` on a
/// `msb_krun` VM. The [`Filesystem`] impl below is what the kernel talks to.
pub struct CortexMount {
    /// `Option` so [`Drop`] can take ownership: `umount_and_join` consumes the
    /// session, and `Drop` only has `&mut self`.
    session: Option<fuser::BackgroundSession>,
    mountpoint: PathBuf,
}

impl CortexMount {
    /// Mount `volume` at `mountpoint` and serve it from a background thread.
    ///
    /// `mountpoint` must already exist. On Linux `fuser` opens `/dev/fuse` itself;
    /// on macOS this needs **macFUSE** — see `adapter/fuse_t.rs` for why FUSE-T
    /// needs its own binding.
    pub fn spawn<T>(volume: T, mountpoint: impl AsRef<Path>) -> Result<Self>
    where
        T: Mountable + 'static,
    {
        Self::spawn_with(
            volume,
            mountpoint,
            vec![MountOption::FSName("cortex".into())],
        )
    }

    /// [`spawn`](Self::spawn) with the mount options spelled out.
    ///
    /// [`MountOption::RO`] makes the *kernel* reject writes before they reach a
    /// backend — stronger than each backend answering `ReadOnly` by hand.
    pub fn spawn_with<T>(
        volume: T,
        mountpoint: impl AsRef<Path>,
        mount_options: Vec<MountOption>,
    ) -> Result<Self>
    where
        T: Mountable + 'static,
    {
        let mountpoint = mountpoint.as_ref().to_path_buf();
        // `Config` is `#[non_exhaustive]`, so it cannot be built with a struct
        // literal from outside `fuser` — start from the default and assign.
        let mut config = Config::default();
        config.mount_options = mount_options;
        let session = fuser::spawn_mount2(PosixFs::new(volume), &mountpoint, &config)?;
        Ok(CortexMount {
            session: Some(session),
            mountpoint,
        })
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Unmount and join the serving thread. [`Drop`] does the same but cannot
    /// report failure.
    pub fn unmount(mut self) -> Result<()> {
        match self.session.take() {
            Some(session) => Ok(session.umount_and_join()?),
            None => Ok(()),
        }
    }
}

impl Drop for CortexMount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            // Swallowed: a `Drop` that panics mid-unwind aborts the process,
            // replacing a failing test's real assertion with a bare abort.
            if let Err(err) = session.umount_and_join() {
                eprintln!(
                    "cortex: unmounting {} failed: {err}",
                    self.mountpoint.display()
                );
            }
        }
    }
}

/// Wrap the shared host-errno table in `fuser`'s newtype. The table is shared with
/// the FUSE-T binding: both answer this host's kernel.
fn to_errno(err: CortexError) -> Errno {
    Errno::from_i32(host_errno(&err))
}

/// Lay the shared attribute policy into `fuser`'s struct, which splits what
/// `st_mode` packs into a separate `kind` and `perm`. Ownership is the *mounting
/// user's*: a mount whose files belong to someone else cannot be traversed.
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
        kind: match stat.kind {
            DirentKind::Dir => FileType::Directory,
            DirentKind::File => FileType::RegularFile,
        },
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

/// The `'static` bound is `fuser`'s: a mounted session outlives the mount call, so
/// the filesystem may not borrow. The krun binding's device owns its backend and
/// needs no such bound.
impl<T: Mountable + 'static> Filesystem for PosixFs<T> {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.lookup_child(parent.0, name) {
            Ok((inode, stat)) => reply.entry(&TTL, &to_file_attr(inode, &stat), Generation(0)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.forget_inode(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.stat_inode(ino.0) {
            Ok(stat) => reply.attr(&TTL, &to_file_attr(ino.0, &stat)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let options = match decode_open_flags(flags.0, &HOST_OPEN_FLAGS) {
            Ok(options) => options,
            Err(err) => return reply.error(to_errno(err)),
        };
        match self.open_inode(ino.0, options) {
            Ok((fh, _stat)) => reply.opened(FileHandle(fh), FopenFlags::empty()),
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
        match self.read_handle(fh.0, offset, size) {
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
        match self.release_handle(fh.0) {
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
        // The cursor protocol is the shared operation's; this closure only
        // encodes. `add` returning true is the `stop` flag.
        let streamed = self.for_each_dirent(ino.0, offset, |child_inode, child, cursor| {
            let kind = match child.kind {
                DirentKind::Dir => FileType::Directory,
                DirentKind::File => FileType::RegularFile,
            };
            Ok(reply.add(INodeNo(child_inode), cursor, kind, &child.name))
        });
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

        match self.create_child(parent.0, name, options) {
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
        // No staging temp file, unlike krun: `fuser` hands over a plain slice.
        match self.write_handle(fh.0, offset, data) {
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
        match self.flush_handle(fh.0) {
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
        match self.flush_handle(fh.0) {
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
        match self.mkdir_child(parent.0, name) {
            Ok((inode, stat)) => reply.entry(&TTL, &to_file_attr(inode, &stat), Generation(0)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.unlink_child(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.rmdir_child(parent.0, name) {
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
        // `RENAME_NOREPLACE`/`RENAME_EXCHANGE` cannot be served: libfuse-t's
        // `rename` takes no flags at all, so a contract carrying them would be
        // unhonourable in one of the three bindings. EINVAL is Linux's own answer
        // for a rename flag it does not implement, and unlike ENOSYS it does not
        // make the kernel stop sending renames for the whole mount.
        if !flags.is_empty() {
            reply.error(Errno::from_i32(libc::EINVAL));
            return;
        }
        match self.rename_child(parent.0, name, newparent.0, newname) {
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
        fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // `fuser` already decoded the validity mask into these `Option`s, where
        // krun consults a bitflag set. Same information, same shared `SetAttr`.
        let want = SetAttr {
            size,
            mode,
            uid,
            gid,
            atime: None,
            mtime: None,
        };
        match self.setattr_inode(ino.0, fh.map(|fh| fh.0), want) {
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
