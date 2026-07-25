//! Binds a [`PosixAdapter`] to `msb_krun`'s FUSE-shaped [`DynFileSystem`].
//!
//! This is the only place the inode bookkeeping in [`super::posix`] meets
//! `msb_krun`: it translates FUSE calls into backend operations and backend
//! errors/metadata into the `stat64`/errno shapes the guest kernel expects.

use std::ffi::CStr;
use std::io::Result;
use std::time::Duration;

use msb_krun::{
    DynFileSystem,
    backends::fs::{Context, Entry, FsOptions, stat64},
};

use super::PosixAdapter;
use crate::mountable::MountableV2;
use crate::{CortexError, DirentKind, Stat};

// The guest kernel is always Linux, so every reply must carry Linux errno
// numbers. The host's `libc` values differ (e.g. ENOSYS is 78 on macOS but 38
// on Linux) and would be misread by the guest.
const LINUX_ENOENT: i32 = 2;
const LINUX_EIO: i32 = 5;
const LINUX_EEXIST: i32 = 17;
const LINUX_ENOTDIR: i32 = 20;
const LINUX_EISDIR: i32 = 21;
const LINUX_EINVAL: i32 = 22;
const LINUX_ENOSYS: i32 = 38;

/// How long the guest may cache a lookup/attribute reply.
const TTL: Duration = Duration::from_secs(1);

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
        CortexError::InvalidName => LINUX_EINVAL,
        CortexError::Unsupported => LINUX_ENOSYS,
        CortexError::Io(_) => LINUX_EIO,
    })
}

/// Project a backend [`Stat`] onto the `stat64` the guest kernel reads back.
fn to_stat64(inode: u64, stat: &Stat) -> stat64 {
    // SAFETY: `stat64` is a repr(C) POD of integers; all-zero is a valid value
    // and we set every field the kernel inspects.
    let mut st: stat64 = unsafe { std::mem::zeroed() };
    st.st_ino = inode as _;
    st.st_size = stat.size as _;
    st.st_nlink = 1 as _;
    st.st_mode = match stat.kind {
        DirentKind::Dir => (libc::S_IFDIR | 0o755) as _,
        DirentKind::File => (libc::S_IFREG | 0o644) as _,
    };
    st
}

impl<T: MountableV2> DynFileSystem for PosixAdapter<T> {
    fn init(&self, _capable: FsOptions) -> Result<FsOptions> {
        Ok(FsOptions::empty())
    }

    fn destroy(&self) {}

    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> Result<Entry> {
        let name = name.to_str().map_err(|_| errno(LINUX_EINVAL))?;

        // Resolve the parent inode to a path, then release the lock before we
        // touch the (possibly slow) backend.
        let parent_path = {
            let table = self.inodes.lock().unwrap();
            table.path_of(parent).ok_or_else(|| errno(LINUX_ENOENT))?
        };
        let child = parent_path.join(name);

        let stat = self.mountable.stat(&child).map_err(to_errno)?;

        // Only now that the entry is known to exist do we assign it an inode.
        let inode = self.inodes.lock().unwrap().intern(child);
        Ok(Entry {
            inode,
            generation: 0,
            attr: to_stat64(inode, &stat),
            attr_flags: 0,
            attr_timeout: TTL,
            entry_timeout: TTL,
        })
    }

    fn forget(&self, _ctx: Context, inode: u64, count: u64) {
        self.inodes.lock().unwrap().forget(inode, count);
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
        let path = {
            let table = self.inodes.lock().unwrap();
            table.path_of(inode).ok_or_else(|| errno(LINUX_ENOENT))?
        };
        let stat = self.mountable.stat(&path).map_err(to_errno)?;
        Ok((to_stat64(inode, &stat), TTL))
    }

    fn setattr(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        attr: msb_krun::backends::fs::stat64,
        handle: Option<u64>,
        valid: msb_krun::backends::fs::SetattrValid,
    ) -> std::io::Result<(msb_krun::backends::fs::stat64, std::time::Duration)> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn readlink(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
    ) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn symlink(
        &self,
        ctx: msb_krun::backends::fs::Context,
        linkname: &std::ffi::CStr,
        parent: u64,
        name: &std::ffi::CStr,
        extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn mknod(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        name: &std::ffi::CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn mkdir(
        &self,
        ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
        mode: u32,
        umask: u32,
        extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn unlink(
        &self,
        ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn rmdir(
        &self,
        ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn rename(
        &self,
        ctx: msb_krun::backends::fs::Context,
        olddir: u64,
        oldname: &std::ffi::CStr,
        newdir: u64,
        newname: &std::ffi::CStr,
        flags: u32,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn link(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        newparent: u64,
        newname: &std::ffi::CStr,
    ) -> std::io::Result<msb_krun::backends::fs::Entry> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn open(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        kill_priv: bool,
        flags: u32,
    ) -> std::io::Result<(Option<u64>, msb_krun::backends::fs::OpenOptions)> {
        Ok((None, msb_krun::backends::fs::OpenOptions::empty()))
    }

    fn create(
        &self,
        ctx: msb_krun::backends::fs::Context,
        parent: u64,
        name: &std::ffi::CStr,
        mode: u32,
        kill_priv: bool,
        flags: u32,
        umask: u32,
        extensions: msb_krun::backends::fs::Extensions,
    ) -> std::io::Result<(
        msb_krun::backends::fs::Entry,
        Option<u64>,
        msb_krun::backends::fs::OpenOptions,
    )> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn read(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        w: &mut dyn msb_krun::backends::fs::ZeroCopyWriter,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn write(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        r: &mut dyn msb_krun::backends::fs::ZeroCopyReader,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn flush(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        lock_owner: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn fsync(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        datasync: bool,
        handle: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn fallocate(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn release(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        flags: u32,
        handle: u64,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn statfs(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
    ) -> std::io::Result<msb_krun::backends::fs::statvfs64> {
        // Safe because we are zero-initializing a struct with only POD fields.
        let mut st: msb_krun::backends::fs::statvfs64 = unsafe { std::mem::zeroed() };
        st.f_namemax = 255;
        st.f_bsize = 512;
        Ok(st)
    }

    fn setxattr(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        name: &std::ffi::CStr,
        value: &[u8],
        flags: u32,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn getxattr(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        name: &std::ffi::CStr,
        size: u32,
    ) -> std::io::Result<msb_krun::backends::fs::GetxattrReply> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn listxattr(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        size: u32,
    ) -> std::io::Result<msb_krun::backends::fs::ListxattrReply> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn removexattr(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        name: &std::ffi::CStr,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn opendir(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        flags: u32,
    ) -> std::io::Result<(Option<u64>, msb_krun::backends::fs::OpenOptions)> {
        Ok((None, msb_krun::backends::fs::OpenOptions::empty()))
    }

    fn readdir(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
    ) -> std::io::Result<Vec<msb_krun::backends::fs::DirEntry<'static>>> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn readdir_for_each(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
        add_entry: &mut msb_krun::backends::fs::AddDirEntry<'_>,
    ) -> std::io::Result<()> {
        let entries = self.readdir(ctx, inode, handle, size, offset)?;
        for entry in entries {
            match add_entry(entry) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        size: u32,
        offset: u64,
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
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        datasync: bool,
        handle: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn releasedir(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        flags: u32,
        handle: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn access(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        mask: u32,
    ) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn lseek(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        offset: u64,
        whence: u32,
    ) -> std::io::Result<u64> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    fn copyfilerange(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode_in: u64,
        handle_in: u64,
        offset_in: u64,
        inode_out: u64,
        handle_out: u64,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(LINUX_ENOSYS))
    }

    // `setupmapping`/`removemapping` (DAX) are left to the trait's ENOSYS
    // defaults: their macOS/Windows signatures name `Sender<WorkerMessage>`,
    // types the `msb_krun` facade doesn't re-export, so we can't restate them.

    fn ioctl(
        &self,
        ctx: msb_krun::backends::fs::Context,
        inode: u64,
        handle: u64,
        flags: u32,
        cmd: u32,
        arg: u64,
        in_size: u32,
        out_size: u32,
        exit_code: &std::sync::Arc<std::sync::atomic::AtomicI32>,
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
