//! Binds a [`PosixFs`] to `msb_krun`'s FUSE-shaped [`DynFileSystem`].
//!
//! This is the only place the inode bookkeeping in [`super::posix`] meets
//! `msb_krun`: it translates FUSE calls into backend operations and backend
//! errors/metadata into the `stat64`/errno shapes the guest kernel expects.

use std::ffi::{CStr, OsStr};
use std::io::{Read, Result, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;

use msb_krun::{
    DynFileSystem,
    backends::fs::{Context, DirEntry, Entry, FsOptions, stat64},
};

use crate::mountable::Mountable;
use crate::mountable::PosixFs;
use crate::mountable::posix::{
    BLOCK_SIZE, NAME_MAX, OpenFlagBits, TOTAL_BLOCKS, TOTAL_INODES, TTL, attr_for,
    decode_open_flags, unix_time,
};
use crate::{CortexError, DirentKind, SetAttr, Stat};

// The guest kernel is always Linux, so every reply must carry Linux errno
// numbers. The host's `libc` values differ (e.g. ENOSYS is 78 on macOS but 38
// on Linux) and would be misread by the guest.
const LINUX_ENOENT: i32 = 2;
const LINUX_EIO: i32 = 5;
const LINUX_EBADF: i32 = 9;
const LINUX_EEXIST: i32 = 17;
const LINUX_EACCES: i32 = 13;
const LINUX_ENOTDIR: i32 = 20;
const LINUX_ENOSPC: i32 = 28;
const LINUX_EROFS: i32 = 30;
const LINUX_EXDEV: i32 = 18;
const LINUX_EFBIG: i32 = 27;
const LINUX_EISDIR: i32 = 21;
const LINUX_EINVAL: i32 = 22;
const LINUX_ENOSYS: i32 = 38;
const LINUX_ENOTEMPTY: i32 = 39;

/// Linux-numbered for the same reason the errnos are. (`O_TRUNC` is `0o1000`
/// here and `0o2000` on a macOS host.)
const LINUX_OPEN_FLAGS: OpenFlagBits = OpenFlagBits {
    append: 0o2000,
    truncate: 0o1000,
    create: 0o100,
    create_new: 0o200,
};

fn errno(raw: i32) -> std::io::Error {
    std::io::Error::from_raw_os_error(raw)
}

/// Translate a backend error into the Linux errno the guest expects.
fn to_errno(err: CortexError) -> std::io::Error {
    errno(match err {
        CortexError::NotFound => LINUX_ENOENT,
        CortexError::NotADirectory => LINUX_ENOTDIR,
        CortexError::IsADirectory => LINUX_EISDIR,
        CortexError::AlreadyExists => LINUX_EEXIST,
        CortexError::NotEmpty => LINUX_ENOTEMPTY,
        CortexError::InvalidName => LINUX_EINVAL,
        CortexError::InvalidArgument => LINUX_EINVAL,
        CortexError::FileTooLarge => LINUX_EFBIG,
        CortexError::BadHandle => LINUX_EBADF,
        CortexError::PermissionDenied => LINUX_EACCES,
        CortexError::NoSpace => LINUX_ENOSPC,
        CortexError::ReadOnly => LINUX_EROFS,
        CortexError::CrossDevice => LINUX_EXDEV,
        CortexError::Unsupported => LINUX_ENOSYS,
        // The host's own errno cannot be forwarded: the guest would read the
        // number as something else entirely. Anything not classified above
        // therefore degrades to EIO, which is why the classification matters.
        CortexError::Io(_) => LINUX_EIO,
    })
}

/// Lay the shared attribute policy into the guest's `stat64`.
///
/// `uid`/`gid` stay 0: the guest runs as root and expects to own what it sees,
/// where the host bindings report the mounting user.
fn to_stat64(inode: u64, stat: &Stat) -> stat64 {
    let attr = attr_for(stat);
    let (mtime, mtime_nsec) = unix_time(attr.mtime);
    let (atime, atime_nsec) = unix_time(attr.atime);
    let (ctime, ctime_nsec) = unix_time(attr.ctime);

    // SAFETY: `stat64` is a repr(C) POD of integers; all-zero is a valid value
    // and we set every field the kernel inspects.
    let mut st: stat64 = unsafe { std::mem::zeroed() };
    st.st_ino = inode as _;
    st.st_size = attr.size as _;
    st.st_blocks = attr.blocks as _;
    st.st_blksize = attr.blksize as _;
    st.st_mode = attr.mode as _;
    st.st_nlink = attr.nlink as _;
    st.st_mtime = mtime as _;
    st.st_mtime_nsec = mtime_nsec as _;
    st.st_atime = atime as _;
    st.st_atime_nsec = atime_nsec as _;
    st.st_ctime = ctime as _;
    st.st_ctime_nsec = ctime_nsec as _;
    st
}

/// `CStr` is the guest's name encoding and `OsStr` is what the shared operations
/// take; on unix those are the same bytes.
fn guest_name(name: &CStr) -> &OsStr {
    OsStr::from_bytes(name.to_bytes())
}

/// The reply shape `lookup`, `create`, and `mkdir` share — each hands the guest an
/// entry *and* takes a kernel reference on it.
fn to_entry(inode: u64, stat: &Stat) -> Entry {
    Entry {
        inode,
        // Inode numbers are never reused (see `InodeTable`), so nothing needs
        // disambiguating. Were they reused, this would become a correctness bug.
        generation: 0,
        attr: to_stat64(inode, stat),
        attr_flags: 0,
        attr_timeout: TTL,
        entry_timeout: TTL,
    }
}

impl<T: Mountable> DynFileSystem for PosixFs<T> {
    /// Empty is not "no features" — it means "the server's defaults".
    ///
    /// What comes back here can only *add*: the server settles on
    /// `capable & (want | supported)`, where `supported` is its own list. So
    /// returning nothing subtracts nothing, and the set still includes
    /// `AUTO_INVAL_DATA` (which is what makes a backend's [`Stat::mtime`]
    /// functional rather than decorative — a guest watches it to decide when to
    /// drop cached pages) and `MAX_PAGES` (which lets a request reach the server's
    /// 1 MiB buffer instead of the 32-page default).
    ///
    /// Worth stating because the line reads like the opposite of what it does.
    ///
    /// [`Stat::mtime`]: crate::Stat::mtime
    fn init(&self, _capable: FsOptions) -> Result<FsOptions> {
        Ok(FsOptions::empty())
    }

    fn destroy(&self) {}

    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> Result<Entry> {
        let (inode, stat) = self
            .lookup_child(parent, guest_name(name))
            .map_err(to_errno)?;
        Ok(to_entry(inode, &stat))
    }

    fn forget(&self, _ctx: Context, inode: u64, count: u64) {
        self.forget_inode(inode, count);
    }

    fn batch_forget(&self, ctx: msb_krun::backends::fs::Context, requests: Vec<(u64, u64)>) {
        for (inode, count) in requests {
            self.forget(ctx, inode, count);
        }
    }

    fn getattr(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> std::io::Result<(msb_krun::backends::fs::stat64, std::time::Duration)> {
        let stat = self.stat_inode(inode).map_err(to_errno)?;
        Ok((to_stat64(inode, &stat), TTL))
    }

    fn setattr(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        inode: u64,
        attr: msb_krun::backends::fs::stat64,
        handle: Option<u64>,
        valid: msb_krun::backends::fs::SetattrValid,
    ) -> std::io::Result<(msb_krun::backends::fs::stat64, std::time::Duration)> {
        use msb_krun::backends::fs::SetattrValid;

        // The mask says which fields the guest means; the rest of `attr` is
        // uninitialized as far as we are concerned. `O_TRUNC` does not arrive here
        // (it rides the open), but an explicit `ftruncate`/`chmod`/`chown`/`touch`
        // does — which is why this cannot stay ENOSYS.
        let want = SetAttr {
            size: valid
                .contains(SetattrValid::SIZE)
                .then_some(attr.st_size as u64),
            mode: valid
                .contains(SetattrValid::MODE)
                .then_some(attr.st_mode as u32),
            uid: valid.contains(SetattrValid::UID).then_some(attr.st_uid),
            gid: valid.contains(SetattrValid::GID).then_some(attr.st_gid),
            // Timestamps are read from the mask but have nowhere to go; the
            // shared operation documents why they are accepted and dropped.
            atime: None,
            mtime: None,
        };
        let stat = self.setattr_inode(inode, handle, want).map_err(to_errno)?;
        Ok((to_stat64(inode, &stat), TTL))
    }

    fn readlink(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
    ) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn symlink(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _linkname: &std::ffi::CStr,
        _parent: u64,
        _name: &std::ffi::CStr,
        _extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn mknod(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _name: &std::ffi::CStr,
        _mode: u32,
        _rdev: u32,
        _umask: u32,
        _extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn mkdir(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
        // The guest's mode and umask are dropped: the shared attribute policy
        // reports fixed permission bits, so there is nowhere to store them.
        _mode: u32,
        _umask: u32,
        _extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        let (inode, stat) = self
            .mkdir_child(parent, guest_name(name))
            .map_err(to_errno)?;
        Ok(to_entry(inode, &stat))
    }

    fn unlink(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
    ) -> std::io::Result<()> {
        self.unlink_child(parent, guest_name(name))
            .map_err(to_errno)
    }

    fn rmdir(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
    ) -> std::io::Result<()> {
        self.rmdir_child(parent, guest_name(name)).map_err(to_errno)
    }

    fn rename(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        olddir: u64,
        oldname: &std::ffi::CStr,
        newdir: u64,
        newname: &std::ffi::CStr,
        flags: u32,
    ) -> std::io::Result<()> {
        // `RENAME_NOREPLACE`/`RENAME_EXCHANGE` cannot be served: libfuse-t's
        // `rename` has no flags argument at all, so a contract carrying them would
        // be unhonourable in one of the three bindings. EINVAL is what Linux itself
        // answers for a rename flag it does not implement — and unlike ENOSYS it
        // does not make the kernel stop sending renames for the whole mount.
        if flags != 0 {
            return Err(errno(LINUX_EINVAL));
        }
        self.rename_child(olddir, guest_name(oldname), newdir, guest_name(newname))
            .map_err(to_errno)
    }

    fn link(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _newparent: u64,
        _newname: &std::ffi::CStr,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn open(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        inode: u64,
        _kill_priv: bool,
        flags: u32,
    ) -> std::io::Result<(Option<u64>, msb_krun::backends::fs::OpenOptions)> {
        // The flags must be honoured, not dropped. `msb_krun`'s server enables
        // `capable & (want | supported)` — a *union* — and puts `ATOMIC_O_TRUNC`
        // in its own `supported`, so `O_TRUNC` arrives here whatever `init` asks
        // for. Ignoring it leaves `echo > existing` with the old tail in place and
        // no error anywhere.
        let options = decode_open_flags(flags as i32, &LINUX_OPEN_FLAGS).map_err(to_errno)?;
        let (fh, _stat) = self.open_inode(inode, options).map_err(to_errno)?;
        Ok((Some(fh), msb_krun::backends::fs::OpenOptions::empty()))
    }

    fn create(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
        _mode: u32,
        _kill_priv: bool,
        flags: u32,
        _umask: u32,
        _extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<(
        msb_krun::backends::fs::Entry,
        Option<u64>,
        msb_krun::backends::fs::OpenOptions,
    )> {
        // A guest `create` always means "make it if absent", whether or not it
        // set `O_CREAT` in the flags word — the opcode itself says so.
        let options = decode_open_flags(flags as i32, &LINUX_OPEN_FLAGS)
            .map_err(to_errno)?
            .create(true);

        let (inode, stat, fh) = self
            .create_child(parent, guest_name(name), options)
            .map_err(to_errno)?;
        Ok((
            to_entry(inode, &stat),
            Some(fh),
            msb_krun::backends::fs::OpenOptions::empty(),
        ))
    }

    fn read(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        handle: u64,
        w: &mut dyn msb_krun::backends::fs::ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> std::io::Result<usize> {
        // No hand-patched errno: the shared operation reports a closed handle as
        // `BadHandle`, which `to_errno` renders as EBADF. While each binding
        // patched this locally, only one of them actually did.
        let data = self.read_handle(handle, offset, size).map_err(to_errno)?;
        if data.is_empty() {
            return Ok(0);
        }

        // `ZeroCopyWriter` copies out of a file descriptor, not a `&[u8]`, so we
        // stage the bytes in a temp file and hand it that fd. A backend that
        // already holds an fd could skip this copy.
        let mut staging = tempfile::tempfile()?;
        staging.write_all(&data)?;
        // Disambiguate: `File` now also impls our `FileHandle::flush`, so name
        // the std `Write::flush` we want on the staging temp file explicitly.
        Write::flush(&mut staging)?;
        w.write_all_from(&mut staging, data.len(), 0)?;
        Ok(data.len())
    }

    fn write(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        handle: u64,
        r: &mut dyn msb_krun::backends::fs::ZeroCopyReader,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _kill_priv: bool,
        _flags: u32,
    ) -> std::io::Result<usize> {
        // `ZeroCopyReader` delivers into a file descriptor, so stage the window in
        // a temp file and read it back.
        let mut staging = tempfile::tempfile()?;
        r.read_exact_to(&mut staging, size as usize, 0)?;
        staging.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; size as usize];
        staging.read_exact(&mut buf)?;

        self.write_handle(handle, offset, &buf).map_err(to_errno)
    }

    fn flush(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        handle: u64,
        _lock_owner: u64,
    ) -> std::io::Result<()> {
        // Not `release_handle`: FLUSH arrives on every `close()`.
        self.flush_handle(handle).map_err(to_errno)
    }

    fn fsync(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _datasync: bool,
        handle: u64,
    ) -> std::io::Result<()> {
        self.flush_handle(handle).map_err(to_errno)
    }

    fn fallocate(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _handle: u64,
        _mode: u32,
        _offset: u64,
        _length: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn release(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _flags: u32,
        handle: u64,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> std::io::Result<()> {
        self.release_handle(handle).map_err(to_errno)
    }

    fn statfs(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
    ) -> std::io::Result<msb_krun::backends::fs::statvfs64> {
        // Safe because we are zero-initializing a struct with only POD fields.
        let mut st: msb_krun::backends::fs::statvfs64 = unsafe { std::mem::zeroed() };
        st.f_namemax = NAME_MAX as _;
        st.f_bsize = BLOCK_SIZE as _;
        // `f_frsize` is what glibc's `statvfs` consumers (`df` among them)
        // divide by, so leaving it zero is a division by zero in the guest.
        st.f_frsize = BLOCK_SIZE as _;
        st.f_blocks = TOTAL_BLOCKS as _;
        st.f_bfree = TOTAL_BLOCKS as _;
        st.f_bavail = TOTAL_BLOCKS as _;
        st.f_files = TOTAL_INODES as _;
        st.f_ffree = TOTAL_INODES as _;
        st.f_favail = TOTAL_INODES as _;
        Ok(st)
    }

    fn setxattr(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _name: &std::ffi::CStr,
        _value: &[u8],
        _flags: u32,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn getxattr(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _name: &std::ffi::CStr,
        _size: u32,
    ) -> std::io::Result<msb_krun::backends::fs::GetxattrReply> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn listxattr(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _size: u32,
    ) -> std::io::Result<msb_krun::backends::fs::ListxattrReply> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn removexattr(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _name: &std::ffi::CStr,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn opendir(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _flags: u32,
    ) -> std::io::Result<(Option<u64>, msb_krun::backends::fs::OpenOptions)> {
        Ok((None, msb_krun::backends::fs::OpenOptions::empty()))
    }

    fn readdir(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _handle: u64,
        _size: u32,
        _offset: u64,
    ) -> std::io::Result<Vec<msb_krun::backends::fs::DirEntry<'static>>> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn readdir_for_each(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut msb_krun::backends::fs::AddDirEntry<'_>,
    ) -> std::io::Result<()> {
        // The cursor protocol is the shared operation's; this closure only
        // encodes. A zero from `add_entry` means the buffer is full — the `stop`.
        self.for_each_dirent(inode, offset, |child_ino, child, cursor| {
            let type_ = match child.kind {
                DirentKind::Dir => libc::DT_DIR,
                DirentKind::File => libc::DT_REG,
            };
            let entry = DirEntry {
                ino: child_ino as _,
                offset: cursor,
                type_: type_ as u32,
                name: child.name.as_bytes(),
            };
            Ok(add_entry(entry)? == 0)
        })
        .map_err(to_errno)
    }

    fn readdirplus(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _handle: u64,
        _size: u32,
        _offset: u64,
    ) -> std::io::Result<
        Vec<(
            msb_krun::backends::fs::DirEntry<'static>,
            msb_krun::backends::fs::Entry,
        )>,
    > {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn readdirplus_for_each(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut msb_krun::backends::fs::AddDirEntryPlus<'_>,
    ) -> std::io::Result<()> {
        let entries = self.readdirplus(ctx, inode, handle, size, offset)?;
        for (dir_entry, entry) in entries {
            match add_entry(dir_entry, entry) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn fsyncdir(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _datasync: bool,
        _handle: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn releasedir(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _flags: u32,
        _handle: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn access(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _mask: u32,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn lseek(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _handle: u64,
        _offset: u64,
        _whence: u32,
    ) -> std::io::Result<u64> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn copyfilerange(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode_in: u64,
        _handle_in: u64,
        _offset_in: u64,
        _inode_out: u64,
        _handle_out: u64,
        _offset_out: u64,
        _len: u64,
        _flags: u64,
    ) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    // `setupmapping`/`removemapping` (DAX) are left to the trait's ENOSYS
    // defaults: their macOS/Windows signatures name `Sender<WorkerMessage>`,
    // types the `msb_krun` facade doesn't re-export, so we can't restate them.

    fn ioctl(
        &self,
        _ctx: msb_krun::backends::fs::Context,
        _inode: u64,
        _handle: u64,
        _flags: u32,
        _cmd: u32,
        _arg: u64,
        _in_size: u32,
        _out_size: u32,
        _exit_code: &std::sync::Arc<std::sync::atomic::AtomicI32>,
    ) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn getlk(&self) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn setlk(&self) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn setlkw(&self) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn bmap(&self) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn poll(&self) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn notify_reply(&self) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }
}

// Tests live beside this file rather than inside it: they had grown longer than
// the implementation, so a reader opening it had to scroll past them to find the
// code. They are still a child module, so private items stay reachable.
#[cfg(test)]
#[path = "krun_tests.rs"]
mod tests;
