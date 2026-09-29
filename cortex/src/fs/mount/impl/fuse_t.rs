//! Binds a [`Posix`] to FUSE-T through libfuse-t's lowlevel API. [`FuseTMount`] is the whole of
//! what it exports.
//!
//! Everything it needs — inode identity, handle lifetime, the readdir cursor, the open
//! decomposition, the attribute policy, the host errno table — already lives in
//! [`posix`](crate::fs::filesystem::posix), so what is left here is marshalling.
//!
//! **Why a binding of its own, beside the `fuser` one?** They differ in who drives the
//! session. `fuser` takes the fd from `fuse_mount` and speaks the kernel FUSE protocol over
//! it itself, which works when that fd is macFUSE's real device. FUSE-T's is a socket to its
//! own helper, which expects to be driven *by* libfuse-t's loop: hand it to a foreign
//! protocol reader and the INIT handshake completes, two probes arrive, then the helper hangs
//! up with no mount appearing. In exchange FUSE-T needs no kernel extension, where macFUSE is
//! a kext needing reduced-security boot on Apple Silicon.
//!
//! **Which mount the kernel ends up with is a choice**, and one this file barely sees. The
//! helper carries three transports — an NFSv4 server, an SMB server, and an FSKit module
//! whose extension ships signed inside `fuse-t.app` — and picking one is a mount option (see
//! [`FuseTBackend`]). Everything below is the same either way: the vtable, the callbacks, and
//! what the guest is told are decided here, and what carries them is decided there.
//!
//! **Why a C shim?** `fuse_lowlevel_ops` has ~50 function pointers with
//! `__APPLE__`-conditional members, `fuse_file_info` has bitfields, and
//! `fuse_entry_param` embeds a host `struct stat`. A wrong layout in Rust is silent memory
//! corruption, not a compile error, so `contrib/fuse_t/shim.c` owns all of them and
//! exposes a flat vtable of our own design instead.

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
    sigchld::Sigchld,
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

/// Mirror of `struct cortex_stat` in `contrib/fuse_t/shim.h`. All fixed-width with an
/// explicit pad, so keeping the two in step is a matter of reading rather than of trusting
/// alignment rules.
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
/// Implemented on the C side, which owns `fuse_add_direntry`'s accounting.
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
/// outlive the session — [`FuseTMount`] boxes it and keeps it until after the loop returns.
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

// Each callback is generic in `T` and monomorphised per store by `ops_for`, so the
// filesystem type stays static — no trait object, no downcast.

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

/// The `fh` the shim passes is dropped. A resize names a path either way, and the inode
/// arriving beside it is already that path — there is no per-handle state a store keeps
/// that a file handle could reach instead.
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

/// No flags parameter, because libfuse-t's `rename` has none — so
/// `RENAME_NOREPLACE`/`RENAME_EXCHANGE` never reach this binding and there is nothing here
/// to refuse.
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
            // An interior NUL cannot be handed to C, and cannot have come from a
            // well-behaved store either.
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

/// What the mount reports as its source — the name `df` and Finder show.
///
/// Fixed, because nothing has wanted another one, and a `CStr` literal so there is neither
/// an allocation nor an invalid-name case to answer for.
const FSNAME: &CStr = c"cortex";

/// How long to wait for FUSE-T to finish mounting. Generous: the helper has to start,
/// negotiate, and get the kernel to complete a mount.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a drop gives the mount table to catch up with a forceful unmount that has
/// already been accepted. Short: this is bookkeeping settling, not work being done.
const UNMOUNT_SETTLE: Duration = Duration::from_secs(3);

/// How long a drop waits for the serving thread to notice that its channel is gone.
///
/// Generous, because overrunning it costs a leaked session and a leaked thread — but
/// bounded, because the alternative is a destructor that never returns. A loop that
/// has not come back in this long is not going to.
const LOOP_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Which of FUSE-T's transports serves the mount.
///
/// The helper carries all three and the choice is one mount option, so nothing on this side
/// changes with it: the same vtable answers the same requests, and what differs is what the
/// kernel thinks it is talking to.
///
/// Not passed at all by [`FuseTMount::try_new`], which leaves the choice to FUSE-T's own
/// `fuse-t.ini`. Naming one here overrides that, so a caller should only do it when the
/// difference is the point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuseTBackend {
    /// An NFSv4 server in FUSE-T's helper, which the kernel mounts as an NFS client. The
    /// original design, and what `fuse-t.ini` defaults to.
    Nfs,

    /// FUSE-T's FSKit module — macOS 15's file-system extension framework, with the extension
    /// shipped and signed inside `fuse-t.app` rather than built here.
    ///
    /// The reason this is a mount option and not a binding of its own: implementing FSKit
    /// directly would mean a Swift app extension, its entitlement, and a filesystem living in
    /// a process the system launches rather than in the one that mounted. FUSE-T's helper has
    /// already paid all of that, and bridges its own RPC to the extension.
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
/// Touched only by the serving thread and by [`FuseTMount`]'s `Drop`, which runs after that thread
/// is joined — never from two threads at once.
struct SessionPtr(*mut c_void);
unsafe impl Send for SessionPtr {}

impl SessionPtr {
    /// Reached through a method, not the field, so a `move` closure captures the whole
    /// wrapper. Since Rust 2021 a closure captures only the fields it names, and naming `.0`
    /// directly would capture the bare `*mut c_void` — which is not `Send`, defeating the
    /// wrapper entirely.
    fn get(&self) -> *mut c_void {
        self.0
    }
}

/// A live FUSE-T mount: constructing one mounts, dropping it unmounts.
///
/// The whole call surface of this binding, and a guard rather than a handle — there is no way
/// to hold a mounted filesystem without holding the thing that takes it down, and no way to
/// reach the inode numbers or the open files behind it, which stay the binding's own.
///
/// **Not** generic in the store, though [`try_new`](Self::try_new) is. The store's type is
/// what makes the vtable typed — `ops_for::<T>` monomorphises every callback so `recover` can
/// reach a `Posix<T>` with no downcast — and that is all decided while the mount is being
/// made. Afterwards nothing here needs the type back, so carrying it would only spread a
/// parameter onto everything that holds a mount.
pub struct FuseTMount {
    session: *mut c_void,

    /// `None` once [`join`](Self::join) has taken it, which is how `Drop` knows not to wait
    /// for a thread that has already been collected.
    thread: Option<JoinHandle<c_int>>,

    mountpoint: PathBuf,

    /// The filesystem, owned and never looked at again: the shim keeps a raw pointer into it
    /// and the serving thread dereferences that on every request, so something has to keep it
    /// alive for the life of the mount.
    ///
    /// A struct's fields go after its own `drop` body, so on the ordinary path this is freed
    /// once `destroy` has returned and no request can reach it. `Drop` takes it out of the
    /// `Option` and forgets it on the one path where the serving thread outlived its
    /// deadline: the loop still holds the shim's pointer into this, and freeing it there is
    /// the same use-after-free the leaked session avoids.
    ///
    /// `dyn Send + Sync` rather than `dyn Any`: the two erase equally well, and this one does
    /// not also advertise a downcast nothing should ever perform.
    _fs: Option<Box<dyn Send + Sync>>,

    /// This process's ownership of the mount point — what an opt-in
    /// [`unmount_on_signal`](crate::fs::unmount_on_signal) reads now, and what a later run's
    /// [`reclaim_abandoned`](crate::fs::reclaim_abandoned) reads if this one is killed. Held
    /// rather than read: dropping it is what gives the mount point up.
    _claim: Claim,
}

impl FuseTMount {
    /// Mount `fs` at `mountpoint`, serve it from a background thread, and return once the
    /// mount is real.
    ///
    /// `mountpoint` must already exist and be empty. Requires FUSE-T
    /// (`brew install --cask fuse-t`) — no kernel extension, no reboot.
    ///
    /// **Blocks until the mount is real**, which is not the same as until it is made: FUSE-T's
    /// `fuse_mount` hands back a session as soon as it has one, but the kernel finishes
    /// attaching it only once the thread below has answered the helper's opening
    /// INIT/STATFS/GETATTR. A caller that touched the path in that window would see the bare
    /// directory underneath, or block on a half-built mount — so the waiting happens here,
    /// once, instead of in every caller.
    ///
    /// Which transport serves it is FUSE-T's own choice, from `fuse-t.ini`. Use
    /// [`try_new_with`](Self::try_new_with) to name one.
    ///
    /// `'static`, because the store is served from that thread for as long as the mount
    /// lives.
    pub fn try_new<T: FileSystem + 'static>(fs: T, mountpoint: &Path) -> io::Result<Self> {
        Self::mount(fs, mountpoint, std::ptr::null())
    }

    /// [`try_new`](Self::try_new), with the transport named rather than configured.
    ///
    /// The only thing this changes is what the kernel is talking to — see [`FuseTBackend`].
    pub fn try_new_with<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        backend: FuseTBackend,
    ) -> io::Result<Self> {
        Self::mount(fs, mountpoint, backend.option().as_ptr())
    }

    /// The two above, differing only in whether they name a backend. `backend` is a C string
    /// or null, which is what the shim reads as "leave it to FUSE-T".
    fn mount<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        backend: *const c_char,
    ) -> io::Result<Self> {
        // What a `SIGKILL`ed run left behind is nobody's but the next run's, and this
        // is the next run. Only mounts whose owning process is gone are touched, so a
        // sibling instance keeps its own.
        reclaim_abandoned();

        let c_mountpoint = CString::new(mountpoint.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidFilename))?;

        // Boxed and never moved again, so the pointer the shim keeps stays valid for as long
        // as the mount lives.
        let fs: Box<Posix<T>> = Box::new(Posix::new(fs));
        let fs_ptr = &*fs as *const Posix<T> as *mut c_void;
        let ops = ops_for::<T>();

        // Scoped to the call and no wider: libfuse-t resets the process's SIGCHLD
        // handling on its way to forking the mount helper, and what that costs is
        // paid by whatever else in the program waits on a child. See `sigchld`.
        let session = {
            let _sigchld = Sigchld::held();
            unsafe {
                cortex_fuse_t_mount(
                    c_mountpoint.as_ptr(),
                    FSNAME.as_ptr(),
                    backend,
                    fs_ptr,
                    &ops,
                )
            }
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
                // The one failure no guard can clean up after, because it is what keeps the
                // guard from existing: nothing is serving, so tear the mount back down here
                // rather than leave it wedged.
                unsafe { cortex_fuse_t_destroy(session) };
            })?;

        // From here on the guard exists, so every way out of this function unmounts by
        // dropping it — including the `Err` below, which needs no cleanup of its own.
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
    /// For a program whose whole job is the mount: `try_new` puts it up without blocking, and
    /// this waits for something else to end it — `umount`, `diskutil unmount`, or a helper
    /// that dies. **It does not unmount**, so a caller with other work to do drops the guard
    /// instead of joining it.
    ///
    /// The thread is taken out here, so the `Drop` that follows skips the join it has already
    /// done and goes straight to releasing the session.
    ///
    /// `Err` is the serving loop ending badly, or its thread having panicked — which cannot
    /// be told apart from a normal exit any other way, since a panic in an `extern "C"`
    /// callback aborts and never reaches this.
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
    /// Decided the unix way — a directory whose device id differs from its parent's has
    /// something mounted on it — which holds whatever the mount is. That matters here: what a
    /// FUSE-T mount *is* depends on the backend serving it, and none of the three is a FUSE
    /// mount, so a test that looked for one would answer differently per transport.
    ///
    /// Sleeps between polls, so this holds whichever thread mounted for as long as FUSE-T
    /// takes — a few polls in practice, and the timeout only when something is wrong. A caller
    /// that cannot give up a thread for that mounts from one it can spare.
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

/// Nothing to arrange: the guard already is the mount, and already knows where it is.
impl Mount for FuseTMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

/// The session pointer is what keeps these from being derived, and it is not what makes the
/// guard shareable or movable.
///
/// # Safety
/// Nothing reachable through `&FuseTMount` touches the pointer: the trait above reads
/// `mountpoint` and there is no other method on a shared borrow, so no amount of sharing can
/// produce a second caller into libfuse-t. The pointer is used by exactly two things — the
/// serving thread, which was handed it at `mount` and holds it alone, and `Drop`, which has
/// `&mut self` and so runs after every borrow is gone and joins that thread before releasing
/// the session.
///
/// Which thread that `Drop` runs on does not matter, and already does not: the loop is spawned
/// onto a thread of its own while the caller keeps the guard, so unmount and destroy were
/// never called from the thread doing the serving.
///
/// [`Mount`] requires both, and a mount that could not be shared or sent would be one a task
/// could not hold — which is what a mount is *for* here.
unsafe impl Send for FuseTMount {}
unsafe impl Sync for FuseTMount {}

impl Drop for FuseTMount {
    /// Unmount → stop the loop → join → unmount again if it was busy → release the session,
    /// in that order and at most once.
    ///
    /// The unmount is attempted twice on purpose. The first is while the mount is still being
    /// served, so anything the kernel has cached can be written back and a plain `umount`
    /// refuses rather than pulls the tree out from under a reader. The second is after the
    /// loop has stopped, which is what makes a reader let go — a mount with traffic on it
    /// answers the first attempt with `EBUSY` and the second without complaint.
    ///
    /// # Why the unmount does not go through libfuse-t
    ///
    /// `fuse_unmount` cannot be called while a second mount is alive in this process.
    /// libfuse-t keeps the FUSE-T helper's pid in one process-global slot (`_cpid`, written by
    /// `fuse_mount_core` after its `fork`) and `fuse_kern_unmount` ends in a *blocking*
    /// `waitpid` on whatever is in it. Every mount overwrites the slot, so an unmount waits on
    /// whichever session mounted last — a helper that is still serving and will not exit until
    /// its own mount goes. With two mounts up, `drop(a); drop(b)` therefore never reaches the
    /// second line: both teardowns sit in `wait4` while the mounts stay in the table.
    /// `_mount_wait_thread`, joined a few instructions later, is a single global in the same
    /// way. The whole path is written for one mount per process, and cortex mounts one per
    /// session.
    ///
    /// So the mount comes down the way a mount whose owner is *gone* has to come down anyway
    /// — `umount`, in a child process, bounded. That is one mechanism for both a guard's own
    /// teardown and [`reclaim_abandoned`](crate::fs::reclaim_abandoned), and it depends on
    /// nothing libfuse-t keeps in a global.
    ///
    /// What is left for the shim is releasing the session that was serving it, which is
    /// per-session and safe: end the loop, join its thread, free.
    ///
    /// The contract is unchanged — the mount is taken down here and there is still no
    /// `unmount` to call. What this cannot promise is that the kernel agreed: every step is
    /// bounded, so a mount that refuses both rungs of the ladder with nothing serving it
    /// leaves this returning anyway, having said so on stderr. The alternative is a
    /// destructor that never returns, which is the bug above wearing different clothes.
    ///
    /// Nothing here panics. A `Drop` that panics mid-unwind aborts the process, which in a
    /// failing test replaces the real assertion with a bare abort.
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        let session = std::mem::replace(&mut self.session, std::ptr::null_mut());
        let mountpoint = resolved(&self.mountpoint);

        // First while the mount is still being served, so an unmount that has to flush a
        // cached write still has something to flush it to.
        let was_busy = !unmount_under(&mountpoint);

        // Then stop serving, whether or not that worked. It is what releases anything still
        // reading through the mount, and a caller that has dropped the guard has already
        // said the mount is over.
        unsafe { cortex_fuse_t_stop(session) };

        // Joined before the session is freed, because the loop reads it — and joined with a
        // deadline, because a thread that did not notice the shutdown must not become a hang
        // in a destructor. The session is then deliberately leaked: freeing it under a live
        // loop is a use-after-free, and a leaked allocation is the cheaper of the two.
        let collected = match self.thread.take() {
            Some(thread) if thread_ends(&thread, LOOP_EXIT_TIMEOUT) => {
                let _ = thread.join();
                true
            }
            Some(_) => false,
            // Already collected by `join`, so there was nothing to wait for.
            None => true,
        };

        // A mount that was busy a moment ago is not busy now that nothing is being served
        // through it, so the rungs that refused the first time are worth one more try.
        let left = if was_busy {
            unmount_under(&mountpoint);
            // Polled rather than read once: a forceful unmount is *accepted* before the
            // table catches up, so a single read here reports a mount that is already on
            // its way out. That report would be a false alarm on every busy teardown.
            settle(&mountpoint, UNMOUNT_SETTLE)
        } else {
            Vec::new()
        };

        // Reported rather than propagated, and never a panic: what is left is left for
        // someone to clear by hand, so saying so is the least this can do.
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
            // Leaked with it: the loop dereferences the shim's pointer into the filesystem on
            // every request, and it is still running.
            std::mem::forget(self._fs.take());
            return;
        }
        unsafe { cortex_fuse_t_destroy(session) };
    }
}

/// Wait for the mount table to stop naming anything at `mountpoint`, and report whatever it
/// still names when the time is up.
///
/// Empty is the good answer. Anything else is a mount that has had both rungs of the ladder
/// aimed at it, with nothing serving it, and is still there.
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
/// Polled, because `JoinHandle::join` has no timed form and the whole point here is to not
/// wait forever on one.
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
