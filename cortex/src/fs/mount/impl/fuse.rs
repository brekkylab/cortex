//! Binds a [`Posix`] to `fuser`'s host-side [`Filesystem`]. [`FuseMount`] is the whole of
//! what it exports.
//!
//! Sibling of [`super::fuse_t`]: the same [`Posix`] operations through a different interface,
//! so everything here is translation. What differs from that side is who drives the session —
//! `fuser` speaks the kernel FUSE protocol over the mount fd itself, which is exactly what
//! FUSE-T's helper will not tolerate — and the attribute type ([`FileAttr`] rather than a
//! `stat` this crate lays out by hand). The errno numbering is shared: both answer this
//! host's kernel.
//!
//! On Linux `fuser` opens `/dev/fuse` itself, so nothing has to be installed. On macOS this
//! is **macFUSE**, a kernel extension needing reduced-security boot on Apple Silicon — which
//! is why [`super::fuse_t`] exists beside it.
//!
//! What stays on `fuser`'s `ENOSYS` defaults is what the store contract has no notion of:
//! symlinks, hard links, extended attributes.
//!
//! **Hard to test in isolation, and structurally so**: a callback here answers by consuming a
//! `Reply*` object only `fuser` can construct, so there is nothing to hand it and nothing it
//! hands back. Exercising it means a real mount.

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
    sigchld::Sigchld,
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

/// What the mount reports as its source — the name `df` and Finder show.
///
/// Fixed, because nothing has wanted another one.
const FSNAME: &str = "cortex";

/// A live mount on the kernel's own FUSE: constructing one mounts, dropping it unmounts.
///
/// The whole call surface of this binding, and the counterpart of
/// [`FuseTMount`](super::FuseTMount) — same shape, different interface underneath.
///
/// Not generic in the store, and needs no erasure to manage it: `fuser`'s session owns the
/// filesystem it was handed, so unlike the FUSE-T binding there is no raw pointer here whose
/// target something else has to keep alive.
pub struct FuseMount {
    /// `Option` so `Drop` can take ownership: both of `fuser`'s exits consume the session, and
    /// `Drop` has only `&mut self`. `None` once [`join`](Self::join) or `Drop` has taken it.
    session: Option<fuser::BackgroundSession>,

    mountpoint: PathBuf,

    /// This process's ownership of the mount point — what an opt-in
    /// [`unmount_on_signal`](crate::fs::unmount_on_signal) reads now, and what a later run's
    /// [`reclaim_abandoned`](crate::fs::reclaim_abandoned) reads if this one is killed. Held
    /// rather than read: dropping it is what gives the mount point up.
    _claim: Claim,
}

impl FuseMount {
    /// Mount `fs` at `mountpoint` and serve it from a background thread.
    ///
    /// `mountpoint` must already exist. On Linux this needs nothing installed; on macOS it
    /// needs macFUSE.
    ///
    /// Returns as soon as the mount is real, and needs no waiting to say so: the mount syscall
    /// happens before this returns, so the path is a mount point by the time a caller has the
    /// guard. (The FUSE-T binding has to poll for that, because its kernel-side mount is
    /// completed by a helper it has not answered yet.)
    ///
    /// `'static`, because the store is served from that thread for as long as the mount lives.
    pub fn try_new<T: FileSystem + 'static>(fs: T, mountpoint: &Path) -> io::Result<Self> {
        Self::try_new_with(fs, mountpoint, vec![MountOption::FSName(FSNAME.into())])
    }

    /// [`try_new`](Self::try_new) with the mount options spelled out.
    ///
    /// [`MountOption::RO`] is the one worth reaching for: it makes the *kernel* reject writes
    /// before they reach a store, which is stronger than every store answering
    /// `ReadOnlyFilesystem` by hand — and it covers the ones that would have answered `Ok`.
    ///
    /// The options are `fuser`'s own, and this is the only binding that has any: FUSE-T takes a
    /// different set entirely, which is why its `try_new_with` chooses a transport instead.
    pub fn try_new_with<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        options: Vec<MountOption>,
    ) -> io::Result<Self> {
        // What a `SIGKILL`ed run left behind is nobody's but the next run's, and this
        // is the next run. Only mounts whose owning process is gone are touched, so a
        // sibling instance keeps its own.
        reclaim_abandoned();

        // `Config` is `#[non_exhaustive]`, so it cannot be built with a struct literal from
        // outside `fuser` — start from the default and assign.
        let mut config = Config::default();
        config.mount_options = options;
        // Held across the mount for the reason in `sigchld`: on macOS this goes
        // through the same libfuse2 mount ABI FUSE-T exports, which resets the
        // process's SIGCHLD handling before forking its helper. On Linux `fuser`
        // opens `/dev/fuse` itself and forks nothing, and the guard finds nothing
        // to put back.
        let session = {
            let _sigchld = Sigchld::held();
            fuser::spawn_mount2(Posix::new(fs), mountpoint, &config)?
        };
        Ok(FuseMount {
            session: Some(session),
            mountpoint: mountpoint.to_path_buf(),
            _claim: claim(mountpoint),
        })
    }

    /// Serve until the mount goes away, then take it down.
    ///
    /// For a program whose whole job is the mount: `try_new` puts it up without blocking, and
    /// this waits for something else to end it — `umount`, `fusermount -u`, or the kernel
    /// tearing the connection down. **It does not unmount**, so a caller with other work to do
    /// drops the guard instead of joining it.
    ///
    /// The session is taken out here, so the `Drop` that follows has nothing left to do.
    ///
    /// `Err` is the serving thread having failed or panicked.
    pub fn join(mut self) -> io::Result<()> {
        match self.session.take() {
            Some(session) => session.join(),
            None => Ok(()),
        }
    }
}

/// Nothing to arrange: the guard already is the mount, and already knows where it is.
impl Mount for FuseMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for FuseMount {
    /// `fuser`'s own unmount, then the operating system's if that was refused.
    ///
    /// `umount_and_join` is `mount.umount()?` followed by the join, so a refused unmount
    /// returns before the join and leaves the mount up — and a mount with readers on it *is*
    /// refused, with `EBUSY`. Stopping there would leave the mount behind, which breaks the
    /// contract rather than merely putting a message on stderr.
    ///
    /// So the refusal is not the end of it. A caller that has dropped the guard has said the
    /// mount is over, and what follows is the escalation the FUSE-T binding and
    /// [`reclaim_abandoned`](crate::fs::reclaim_abandoned) also use — a bounded child per
    /// attempt, ending in a lazy detach — so a busy mount comes down the same way whoever is
    /// taking it down.
    ///
    /// Nothing here panics. A `Drop` that panics mid-unwind aborts the process, which in a
    /// failing test replaces the real assertion with a bare abort.
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
        // Not silent: a mount that would not come down is left behind for someone to clear by
        // hand, so saying so — and saying what `fuser` made of it — is the least this can do.
        eprintln!(
            "cortex: unmounting {} failed: {refused}",
            self.mountpoint.display()
        );
    }
}

/// Wrap the shared host-errno table in `fuser`'s newtype. The table is shared with the FUSE-T
/// binding: both answer this host's kernel.
fn to_errno(err: io::Error) -> Errno {
    Errno::from_i32(host_errno(&err))
}

/// Lay the shared attribute policy into `fuser`'s struct, which splits what `st_mode` packs
/// into a separate `kind` and `perm`. Ownership is the *mounting user's*: a mount whose files
/// belong to someone else cannot be traversed.
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
        // The cursor protocol is the shared operation's; this closure only encodes. `add`
        // returning true is the `stop` flag.
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
        // `RENAME_NOREPLACE`/`RENAME_EXCHANGE` cannot be served: libfuse-t's `rename` takes no
        // flags at all, so a contract carrying them would be unhonourable in one of the
        // bindings. EINVAL is Linux's own answer for a rename flag it does not implement, and
        // unlike ENOSYS it does not make the kernel stop sending renames for the whole mount.
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
        // `fuser` has already decoded the validity mask into these `Option`s. The file handle
        // it may also carry is dropped: a resize names a path either way, and the inode
        // arriving beside it is already that path.
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
