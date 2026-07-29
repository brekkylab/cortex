//! Binds a [`PosixFs`] to `msb_krun`'s FUSE-shaped [`DynFileSystem`].
//!
//! This is the only place the inode bookkeeping in [`super::posix`] meets
//! `msb_krun`: it translates FUSE calls into backend operations and backend
//! errors/metadata into the `stat64`/errno shapes the guest kernel expects.

use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::io::{Read, Result, Seek, SeekFrom, Write};

use msb_krun::{
    DynFileSystem,
    backends::fs::{Context, DirEntry, Entry, FsOptions, stat64},
};

use crate::mountable::PosixFs;
use crate::mountable::posix::{
    BLOCK_SIZE, NAME_MAX, OpenFlagBits, TOTAL_BLOCKS, TOTAL_INODES, TTL, attr_for,
    decode_open_flags, unix_time,
};
use crate::mountable::Mountable;
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
            size: valid.contains(SetattrValid::SIZE).then_some(attr.st_size as u64),
            mode: valid.contains(SetattrValid::MODE).then_some(attr.st_mode as u32),
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
        _olddir: u64,
        _oldname: &std::ffi::CStr,
        _newdir: u64,
        _newname: &std::ffi::CStr,
        _flags: u32,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
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
        let mut options = decode_open_flags(flags as i32, &LINUX_OPEN_FLAGS).map_err(to_errno)?;
        options.create = true;

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

#[cfg(test)]
mod tests {
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
    use msb_krun::backends::fs::Extensions;
    use crate::{InMemVolume, PosixFs};
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
        Context { uid: 0, gid: 0, pid: 0 }
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
            Err(err) => err.raw_os_error().expect("filesystem errors carry an errno"),
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
            .create(ctx(), ROOT, c"fresh.txt", 0o644, false, GUEST_O_WRONLY, 0, Extensions::default())
            .unwrap();
        let handle = handle.expect("create hands back a file handle");
        assert_eq!(entry.attr.st_size, 0);

        // WRITE through the ZeroCopy path: the guest delivers via a file
        // descriptor, not a slice, which is why this binding stages.
        let mut source = Supplied(b"written".to_vec());
        let n = fs
            .write(ctx(), entry.inode, handle, &mut source, 7, 0, None, false, false, 0)
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
        assert_eq!(fs.lookup(ctx(), ROOT, c"fresh.txt").unwrap().inode, entry.inode);

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
        let dir = fs.mkdir(ctx(), ROOT, c"made", 0o755, 0, Extensions::default()).unwrap();
        assert_eq!(dir.attr.st_nlink as u32, 2);

        // A file inside blocks removal and `unlink` refuses the directory, so the
        // guest has to decompose `rm -rf` itself.
        let (child, child_fh, _) = fs
            .create(ctx(), dir.inode, c"inside", 0o644, false, GUEST_O_WRONLY, 0, Extensions::default())
            .unwrap();
        fs.release(ctx(), child.inode, 0, child_fh.unwrap(), false, false, None)
            .unwrap();
        assert_eq!(
            expect_errno(fs.rmdir(ctx(), ROOT, c"made")),
            LINUX_ENOTEMPTY
        );
        assert_eq!(
            expect_errno(fs.unlink(ctx(), ROOT, c"made")),
            LINUX_EISDIR
        );

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
        assert_eq!(file.attr.st_mode as u32 & libc::S_IFREG as u32, libc::S_IFREG as u32);
        assert_eq!(file.attr.st_ino as u64, file.inode);

        let dir = fs.lookup(ctx(), ROOT, c"sub").unwrap();
        assert_eq!(dir.attr.st_mode as u32 & libc::S_IFDIR as u32, libc::S_IFDIR as u32);
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
            .read(ctx(), entry.inode, handle, &mut out, CONTENT.len() as u32, 0, None, 0)
            .unwrap();
        assert_eq!(n, CONTENT.len());
        assert_eq!(out.0, CONTENT);

        // A window past the end is a short read, which is how the guest learns
        // where the file stops.
        let mut tail = Collected(Vec::new());
        let n = fs
            .read(ctx(), entry.inode, handle, &mut tail, 4096, CONTENT.len() as u64, None, 0)
            .unwrap();
        assert_eq!(n, 0);
        assert!(tail.0.is_empty());

        // Offsets address bytes, not blocks.
        let mut mid = Collected(Vec::new());
        fs.read(ctx(), entry.inode, handle, &mut mid, 4, 6, None, 0).unwrap();
        assert_eq!(mid.0, b"from");

        fs.release(ctx(), entry.inode, 0, handle, false, false, None).unwrap();
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
        fs.readdir_for_each(ctx(), ROOT, 0, 4096, 0, &mut collect).unwrap();

        // `.` and `..` lead, both pointing back at this directory, and offsets
        // are 1-based so `0` can keep meaning "from the beginning".
        assert_eq!(seen[0], (".".to_string(), libc::DT_DIR as u32, 1, ROOT));
        assert_eq!(seen[1], ("..".to_string(), libc::DT_DIR as u32, 2, ROOT));

        let names: std::collections::HashMap<_, _> =
            seen[2..].iter().map(|(n, t, _, i)| (n.clone(), (*t, *i))).collect();
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
        fs.readdir_for_each(ctx(), ROOT, 0, 4096, 2, &mut collect).unwrap();
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
        assert_eq!(expect_errno(fs.getattr(ctx(), entry.inode, None)), LINUX_ENOENT);
    }
}
