//! Binds a [`PosixFs`] to FUSE-T through libfuse-t's lowlevel API.
//!
//! Sibling of [`super::krun`] and [`super::fuse`]. Everything it needs — inode
//! identity, handle lifetime, the readdir cursor, atomic truncation, the
//! attribute policy, the host errno table — already lives in
//! [`super::super::posix`], so what is left here is marshalling.
//!
//! **Why not just the `fuser` binding?** They differ in who drives the session.
//! `fuser` takes the fd from `fuse_mount` and speaks the kernel protocol over it
//! itself, which works when that fd is macFUSE's real device. FUSE-T's is a
//! socket to its `go-nfsv4` helper, which expects to be driven *by* libfuse-t's
//! own loop: hand it to a foreign protocol reader and the INIT handshake
//! completes, two probes arrive, then the helper hangs up and no mount appears.
//! (Verified, and verified not to be cortex's fault — a hardcoded `fuser`
//! filesystem fails identically.) In exchange FUSE-T needs no kernel extension,
//! where macFUSE is a kext needing reduced-security boot on Apple Silicon.
//!
//! **Why a C shim?** `fuse_lowlevel_ops` has ~50 function pointers with
//! `__APPLE__`-conditional members, `fuse_file_info` has bitfields, and
//! `fuse_entry_param` embeds a host `struct stat`. A wrong layout in Rust is
//! silent memory corruption, not a compile error, so `contrib/fuse_t/shim.c`
//! owns all of them and exposes a flat vtable of our own design instead.

use std::{
    any::Any,
    ffi::{CStr, CString, OsStr, c_char, c_int, c_long, c_void},
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    CortexError, Result, Stat,
    mountable::{
        Mountable, PosixFs, SetAttr,
        posix::{
            BLOCK_SIZE, NAME_MAX, OpenFlagBits, TOTAL_BLOCKS, TOTAL_INODES, attr_for,
            decode_open_flags, host_errno, mode_for, unix_time,
        },
    },
};

/// Open flags in the host's numbering, exactly as the `fuser` binding uses —
/// this reply also goes to this host's kernel.
const HOST_OPEN_FLAGS: OpenFlagBits = OpenFlagBits {
    append: libc::O_APPEND,
    truncate: libc::O_TRUNC,
    create: libc::O_CREAT,
    create_new: libc::O_EXCL,
};

/// How long to wait for FUSE-T to finish mounting. Generous: the helper has to
/// start, negotiate, and get the kernel to complete an NFS mount.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Mirror of `struct cortex_stat` in `contrib/fuse_t/shim.h`. All fixed-width
/// with an explicit pad, so keeping the two in step is a matter of reading rather
/// than of trusting alignment rules.
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

/// Emits one directory entry, returning non-zero once the kernel's buffer is
/// full. Implemented on the C side, which owns `fuse_add_direntry`'s accounting.
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
        fs: *mut c_void,
        ops: *const Ops,
    ) -> *mut c_void;
    fn cortex_fuse_t_loop(session: *mut c_void) -> c_int;
    fn cortex_fuse_t_unmount(session: *mut c_void);
    fn cortex_fuse_t_destroy(session: *mut c_void);
}

/// Recover the filesystem from the opaque pointer the shim carries for us.
///
/// # Safety
/// `fs` must be the pointer given to `cortex_fuse_t_mount`, and the `PosixFs<T>`
/// behind it must outlive the session — [`FuseTMount`] boxes it and keeps it
/// until after the loop returns.
unsafe fn recover<'a, T: Mountable>(fs: *mut c_void) -> &'a PosixFs<T> {
    unsafe { &*(fs as *const PosixFs<T>) }
}

/// `0` for success, or the negative errno the shim expects.
fn code(result: Result<()>) -> c_int {
    match result {
        Ok(()) => 0,
        Err(err) => -host_errno(&err),
    }
}

// Each callback is generic in `T` and monomorphised per backend by `ops_for`, so
// the filesystem type stays static — no trait object, no downcast.

unsafe extern "C" fn lookup<T: Mountable>(
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

unsafe extern "C" fn getattr<T: Mountable>(
    fs: *mut c_void,
    inode: u64,
    out: *mut CortexStat,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.stat_inode(inode)).map(|stat| unsafe {
        *out = to_cortex_stat(inode, &stat);
    }))
}

unsafe extern "C" fn setattr<T: Mountable>(
    fs: *mut c_void,
    inode: u64,
    fh: u64,
    has_fh: c_int,
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
    let handle = (has_fh != 0).then_some(fh);
    code(
        super::block_on(fs.setattr_inode(inode, handle, want)).map(|stat| unsafe {
            *out = to_cortex_stat(inode, &stat);
        }),
    )
}

unsafe extern "C" fn open<T: Mountable>(
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
        super::block_on(fs.open_inode(inode, options)).map(|(fh, _stat)| unsafe {
            *out_fh = fh;
        }),
    )
}

unsafe extern "C" fn create<T: Mountable>(
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

unsafe extern "C" fn read<T: Mountable>(
    fs: *mut c_void,
    fh: u64,
    offset: u64,
    size: u64,
    buf: *mut c_char,
) -> c_long {
    let fs = unsafe { recover::<T>(fs) };
    match super::block_on(fs.read_handle(fh, offset, size as u32)) {
        Ok(data) => {
            // The shim allocated `size`; a short read is EOF, so copy only what
            // arrived and let the count say so.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len()) };
            data.len() as c_long
        }
        Err(err) => -host_errno(&err) as c_long,
    }
}

unsafe extern "C" fn write<T: Mountable>(
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

unsafe extern "C" fn flush<T: Mountable>(fs: *mut c_void, fh: u64) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.flush_handle(fh)))
}

unsafe extern "C" fn release<T: Mountable>(fs: *mut c_void, fh: u64) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    code(super::block_on(fs.release_handle(fh)))
}

unsafe extern "C" fn mkdir<T: Mountable>(
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

unsafe extern "C" fn unlink<T: Mountable>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    code(super::block_on(fs.unlink_child(parent, name)))
}

unsafe extern "C" fn rmdir<T: Mountable>(
    fs: *mut c_void,
    parent: u64,
    name: *const c_char,
) -> c_int {
    let fs = unsafe { recover::<T>(fs) };
    let name = OsStr::from_bytes(unsafe { CStr::from_ptr(name) }.to_bytes());
    code(super::block_on(fs.rmdir_child(parent, name)))
}

/// No flags parameter, because libfuse-t's `rename` has none — so
/// `RENAME_NOREPLACE`/`RENAME_EXCHANGE` never reach this binding and there is
/// nothing here to refuse. The two bindings that do receive them answer EINVAL.
unsafe extern "C" fn rename<T: Mountable>(
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

unsafe extern "C" fn readdir<T: Mountable>(
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
            // An interior NUL cannot be handed to C, and cannot have come from a
            // well-behaved backend either.
            let name = CString::new(child.name.as_bytes()).map_err(|_| CortexError::InvalidName)?;
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

unsafe extern "C" fn forget<T: Mountable>(fs: *mut c_void, inode: u64, nlookup: u64) {
    let fs = unsafe { recover::<T>(fs) };
    fs.forget_inode(inode, nlookup);
}

// Tests live beside this file rather than inside it, matching the crate's other
// modules. They are still a child module, so the callbacks and the `Ops` table —
// all private — stay reachable.
#[cfg(test)]
#[path = "fuse_t_tests.rs"]
mod tests;

/// The vtable for one concrete backend, with every callback monomorphised for it.
fn ops_for<T: Mountable>() -> Ops {
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

/// The session pointer, sent to the serving thread.
///
/// # Safety
/// Touched only by the serving thread and by [`FuseTMount::teardown`], which runs
/// after that thread is joined — never from two threads at once.
struct SessionPtr(*mut c_void);
unsafe impl Send for SessionPtr {}

impl SessionPtr {
    /// Reached through a method, not the field, so a `move` closure captures the
    /// whole wrapper. Since Rust 2021 a closure captures only the fields it names,
    /// and naming `.0` directly would capture the bare `*mut c_void` — which is
    /// not `Send`, defeating the wrapper entirely.
    fn get(&self) -> *mut c_void {
        self.0
    }
}

/// A live FUSE-T mount, unmounted when this guard is dropped.
///
/// The FUSE-T counterpart of `CortexMount` (`adapter/fuse.rs`), and the one that
/// needs no kernel extension.
pub struct FuseTMount {
    session: *mut c_void,
    thread: Option<JoinHandle<c_int>>,
    mountpoint: PathBuf,
    /// Keeps the `PosixFs<T>` alive: the shim holds a raw pointer into it that
    /// the serving thread dereferences on every request. Erased to `Any` only so
    /// this type needs no parameter of its own; never downcast.
    _fs: Box<dyn Any + Send + Sync>,
}

impl FuseTMount {
    /// Mount `volume` at `mountpoint` and serve it from a background thread.
    ///
    /// `mountpoint` must already exist. Requires FUSE-T
    /// (`brew install --cask fuse-t`) — no kernel extension, no reboot.
    pub fn spawn<T>(volume: T, mountpoint: impl AsRef<Path>) -> Result<Self>
    where
        T: Mountable + 'static,
        T::Handle: 'static,
    {
        Self::spawn_named(volume, mountpoint, "cortex")
    }

    /// [`spawn`](Self::spawn) with the name the mount reports as its source.
    ///
    /// Private: nothing has wanted a name other than `cortex` yet, and the fuser
    /// binding's `spawn_with` is the one that has a caller. Widen it when something
    /// needs it rather than carrying the surface on the chance.
    fn spawn_named<T>(volume: T, mountpoint: impl AsRef<Path>, fsname: &str) -> Result<Self>
    where
        T: Mountable + 'static,
        T::Handle: 'static,
    {
        let mountpoint = mountpoint.as_ref().to_path_buf();
        let c_mountpoint = CString::new(mountpoint.as_os_str().as_bytes())
            .map_err(|_| CortexError::InvalidName)?;
        let c_fsname = CString::new(fsname).map_err(|_| CortexError::InvalidName)?;

        // Boxed and never moved again, so the pointer the shim keeps stays valid
        // for as long as this guard lives.
        let fs: Box<PosixFs<T>> = Box::new(PosixFs::new(volume));
        let fs_ptr = &*fs as *const PosixFs<T> as *mut c_void;
        let ops = ops_for::<T>();

        let session =
            unsafe { cortex_fuse_t_mount(c_mountpoint.as_ptr(), c_fsname.as_ptr(), fs_ptr, &ops) };
        if session.is_null() {
            return Err(CortexError::Io(io::Error::other(format!(
                "FUSE-T could not mount {}: is fuse-t installed, and does the \
                 mount point exist and is it empty?",
                mountpoint.display()
            ))));
        }

        let sendable = SessionPtr(session);
        let thread = std::thread::Builder::new()
            .name("cortex-fuse-t".into())
            .spawn(move || unsafe { cortex_fuse_t_loop(sendable.get()) })
            .map_err(|err| {
                // Nothing is serving yet, so tear the mount back down rather than
                // leaving it wedged.
                unsafe { cortex_fuse_t_destroy(session) };
                CortexError::Io(err)
            })?;

        let mut mount = FuseTMount {
            session,
            thread: Some(thread),
            mountpoint,
            _fs: fs,
        };

        // `fuse_mount` returning is not the mount being usable: FUSE-T completes
        // it only once the session has answered its helper's opening
        // INIT/STATFS/GETATTR, which needs the thread above running. Touching the
        // path in that window either sees the bare directory underneath or blocks
        // on a half-built NFS mount, so wait here rather than make callers sleep.
        if !mount.wait_until_mounted(MOUNT_TIMEOUT) {
            mount.teardown();
            return Err(CortexError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "FUSE-T did not finish mounting {} within {MOUNT_TIMEOUT:?}",
                    mount.mountpoint.display()
                ),
            )));
        }
        Ok(mount)
    }

    /// Poll until `mountpoint` is a mount point, or give up.
    ///
    /// Decided the unix way — a directory whose device id differs from its
    /// parent's has something mounted on it — which holds whatever the mount is.
    /// That matters here: FUSE-T's is an NFS mount, not a FUSE one.
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

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Unmount, join the serving thread, and release the session. [`Drop`] does
    /// the same but cannot report failure.
    pub fn unmount(mut self) -> Result<()> {
        self.teardown();
        Ok(())
    }

    /// Unmount → join → destroy, in that order and at most once. Unmounting is
    /// what makes the blocking loop return, so joining first would hang;
    /// destroying first would free the session under the thread still reading it.
    fn teardown(&mut self) {
        if self.session.is_null() {
            return;
        }
        unsafe { cortex_fuse_t_unmount(self.session) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        unsafe { cortex_fuse_t_destroy(self.session) };
        self.session = std::ptr::null_mut();
    }
}

impl Drop for FuseTMount {
    fn drop(&mut self) {
        // Never panics: a `Drop` that panics mid-unwind aborts the process, which
        // in a failing test replaces the real assertion with a bare abort.
        self.teardown();
    }
}
