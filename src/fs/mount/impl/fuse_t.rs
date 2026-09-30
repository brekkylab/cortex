//! Binds a [`Posix`] to FUSE-T through libfuse-t's lowlevel API.
//!
//! Inode identity, handle lifetime, the readdir cursor, open decomposition, attributes and the
//! host errno table live in [`posix`](crate::fs::filesystem::posix); this file only marshals,
//! through the flat vtable of the C shim in `contrib/fuse_t/`, which owns libfuse-t's structs.
//!
//! **Why not `fuser`?** `fuser` speaks the kernel FUSE protocol over the fd from `fuse_mount`
//! itself. FUSE-T's fd is a socket to its helper, which expects libfuse-t's own loop: with a
//! foreign reader, INIT completes, two probes arrive, and the helper hangs up with no mount.

use std::{
    ffi::{CStr, CString, OsStr, c_char, c_int, c_long, c_void},
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use super::super::{
    claim::{Claim, claim, reclaim_abandoned},
    table::{mounts_under, resolved, unmount_under},
};
use crate::fs::{
    FileSystem, Mount, Posix, SetAttr, Stat,
    filesystem::posix::{
        BLOCK_SIZE, NAME_MAX, OpenFlagBits, TOTAL_BLOCKS, TOTAL_INODES, attr_for,
        decode_open_flags, host_errno, mode_for, unix_time,
    },
};

/// Open flags in the host's numbering: this reply goes to this host's kernel.
const HOST_OPEN_FLAGS: OpenFlagBits = OpenFlagBits {
    truncate: libc::O_TRUNC,
    create: libc::O_CREAT,
    create_new: libc::O_EXCL,
};

/// Mirror of `struct cortex_stat` in `contrib/fuse_t/shim.h`. Fixed-width fields and an
/// explicit pad, so the layouts match without relying on alignment rules.
#[repr(C)]
#[derive(Default)]
struct CortexStat {
    ino: u64,
    size: u64,
    blocks: u64,
    mode: u32,
    nlink: u32,
    blksize: u32,
    _pad: u32,
    mtime: i64,
    mtime_nsec: i64,
    atime: i64,
    atime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

fn to_cortex_stat(inode: u64, stat: &Stat) -> CortexStat {
    let attr = attr_for(stat);
    let (mtime, mtime_nsec) = unix_time(attr.mtime);
    let (atime, atime_nsec) = unix_time(attr.atime);
    let (ctime, ctime_nsec) = unix_time(attr.ctime);
    CortexStat {
        ino: inode,
        size: attr.size,
        blocks: attr.blocks,
        mode: attr.mode,
        nlink: attr.nlink,
        blksize: attr.blksize,
        _pad: 0,
        mtime,
        mtime_nsec,
        atime,
        atime_nsec,
        ctime,
        ctime_nsec,
    }
}

/// Emits one directory entry, returning non-zero once the kernel's buffer is full.
/// Implemented in C, which owns `fuse_add_direntry`'s accounting.
type DirentSink = unsafe extern "C" fn(*mut c_void, u64, *const c_char, u32, u64) -> c_int;

/// Mirror of `struct cortex_fuse_t_ops`. Field order is the contract.
#[repr(C)]
struct Ops {
    lookup:
        unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64, *mut CortexStat) -> c_int,
    getattr: unsafe extern "C" fn(*mut c_void, u64, *mut CortexStat) -> c_int,
    setattr:
        unsafe extern "C" fn(*mut c_void, u64, u64, c_int, u64, c_int, *mut CortexStat) -> c_int,
    open: unsafe extern "C" fn(*mut c_void, u64, c_int, *mut u64) -> c_int,
    create: unsafe extern "C" fn(
        *mut c_void,
        u64,
        *const c_char,
        c_int,
        *mut u64,
        *mut u64,
        *mut CortexStat,
    ) -> c_int,
    read: unsafe extern "C" fn(*mut c_void, u64, u64, u64, *mut c_char) -> c_long,
    write: unsafe extern "C" fn(*mut c_void, u64, u64, u64, *const c_char) -> c_long,
    flush: unsafe extern "C" fn(*mut c_void, u64) -> c_int,
    release: unsafe extern "C" fn(*mut c_void, u64) -> c_int,
    mkdir:
        unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64, *mut CortexStat) -> c_int,
    unlink: unsafe extern "C" fn(*mut c_void, u64, *const c_char) -> c_int,
    rmdir: unsafe extern "C" fn(*mut c_void, u64, *const c_char) -> c_int,
    rename: unsafe extern "C" fn(*mut c_void, u64, *const c_char, u64, *const c_char) -> c_int,
    readdir: unsafe extern "C" fn(*mut c_void, u64, u64, *mut c_void, DirentSink) -> c_int,
    forget: unsafe extern "C" fn(*mut c_void, u64, u64),
    total_blocks: u64,
    total_inodes: u64,
    block_size: u32,
    name_max: u32,
}

unsafe extern "C" {
    fn cortex_fuse_t_mount(
        mountpoint: *const c_char,
        fsname: *const c_char,
        backend: *const c_char,
        fs: *mut c_void,
        ops: *const Ops,
    ) -> *mut c_void;
    fn cortex_fuse_t_loop(session: *mut c_void) -> c_int;
    fn cortex_fuse_t_stop(session: *mut c_void);
    fn cortex_fuse_t_destroy(session: *mut c_void);
}

/// Recover the filesystem from the opaque pointer the shim carries for us.
///
/// # Safety
/// `fs` must be the pointer given to `cortex_fuse_t_mount`, and the `Posix<T>` behind it must
/// outlive the session ([`FuseTMount`] keeps it boxed until the loop returns).
unsafe fn recover<'a, T: FileSystem>(fs: *mut c_void) -> &'a Posix<T> {
    unsafe { &*(fs as *const Posix<T>) }
}

/// `0` for success, or the negative errno the shim expects.
fn code(result: io::Result<()>) -> c_int {
    match result {
        Ok(()) => 0,
        Err(err) => -host_errno(&err),
    }
}

// Callbacks are generic in `T` and monomorphised per store by `ops_for`: no trait object, no
// downcast.

unsafe extern "C" fn lookup<T: FileSystem>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
    out_inode: *mut u64,
    out: *mut CortexStat,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    code(
        super::block_on(fs.lookup_child(parent, name)).map(|(inode, stat)| unsafe {
            *out_inode = inode;
            *out = to_cortex_stat(inode, &stat);
        }),
    )
}

unsafe extern "C" fn getattr<T: FileSystem>(
    fs: *mut c_void,
    inode: u64,
    out: *mut CortexStat,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.stat_inode(inode)).map(|stat| unsafe {
        *out = to_cortex_stat(inode, &stat);
    }))
}

/// `fh` is ignored: the inode already names the path, and stores keep no per-handle state.
unsafe extern "C" fn setattr<T: FileSystem>(
    fs: *mut c_void,
    inode: u64,
    _fh: u64,
    _has_fh: c_int,
    size: u64,
    has_size: c_int,
    out: *mut CortexStat,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let want = SetAttr {
        // The flag, not the value: zero is an ordinary size to ask for.
        size: (has_size != 0).then_some(size),
        ..Default::default()
    };
    code(
        super::block_on(fs.setattr_inode(inode, want)).map(|stat| unsafe {
            *out = to_cortex_stat(inode, &stat);
        }),
    )
}

unsafe extern "C" fn open<T: FileSystem>(
    fs: *mut c_void,
    inode: u64,
    flags: c_int,
    out_fh: *mut u64,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let options = match decode_open_flags(flags, &HOST_OPEN_FLAGS) {
        Ok(options) => options,
        Err(err) => return -host_errno(&err),
    };
    code(
        super::block_on(fs.open_inode(inode, options)).map(|fh| unsafe {
            *out_fh = fh;
        }),
    )
}

unsafe extern "C" fn create<T: FileSystem>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
    flags: c_int,
    out_inode: *mut u64,
    out_fh: *mut u64,
    out: *mut CortexStat,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    // The opcode itself means "make it if absent", whatever the flags word says.
    let options = match decode_open_flags(flags, &HOST_OPEN_FLAGS) {
        Ok(options) => options.create(true),
        Err(err) => return -host_errno(&err),
    };

    code(
        super::block_on(fs.create_child(parent, name, options)).map(|(inode, stat, fh)| unsafe {
            *out_inode = inode;
            *out_fh = fh;
            *out = to_cortex_stat(inode, &stat);
        }),
    )
}

unsafe extern "C" fn read<T: FileSystem>(
    fs: *mut c_void,
    fh: u64,
    offset: u64,
    size: u64,
    buf: *mut c_char,
) -> c_long {
    let fs = unsafe { recover::<T>(fs) };
    match super::block_on(fs.read_handle(fh, offset, size as u32)) {
        Ok(data) => {
            // The shim allocated `size`; a short read is EOF, so copy only what arrived
            // and let the count say so.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len()) };
            data.len() as c_long
        }
        Err(err) => -host_errno(&err) as c_long,
    }
}

unsafe extern "C" fn write<T: FileSystem>(
    fs: *mut c_void,
    fh: u64,
    offset: u64,
    size: u64,
    buf: *const c_char,
) -> c_long {
    let fs = unsafe { recover::<T>(fs) };
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, size as usize) };
    match super::block_on(fs.write_handle(fh, offset, data)) {
        Ok(written) => written as c_long,
        Err(err) => -host_errno(&err) as c_long,
    }
}

unsafe extern "C" fn flush<T: FileSystem>(fs: *mut c_void, fh: u64) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.flush_handle(fh)))
}

unsafe extern "C" fn release<T: FileSystem>(fs: *mut c_void, fh: u64) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.release_handle(fh)))
}

unsafe extern "C" fn mkdir<T: FileSystem>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
    out_inode: *mut u64,
    out: *mut CortexStat,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    code(
        super::block_on(fs.mkdir_child(parent, name)).map(|(inode, stat)| unsafe {
            *out_inode = inode;
            *out = to_cortex_stat(inode, &stat);
        }),
    )
}

unsafe extern "C" fn unlink<T: FileSystem>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    code(super::block_on(fs.unlink_child(parent, name)))
}

unsafe extern "C" fn rmdir<T: FileSystem>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    code(super::block_on(fs.rmdir_child(parent, name)))
}

/// No flags: libfuse-t's `rename` has none, so `RENAME_NOREPLACE`/`RENAME_EXCHANGE` never
/// arrive.
unsafe extern "C" fn rename<T: FileSystem>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
    newparent: u64,
    newname: *const c_char,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    let newname = OsStr::from_bytes(unsafe { CStr::from_ptr(newname) }.to_bytes());
    code(super::block_on(
        fs.rename_child(parent, name, newparent, newname),
    ))
}

unsafe extern "C" fn readdir<T: FileSystem>(
    fs: *mut c_void,
    inode: u64,
    offset: u64,
    sink: *mut c_void,
    emit: DirentSink,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.for_each_dirent(
        inode,
        offset,
        |child_inode, child, cursor| {
            // An interior NUL cannot go to C, and no well-behaved store produces one.
            let name = CString::new(child.name.as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidFilename))?;
            let stop = unsafe {
                emit(
                    sink,
                    child_inode,
                    name.as_ptr(),
                    mode_for(child.kind),
                    cursor,
                )
            };
            Ok(stop != 0)
        },
    )))
}

unsafe extern "C" fn forget<T: FileSystem>(fs: *mut c_void, inode: u64, nlookup: u64) {
    let fs = unsafe { recover::<T>(fs) };
    fs.forget_inode(inode, nlookup);
}

/// The vtable for one concrete store, with every callback monomorphised for it.
fn ops_for<T: FileSystem>() -> Ops {
    Ops {
        lookup: lookup::<T>,
        getattr: getattr::<T>,
        setattr: setattr::<T>,
        open: open::<T>,
        create: create::<T>,
        read: read::<T>,
        write: write::<T>,
        flush: flush::<T>,
        release: release::<T>,
        mkdir: mkdir::<T>,
        unlink: unlink::<T>,
        rmdir: rmdir::<T>,
        rename: rename::<T>,
        readdir: readdir::<T>,
        forget: forget::<T>,
        total_blocks: TOTAL_BLOCKS,
        total_inodes: TOTAL_INODES,
        block_size: BLOCK_SIZE as u32,
        name_max: NAME_MAX,
    }
}

/// The mount's source name, as `df` and Finder show it. A `CStr` literal, so there is no
/// allocation or invalid-name case.
const FSNAME: &CStr = c"cortex";

/// How long to wait for FUSE-T to finish mounting. Generous: the helper has to start,
/// negotiate, and get the kernel to complete a mount.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a drop gives the mount table to catch up with a forceful unmount that has
/// already been accepted. Short: this is bookkeeping settling, not work being done.
const UNMOUNT_SETTLE: Duration = Duration::from_secs(3);

/// How long a drop waits for the serving thread to notice its channel is gone. Generous, since
/// overrunning leaks the session and thread; bounded, so the destructor always returns.
const LOOP_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Which of FUSE-T's transports serves the mount.
///
/// A single mount option; the same vtable serves every transport, and only what the kernel
/// talks to differs.
///
/// [`FuseTMount::try_new`] passes none, leaving the choice to `fuse-t.ini`; naming one
/// overrides that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuseTBackend {
    /// An NFSv4 server in FUSE-T's helper, mounted as an NFS client. The `fuse-t.ini` default.
    Nfs,

    /// FUSE-T's FSKit module (macOS 15's file-system extension framework), shipped signed
    /// inside `fuse-t.app`.
    ///
    /// A mount option rather than a binding: direct FSKit needs a Swift app extension, its
    /// entitlement, and a filesystem in a system-launched process. FUSE-T's helper already
    /// has those and bridges its RPC to the extension.
    FsKit,

    /// An SMB server in the helper, mounted as an SMB client.
    Smb,
}

impl FuseTBackend {
    /// The spelling FUSE-T's `backend=` option takes.
    fn option(self) -> &'static CStr {
        match self {
            FuseTBackend::Nfs => c"nfs",
            FuseTBackend::FsKit => c"fskit",
            FuseTBackend::Smb => c"smb",
        }
    }
}

/// The session pointer, sent to the serving thread.
///
/// # Safety
/// Touched only by the serving thread and by [`FuseTMount`]'s `Drop`, which runs after that
/// thread is joined, never from two threads at once.
struct SessionPtr(*mut c_void);
unsafe impl Send for SessionPtr {}

impl SessionPtr {
    /// A method so a `move` closure captures the whole `Send` wrapper: since Rust 2021, naming
    /// `.0` would capture only the bare, non-`Send` `*mut c_void`.
    fn get(&self) -> *mut c_void {
        self.0
    }
}

/// A live FUSE-T mount: constructing one mounts, dropping it unmounts.
///
/// **Not** generic in the store, though [`try_new`](Self::try_new) is: the type only matters
/// for building the vtable, so carrying it would spread a parameter onto every holder.
pub struct FuseTMount {
    session: *mut c_void,

    /// `None` once [`join`](Self::join) collected it, so `Drop` does not wait again.
    thread: Option<JoinHandle<c_int>>,

    mountpoint: PathBuf,

    /// Kept alive for the mount's life because the shim holds a raw pointer into it that the
    /// serving thread dereferences on every request.
    ///
    /// Fields drop after the `drop` body, so normally this is freed after `destroy`. If the
    /// serving thread outlived its deadline, `Drop` forgets it instead, since the loop still
    /// holds the pointer.
    ///
    /// `dyn Send + Sync`, not `dyn Any`, so it does not advertise a downcast.
    _fs: Option<Box<dyn Send + Sync>>,

    /// This process's ownership of the mount point, read by
    /// [`unmount_on_signal`](crate::fs::unmount_on_signal) and, if this process is killed, a
    /// later run's [`reclaim_abandoned`]. Dropping it gives the
    /// mount point up.
    _claim: Claim,
}

impl FuseTMount {
    /// Mount `fs` at `mountpoint`, serve it from a background thread, and return once the
    /// mount is real.
    ///
    /// `mountpoint` must already exist and be empty. Requires FUSE-T
    /// (`brew install --cask fuse-t`) — no kernel extension, no reboot (macFUSE is a kext
    /// needing reduced-security boot on Apple Silicon).
    ///
    /// **Blocks until the mount is real.** `fuse_mount` returns a session early, but the kernel
    /// attaches it only after the serving thread answers the helper's opening
    /// INIT/STATFS/GETATTR; touching the path before then would see the bare directory or
    /// block on a half-built mount.
    ///
    /// The transport comes from `fuse-t.ini`; [`try_new_with`](Self::try_new_with) names one.
    ///
    /// `'static` because the store is served from that thread for the mount's lifetime.
    pub fn try_new<T: FileSystem + 'static>(fs: T, mountpoint: &Path) -> io::Result<Self> {
        Self::mount(fs, mountpoint, std::ptr::null())
    }

    /// [`try_new`](Self::try_new) with the transport named; see [`FuseTBackend`].
    pub fn try_new_with<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        backend: FuseTBackend,
    ) -> io::Result<Self> {
        Self::mount(fs, mountpoint, backend.option().as_ptr())
    }

    /// `backend` is a C string, or null to leave the choice to FUSE-T.
    fn mount<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        backend: *const c_char,
    ) -> io::Result<Self> {
        // Before anything reaches the shim: without FUSE-T its `dlsym` pointers are null.
        crate::fs::mount_support()?;

        // Clears mounts a `SIGKILL`ed run left; live siblings' mounts are untouched.
        reclaim_abandoned();

        let c_mountpoint = CString::new(mountpoint.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidFilename))?;

        // Boxed and never moved, so the shim's pointer stays valid for the mount's life.
        let fs: Box<Posix<T>> = Box::new(Posix::new(fs));
        let fs_ptr = &*fs as *const Posix<T> as *mut c_void;
        let ops = ops_for::<T>();

        let session = unsafe {
            cortex_fuse_t_mount(
                c_mountpoint.as_ptr(),
                FSNAME.as_ptr(),
                backend,
                fs_ptr,
                &ops,
            )
        };
        if session.is_null() {
            return Err(io::Error::other(format!(
                "FUSE-T could not mount {}: is fuse-t installed, and does the mount point \
                 exist and is it empty?",
                mountpoint.display()
            )));
        }

        let sendable = SessionPtr(session);
        let thread = std::thread::Builder::new()
            .name("cortex-fuse-t".into())
            .spawn(move || unsafe { cortex_fuse_t_loop(sendable.get()) })
            .inspect_err(|_| {
                // No guard exists yet to clean up, and nothing is serving, so tear down here
                // rather than leave the mount wedged.
                unsafe { cortex_fuse_t_destroy(session) };
            })?;

        // From here every exit, including the `Err` below, unmounts by dropping the guard.
        let mount = FuseTMount {
            session,
            thread: Some(thread),
            mountpoint: mountpoint.to_path_buf(),
            _fs: Some(fs),
            _claim: claim(mountpoint),
        };
        if !mount.wait_until_mounted(MOUNT_TIMEOUT) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "FUSE-T did not finish mounting {} within {MOUNT_TIMEOUT:?}",
                    mountpoint.display()
                ),
            ));
        }
        Ok(mount)
    }

    /// Serve until the mount goes away, then take it down.
    ///
    /// Waits for something else to end the mount (`umount`, `diskutil unmount`, or the helper
    /// dying). **It does not unmount**; to end the mount, drop the guard.
    ///
    /// `Err` if the serving loop ended badly or its thread panicked. (A panic in an
    /// `extern "C"` callback aborts instead.)
    pub fn join(mut self) -> io::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        match thread.join() {
            Ok(0) => Ok(()),
            Ok(code) => Err(io::Error::other(format!(
                "the FUSE-T session for {} ended with {code}",
                self.mountpoint.display()
            ))),
            Err(_) => Err(io::Error::other(format!(
                "the thread serving {} panicked",
                self.mountpoint.display()
            ))),
        }
    }

    /// Poll until the mountpoint is a mount point, or give up.
    ///
    /// A device id differing from the parent's means something is mounted, whichever backend
    /// serves it (none of them is a FUSE mount).
    ///
    /// Sleeps between polls, holding the mounting thread; usually a few polls.
    fn wait_until_mounted(&self, timeout: Duration) -> bool {
        use std::os::unix::fs::MetadataExt;

        let Some(parent) = self.mountpoint.parent() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        loop {
            match (
                std::fs::metadata(&self.mountpoint),
                std::fs::metadata(parent),
            ) {
                (Ok(here), Ok(above)) if here.dev() != above.dev() => return true,
                _ => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Mount for FuseTMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

/// Not derived only because of the session pointer. [`Mount`] requires both.
///
/// # Safety
/// Nothing reachable through `&FuseTMount` touches the pointer. Only the serving thread
/// (handed it at `mount`) and `Drop` (`&mut self`, joining that thread before releasing the
/// session) use it. `Drop` never runs on the serving thread, so which thread it runs on does
/// not matter.
unsafe impl Send for FuseTMount {}
unsafe impl Sync for FuseTMount {}

impl Drop for FuseTMount {
    /// Unmount, stop the loop, join, unmount again if it was busy, release the session; in
    /// that order and at most once.
    ///
    /// Two unmounts on purpose: the first runs while still served, so cached writes flush and
    /// a busy mount refuses (`EBUSY`) rather than being pulled from under a reader; the second
    /// runs after the loop stops, which makes readers let go.
    ///
    /// # Why not `fuse_unmount`
    ///
    /// It breaks with two mounts in one process. libfuse-t keeps the helper's pid in one global
    /// (`_cpid`, set by `fuse_mount_core` after `fork`, overwritten by every mount), and
    /// `fuse_kern_unmount` ends in a blocking `waitpid` on it: an unmount waits on the last
    /// mounted helper, which will not exit until its own mount goes, so `drop(a); drop(b)`
    /// hangs in `wait4`. `_mount_wait_thread` is a single global too. So unmounting is a
    /// bounded `umount` in a child process, which needs no libfuse-t global; the rest is
    /// per-session: the shim ends the loop, this joins its thread, the shim frees the session.
    ///
    /// Every step is bounded, so a mount that refuses both attempts leaves this returning
    /// anyway after reporting on stderr, rather than a destructor that never returns.
    ///
    /// Never panics: a panic mid-unwind aborts, hiding a failing test's real assertion.
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        let session = std::mem::replace(&mut self.session, std::ptr::null_mut());
        let mountpoint = resolved(&self.mountpoint);

        // While still served, so a cached write can flush.
        let was_busy = !unmount_under(&mountpoint);

        // Stop serving regardless; that releases anything still reading through the mount.
        unsafe { cortex_fuse_t_stop(session) };

        // Joined before freeing the session, which the loop reads, with a deadline so a stuck
        // thread cannot hang the destructor. On overrun the session is leaked, since freeing
        // it under a live loop is a use-after-free.
        let collected = match self.thread.take() {
            Some(thread) if thread_ends(&thread, LOOP_EXIT_TIMEOUT) => {
                let _ = thread.join();
                true
            }
            Some(_) => false,
            // Already collected by `join`.
            None => true,
        };

        // Nothing is served now, so a mount that was busy is worth one more try.
        let left = if was_busy {
            unmount_under(&mountpoint);
            // Polled: the table lags an accepted forceful unmount, so one read would falsely
            // report every busy teardown.
            settle(&mountpoint, UNMOUNT_SETTLE)
        } else {
            Vec::new()
        };

        // Reported, not propagated: what is left must be cleared by hand.
        for survivor in left {
            eprintln!(
                "cortex: {} would not unmount — take it down by hand",
                survivor.display()
            );
        }
        if !collected {
            eprintln!(
                "cortex: the thread serving {} did not stop within {LOOP_EXIT_TIMEOUT:?}; \
                 leaving its session and filesystem allocated",
                self.mountpoint.display()
            );
            // Leaked too: the still-running loop dereferences the shim's pointer into it.
            std::mem::forget(self._fs.take());
            return;
        }
        unsafe { cortex_fuse_t_destroy(session) };
    }
}

/// Wait for the mount table to stop naming anything at `mountpoint`; returns what it still
/// names at the deadline (empty on success).
fn settle(mountpoint: &Path, timeout: Duration) -> Vec<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = mounts_under(mountpoint);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Whether `thread` finishes inside `timeout`.
///
/// Polled, because `JoinHandle::join` has no timed form.
fn thread_ends(thread: &JoinHandle<c_int>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    true
}
