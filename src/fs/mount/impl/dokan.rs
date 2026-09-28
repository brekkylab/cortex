//! Binds a [`FileSystem`] to Dokany's user-mode filesystem API. [`DokanMount`] is the whole of
//! what it exports.
//!
//! **This is the one binding that does not go through [`Posix`](crate::fs::Posix)**, and the
//! reason is in that module's own doc: `Posix` exists because a kernel addresses files by
//! number and by descriptor, and a path-addressed consumer reaches [`FileSystem`] directly.
//! Dokan is that consumer. Every callback below carries the full path from the volume root —
//! `\`, `\src\main.rs` — because the NT I/O manager resolves names itself and hands the
//! filesystem the result. There is no lookup, no inode, and nothing for an inode table to
//! answer: mapping those paths onto numbers and back would be a round trip per callback bought
//! for a vocabulary this interface never speaks.
//!
//! What it still shares is the part that is about the *store* rather than the interface —
//! [`attr_for`] for the attribute policy and the timestamp fallbacks, and the synthetic
//! capacity constants — so the numbers a Windows reader sees are derived the same way a FUSE
//! reader's are. What it decides for itself is the same pair every binding decides: the error
//! numbering its consumer expects ([`nt_status`], where the others have `host_errno`) and the
//! concrete attribute type it must fill ([`FileInfo`]).
//!
//! # What Windows does differently, and what this file does about it
//!
//! * **One entry point.** [`create_file`](FileSystemHandler::create_file) is open, create,
//!   `mkdir` and "open the directory to list it", separated by an NT `create_disposition` and
//!   `create_options`. It decomposes onto the store the way `Posix::realize_open` decomposes
//!   `O_CREAT`/`O_EXCL`/`O_TRUNC`, and for the same reason: exclusivity is decided by the
//!   store's own [`create`](FileSystem::create) and nothing else.
//! * **Deletion is a disposition, not a call.** `delete_file`/`delete_directory` only answer
//!   *may this be deleted*, and the removal happens in [`cleanup`] once the last handle is
//!   gone. That is the whole of why there is no silly-rename here: POSIX needs one because a
//!   descriptor must outlive its name, and Windows arranges for the name to outlive the
//!   descriptor instead. The store never sees a file vanish from under an open handle.
//! * **Access is the kernel's to enforce.** `Posix` re-checks a read against the mode its open
//!   recorded; this does not. The NT kernel has already checked `desired_access` against the
//!   handle before the IRP is built, and paging I/O — a memory-mapped file's reads — arrives on
//!   handles whose access says nothing useful, so a second check here would reject work the
//!   kernel had already allowed.
//!
//! # Runtime, not build time
//!
//! Dokany's kernel driver (`dokan2.sys`) is installed on the host by its own installer; what
//! this links is the user-mode DLL beside it. `dokan-sys` links the *installed* library when
//! `DokanLibrary2_LibraryPath_x64` is set and otherwise builds one from its own vendored
//! sources — which compiles, but produces a DLL whose version need not match the installed
//! driver, and a mismatch surfaces as [`FileSystemMountError::Version`] at mount time. So set
//! the variable (the Dokany installer does) rather than relying on the fallback.
//!
//! **Hard to test in isolation, and structurally so**, exactly as the FUSE bindings are: the
//! callbacks answer through objects only Dokan constructs, and reaching them means a real
//! mount against a real driver. What is unit-tested below is what does not need one — the name
//! translation and the status table.

use std::{
    collections::hash_map::DefaultHasher,
    ffi::OsString,
    hash::{Hash, Hasher},
    io,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::Duration,
};

use ::dokan::{
    CreateFileInfo, DiskSpaceInfo, FileInfo, FileSystemHandler, FileSystemMounter,
    FileTimeOperation, FillDataError, FillDataResult, FindData, IO_SECURITY_CONTEXT, MountFlags,
    MountOptions, OperationInfo, OperationResult, VolumeInfo, map_win32_error_to_ntstatus,
};
use dokan_sys::win32::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OVERWRITE, FILE_OVERWRITE_IF, FILE_SUPERSEDE,
};
use widestring::{U16CStr, U16CString};
use winapi::{
    shared::{
        ntdef::NTSTATUS,
        ntstatus::{
            STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL,
            STATUS_FILE_IS_A_DIRECTORY, STATUS_FILE_TOO_LARGE, STATUS_INVALID_PARAMETER,
            STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY, STATUS_NOT_IMPLEMENTED,
            STATUS_NOT_SAME_DEVICE, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
            STATUS_OBJECT_NAME_NOT_FOUND, STATUS_UNEXPECTED_IO_ERROR, STATUS_UNSUCCESSFUL,
        },
    },
    um::winnt::{
        ACCESS_MASK, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_CASE_PRESERVED_NAMES,
        FILE_CASE_SENSITIVE_SEARCH, FILE_UNICODE_ON_DISK,
    },
};

use crate::fs::{
    DirentKind, FileSystem, Mount,
    filesystem::posix::{BLOCK_SIZE, NAME_MAX, TOTAL_BLOCKS, attr_for},
};

/// What the volume calls itself — the label Explorer shows beside the drive letter.
const VOLUME_NAME: &str = "cortex";

/// What the volume reports as its *format*.
///
/// `NTFS` rather than anything descriptive, and not a cosmetic choice: UAC refuses to elevate
/// a process whose image sits on a volume whose filesystem it does not recognise, so a custom
/// name here breaks running an installer off the mount. Dokany's own sample carries the same
/// value for the same reason.
const FS_NAME: &str = "NTFS";

/// How long the driver waits for one of these callbacks before it gives up on the operation.
///
/// Generous, because a callback here may be an object store answering across a network, and
/// the driver's own default is tuned for a filesystem backed by a local disk. A store slower
/// than this does not fail the operation cleanly — the driver abandons it and the caller sees
/// an I/O error — so the number is the ceiling on how slow a backend may be, not a hint.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long [`DokanMount::try_new`] waits for the driver to report the mount is live.
///
/// There is no failure this covers that is also *reported* — a mount that will not come up
/// usually fails `mount()` outright — so this is the backstop for a driver that accepts the
/// filesystem and then never calls [`mounted`](FileSystemHandler::mounted), which would
/// otherwise hang the caller with no diagnosis.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);

/// A live Dokan mount: constructing one mounts, dropping it unmounts.
///
/// The whole call surface of this binding, and the Windows counterpart of
/// [`FuseMount`](super::FuseMount) — same shape, a different interface and a different kernel
/// underneath.
///
/// **No [`Claim`](super::super::claim::Claim), unlike the FUSE guards**, and nothing to
/// reclaim on the next run. Dokany's driver owns the volume and tears it down when the process
/// that registered it dies, however it dies — so the mount point a killed run leaves behind is
/// cleared by the driver rather than by whoever comes next. That is why
/// [`reclaim_abandoned`](crate::fs::reclaim_abandoned) is `#[cfg(unix)]` and stays that way:
/// there is no Windows half of it missing.
pub struct DokanMount {
    mountpoint: PathBuf,

    /// The mount point as the driver names it. Kept because `Drop` unmounts by *name* —
    /// `DokanRemoveMountPoint` is the only way in, since the handle that would close the
    /// filesystem belongs to the serving thread, which is blocked inside it.
    wide: U16CString,

    /// The thread the filesystem is served from. `None` once [`join`](Self::join) or `Drop`
    /// has taken it, so whichever runs second does nothing.
    serving: Option<JoinHandle<()>>,
}

impl DokanMount {
    /// Mount `fs` at `mountpoint` and serve it from a background thread.
    ///
    /// `mountpoint` is a drive letter (`Z:\`) or an existing empty directory on an NTFS
    /// volume. Needs Dokany installed — see the module docs.
    ///
    /// Returns once the driver has reported the mount live, so the path is openable by the
    /// time a caller has the guard. (Like the FUSE-T binding and unlike the `fuser` one, that
    /// needs waiting for: `mount()` returns as soon as the filesystem is registered, which is
    /// before the volume exists.)
    ///
    /// `'static`, because the store is served from that thread for as long as the mount lives.
    pub fn try_new<T: FileSystem + 'static>(fs: T, mountpoint: &Path) -> io::Result<Self> {
        Self::try_new_with(fs, mountpoint, MountFlags::empty())
    }

    /// [`try_new`](Self::try_new) with the mount flags spelled out.
    ///
    /// [`MountFlags::WRITE_PROTECT`] is the one worth reaching for, and is the counterpart of
    /// `MountOption::RO`: it makes the *driver* reject writes before they reach a store, which
    /// is stronger than every store answering `ReadOnlyFilesystem` by hand — and it covers the
    /// ones that would have answered `Ok`.
    ///
    /// [`MountFlags::CURRENT_SESSION`] is the other one worth knowing: a drive letter belongs
    /// to a logon session, so a mount made by a service is not visible to the desktop unless
    /// the mount manager publishes it.
    pub fn try_new_with<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        flags: MountFlags,
    ) -> io::Result<Self> {
        // First, before the serving thread's `library()` makes the first call into
        // `dokan2.dll`: delay-loaded, a DLL that is not there fails that call with an SEH
        // exception, which no `Result` catches.
        crate::fs::mount_support()?;

        let wide = U16CString::from_os_str(mountpoint).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidFilename,
                "mount point contains an interior nul",
            )
        })?;

        // One slot: the thread reports readiness exactly once, and a send must not block a
        // serving thread that nobody is listening to any more.
        let (ready, mounted) = mpsc::sync_channel(1);
        let serving = {
            let wide = wide.clone();
            std::thread::Builder::new()
                .name("cortex-dokan".into())
                .spawn(move || serve(fs, wide, flags, ready))?
        };

        match mounted.recv_timeout(MOUNT_TIMEOUT) {
            Ok(Ok(())) => Ok(DokanMount {
                mountpoint: mountpoint.to_path_buf(),
                wide,
                serving: Some(serving),
            }),
            // The thread reported a failure and is on its way out; it never mounted, so there
            // is nothing to take down.
            Ok(Err(err)) => {
                let _ = serving.join();
                Err(err)
            }
            // Dropped without reporting, which means the thread unwound. `join` gives the
            // panic, not that we can do much with it.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = serving.join();
                Err(io::Error::other("cortex dokan serving thread ended"))
            }
            // Accepted and then silent. The filesystem may yet be registered, so take it down
            // by name rather than leaving a volume nobody holds a guard for.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = ::dokan::unmount(&wide);
                let _ = serving.join();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "dokan did not report the mount live",
                ))
            }
        }
    }

    /// Serve until the mount goes away, then take it down.
    ///
    /// For a program whose whole job is the mount: `try_new` puts it up without blocking, and
    /// this waits for something else to end it — `dokanctl /u`, an eject from Explorer, or the
    /// driver tearing the volume down. **It does not unmount**, so a caller with other work to
    /// do drops the guard instead of joining it.
    ///
    /// The thread is taken out here, so the `Drop` that follows has nothing left to do.
    ///
    /// `Err` is the serving thread having panicked.
    pub fn join(mut self) -> io::Result<()> {
        match self.serving.take() {
            Some(serving) => serving
                .join()
                .map_err(|_| io::Error::other("cortex dokan serving thread panicked")),
            None => Ok(()),
        }
    }
}

/// Nothing to arrange: the guard already is the mount, and already knows where it is.
impl Mount for DokanMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// `file:///Z:/...`, which is a Windows path's spelling as a URL and not what the default
    /// would produce.
    ///
    /// [`Mount::url`]'s default is `file://` plus the path as it stands, which is right for a
    /// POSIX path — it already starts with the `/` that separates an empty authority from an
    /// absolute path. A Windows path does not, so the default would emit `file://Z:\x`, whose
    /// authority is `Z:` and whose path is a backslash-run no reader will take apart.
    fn url(&self) -> Option<String> {
        let path = self
            .mountpoint
            .to_str()
            .filter(|_| self.mountpoint.is_absolute())?;
        Some(format!("file:///{}", path.replace('\\', "/")))
    }
}

impl Drop for DokanMount {
    /// Ask the driver to remove the mount point, then wait for the serving thread to notice,
    /// then put the directory back the way it was found.
    ///
    /// The order is forced: the thread is blocked in Dokan's own wait-for-closed, so it cannot
    /// end until the volume is gone, and joining first would deadlock. `unmount` is what ends
    /// both.
    ///
    /// Nothing here panics. A `Drop` that panics mid-unwind aborts the process, which in a
    /// failing test replaces the real assertion with a bare abort — so a refused unmount is
    /// reported and the thread is left running rather than joined into a hang.
    fn drop(&mut self) {
        let Some(serving) = self.serving.take() else {
            return;
        };
        if !::dokan::unmount(&self.wide) {
            eprintln!(
                "cortex: unmounting {} failed; the volume is left for dokanctl to clear",
                self.mountpoint.display()
            );
            return;
        }
        let _ = serving.join();
        self.reclaim_mountpoint();
    }
}

impl DokanMount {
    /// Put the mount point back to the empty directory it was before the mount.
    ///
    /// **`unmount` takes the volume down and leaves the mount point standing.** What a caller
    /// handed over was an empty directory; what is there afterwards is a reparse point onto a
    /// volume that no longer exists — invisible to a listing of its parent, impossible to
    /// open, and refused with `ERROR_ALREADY_EXISTS` by the next `create_dir_all`. So the
    /// second mount of the same path fails on a path the first one was supposed to have
    /// released, which makes a mount a once-per-directory thing rather than a guard.
    ///
    /// `symlink_metadata` and not `exists`: the latter follows the reparse point, finds the
    /// volume gone and answers that nothing is there. `remove_dir` does not follow it either,
    /// so what it removes is the junction rather than anything it points at.
    ///
    /// Failures are ignored rather than reported. This runs from `Drop`, the mount is already
    /// down, and what is left if it does not work is the state that was there before — which
    /// the next mount will report on its own.
    fn reclaim_mountpoint(&self) {
        use std::os::windows::fs::MetadataExt as _;

        /// `FILE_ATTRIBUTE_REPARSE_POINT`, spelled here rather than taken from `winapi`: it is
        /// one number and the crate is otherwise only needed for the NTSTATUS surface.
        const REPARSE_POINT: u32 = 0x0000_0400;

        let Ok(meta) = std::fs::symlink_metadata(&self.mountpoint) else {
            return;
        };
        if meta.file_attributes() & REPARSE_POINT == 0 {
            return;
        }
        if std::fs::remove_dir(&self.mountpoint).is_ok() {
            let _ = std::fs::create_dir(&self.mountpoint);
        }
    }
}

/// Initialise the Dokan library, once per process.
///
/// Paired with `DokanShutdown`, which is **never called** — the same trade as the runtime in
/// [`block_on`](super::block_on), and for a stronger reason: shutting the library down while
/// any mount is still up is undefined, and this crate hands mounts out to callers who decide
/// their own lifetimes, so there is no moment it could be called safely. What it would release
/// is released by process exit anyway.
fn library() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(::dokan::init);
}

/// The body of the serving thread: build the handler, mount, and stay until it is unmounted.
///
/// Everything Dokan borrows lives here as a local — the mounter borrows the handler, the mount
/// point and the options for as long as the filesystem exists, which is not a shape a guard
/// struct can hold without being self-referential. So the thread owns them, and the guard
/// holds only the name it can unmount by.
fn serve<T: FileSystem>(
    fs: T,
    wide: U16CString,
    flags: MountFlags,
    ready: SyncSender<io::Result<()>>,
) {
    library();

    let handler = Handler {
        store: fs,
        ready: Mutex::new(Some(ready)),
    };
    let options = MountOptions {
        flags,
        timeout: OPERATION_TIMEOUT,
        // The same block the free-space reply is denominated in, so the two agree about what a
        // block is. Left at the library's default (`0`) they would not have to.
        allocation_unit_size: BLOCK_SIZE as u32,
        sector_size: BLOCK_SIZE as u32,
        ..Default::default()
    };

    let mut mounter = FileSystemMounter::new(&handler, &wide, &options);
    match mounter.mount() {
        // Dropping the filesystem blocks until the volume is gone, which is what keeps this
        // thread — and everything the mounter borrowed — alive for the length of the mount.
        // `mounted` has already reported readiness by the time anything can be served.
        Ok(fs) => drop(fs),
        Err(err) => handler.report(Err(io::Error::other(format!("dokan mount failed: {err}")))),
    }
}

/// The store, plus the one-shot channel that tells `try_new` the mount is live.
struct Handler<T: FileSystem> {
    store: T,

    /// Taken by whichever gets there first — [`mounted`](FileSystemHandler::mounted) on the
    /// way up, or a failed `mount()` on the way out. `None` afterwards, so a second report is
    /// dropped rather than blocking on a channel nobody is reading.
    ready: Mutex<Option<SyncSender<io::Result<()>>>>,
}

impl<T: FileSystem> Handler<T> {
    fn report(&self, outcome: io::Result<()>) {
        if let Some(ready) = crate::lock::lock(&self.ready).take() {
            let _ = ready.send(outcome);
        }
    }
}

impl<'c, 'h: 'c, T: FileSystem + 'h> FileSystemHandler<'c, 'h> for Handler<T> {
    /// Nothing. Every callback carries the full path, so there is no per-open fact that is not
    /// also in the arguments — and caching the resolved path here would go stale the moment
    /// [`move_file`](Self::move_file) renamed the object out from under it, which is exactly
    /// the case the kernel handles by passing the new name.
    type Context = ();

    /// Open, create, `mkdir` and "open a directory to list it", decomposed onto the store.
    ///
    /// The disposition decides two independent things — whether a name that is absent may be
    /// made, and whether a name that is present is emptied — and the order is the one
    /// `Posix::realize_open` uses, for the same reasons:
    ///
    /// * **Creation first**, because exclusivity is decided against the store and nothing
    ///   else. [`FileSystem::create`] is already exclusive, so `FILE_OPEN_IF` reads its
    ///   `AlreadyExists` as the answer it wanted and the stat-then-create race never exists.
    /// * **Truncation second, and only for a name that was already there**, because a file
    ///   this call just made is empty and resizing it is a round trip for nothing.
    ///
    /// `new_file_created` is reported honestly because Dokan reads it: for the three "or
    /// create" dispositions it turns a `false` into `STATUS_OBJECT_NAME_COLLISION`, which is
    /// NT's *informational* way of saying the handle is good but the file was already there —
    /// what Win32 surfaces as `ERROR_ALREADY_EXISTS` from a `CreateFile` that succeeded.
    #[allow(clippy::too_many_arguments)]
    fn create_file(
        &'h self,
        file_name: &U16CStr,
        _security_context: &IO_SECURITY_CONTEXT,
        _desired_access: ACCESS_MASK,
        _file_attributes: u32,
        _share_access: u32,
        create_disposition: u32,
        create_options: u32,
        _info: &mut OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<CreateFileInfo<Self::Context>> {
        let path = store_path(file_name)?;
        let want_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let want_file = create_options & FILE_NON_DIRECTORY_FILE != 0;

        // What this disposition is allowed to do. `exclusive` is NT's `O_EXCL`: the one
        // disposition for which an existing name is a failure rather than a fallback.
        let (may_create, may_truncate) = match create_disposition {
            FILE_CREATE => (true, false),
            FILE_OPEN => (false, false),
            FILE_OPEN_IF => (true, false),
            FILE_OVERWRITE => (false, true),
            FILE_OVERWRITE_IF | FILE_SUPERSEDE => (true, true),
            _ => return Err(STATUS_INVALID_PARAMETER),
        };
        let exclusive = create_disposition == FILE_CREATE;

        let created = if may_create {
            let made = if want_dir {
                block_on(self.store.mkdir(&path))
            } else {
                block_on(self.store.create(&path))
            };
            match made {
                Ok(stat) => Some(stat),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && !exclusive => None,
                Err(err) => return Err(nt_status(&err)),
            }
        } else {
            None
        };

        // Read before the match consumes it: what Dokan is told here is whether *this* call
        // made the file, and the match below is where a name that was already there stops
        // being distinguishable from one that was not.
        let new_file_created = created.is_some();

        let stat = match created {
            Some(stat) => stat,
            None => {
                // The one open that pays for its metadata; the store that made a file already
                // reported it. A name that is not there fails here, which is how `FILE_OPEN`
                // and `FILE_OVERWRITE` get their "must exist" without a branch of their own.
                let stat = block_on(self.store.stat(&path)).map_err(|err| nt_status(&err))?;
                // Directories are not emptied by a disposition; the kind checks below turn
                // that request into the error NT expects.
                if may_truncate && stat.kind == DirentKind::File {
                    block_on(self.store.truncate(&path, 0)).map_err(|err| nt_status(&err))?;
                }
                stat
            }
        };

        let is_dir = stat.kind == DirentKind::Dir;
        if is_dir && want_file {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        if !is_dir && want_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }

        Ok(CreateFileInfo {
            context: (),
            is_dir,
            new_file_created,
        })
    }

    /// Where a deletion actually happens.
    ///
    /// Windows decides to delete when the handle is opened or marked, and *performs* it when
    /// the last handle closes — so `delete_file`/`delete_directory` below are the check and
    /// this is the act. It is the inverse of POSIX, where the name goes immediately and the
    /// file lingers for the open descriptors, and it is why this binding needs nothing like
    /// `Posix`'s silly-rename: no open handle is ever left naming a file that is gone.
    ///
    /// No return value, which is the cost of that arrangement: a store that refuses the
    /// removal here has nowhere to report it, and the caller has already been told the delete
    /// would succeed. Saying so on stderr is the most that is left — which is why the checks
    /// below are worth making properly.
    fn cleanup(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) {
        if !info.delete_on_close() {
            return;
        }
        let Ok(path) = store_path(file_name) else {
            return;
        };
        let removed = if info.is_dir() {
            block_on(self.store.rmdir(&path))
        } else {
            block_on(self.store.unlink(&path))
        };
        if let Err(err) = removed {
            eprintln!("cortex: removing {} failed: {err}", path.display());
        }
    }

    fn read_file(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        buffer: &mut [u8],
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<u32> {
        let path = store_path(file_name)?;
        let offset = u64::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?;
        // A short read is EOF and nothing else (see `FileSystem::read_at`), and a short count
        // is how NT says the same thing, so this passes straight through.
        let read =
            block_on(self.store.read_at(&path, buffer, offset)).map_err(|e| nt_status(&e))?;
        Ok(read as u32)
    }

    fn write_file(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        buffer: &[u8],
        info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<u32> {
        let path = store_path(file_name)?;
        // `write_to_eof` is NT's `FILE_WRITE_TO_END_OF_FILE`, and the offset that came with it
        // is meaningless. The store contract has no append — a kernel that resolves the end
        // itself is the usual case, and this one does not — so the end is resolved here, which
        // costs a `stat` per append and is the reason `OpenOptions` carries no flag for it.
        let offset = if info.write_to_eof() {
            block_on(self.store.stat(&path))
                .map_err(|e| nt_status(&e))?
                .size
        } else {
            u64::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?
        };

        // One loop above every store, as `Posix::write_handle` does it: `write_at` may legally
        // write less than it was given, and NT will not resend the remainder for us.
        let mut written = 0usize;
        while written < buffer.len() {
            let n = block_on(self.store.write_at(
                &path,
                &buffer[written..],
                offset + written as u64,
            ))
            .map_err(|e| nt_status(&e))?;
            if n == 0 {
                // No progress on a non-empty buffer has no defined meaning, and looping on it
                // would not terminate.
                return Err(STATUS_UNEXPECTED_IO_ERROR);
            }
            written += n;
        }
        Ok(written as u32)
    }

    fn flush_file_buffers(
        &'h self,
        file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        let path = store_path(file_name)?;
        block_on(self.store.flush(&path)).map_err(|e| nt_status(&e))
    }

    fn get_file_information(
        &'h self,
        file_name: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<FileInfo> {
        let path = store_path(file_name)?;
        let stat = block_on(self.store.stat(&path)).map_err(|e| nt_status(&e))?;
        let attr = attr_for(&stat);
        Ok(FileInfo {
            attributes: file_attributes(stat.kind),
            creation_time: attr.crtime,
            last_access_time: attr.atime,
            last_write_time: attr.mtime,
            file_size: attr.size,
            // As `attr_for` reports it, and for the same reason: nothing here has hard links.
            number_of_links: 1,
            file_index: file_index(&path),
        })
    }

    /// Stream the directory's children.
    ///
    /// No `.` or `..`: the NT convention is that a filesystem driver does not synthesise them
    /// and the shell does not expect them, which is the opposite of the FUSE bindings, where
    /// `Posix::dir_entries` puts both at the head of every listing.
    fn find_files(
        &'h self,
        file_name: &U16CStr,
        mut fill_find_data: impl FnMut(&FindData) -> FillDataResult,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        let path = store_path(file_name)?;
        let children = block_on(self.store.list(&path)).map_err(|e| nt_status(&e))?;

        for child in children {
            // A listing may or may not carry metadata (see `Dirent::stat`), and a name whose
            // store did not is worth a round trip rather than a row of zeroes: Explorer sorts
            // and filters on what is here.
            let stat = match child.stat() {
                Some(stat) => stat.clone(),
                None => match block_on(self.store.stat(&path.join(&child.name))) {
                    Ok(stat) => stat,
                    // Raced away between the listing and the stat. It is not in the directory
                    // any more, so it is not in this listing of it either.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(nt_status(&err)),
                },
            };
            let Ok(name) = U16CString::from_str(&child.name) else {
                // An interior nul cannot be a Windows file name at all, so there is nothing to
                // report it as. Dropping it keeps the rest of the listing.
                continue;
            };

            let attr = attr_for(&stat);
            let filled = fill_find_data(&FindData {
                attributes: file_attributes(stat.kind),
                creation_time: attr.crtime,
                last_access_time: attr.atime,
                last_write_time: attr.mtime,
                file_size: attr.size,
                file_name: name,
            });
            match filled {
                Ok(()) => {}
                // `WIN32_FIND_DATAW` has no room for a name this long, so it cannot be
                // listed — but the directory around it can.
                Err(FillDataError::NameTooLong) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// Accepted and dropped, like `Posix::setattr_inode` drops a mode.
    ///
    /// Nothing stores Windows attributes and [`file_attributes`] reports a fixed pair, so
    /// there is nothing to write. Failing instead would break `copy`, `xcopy` and `attrib` for
    /// no gain; the caller's next `GetFileInformationByHandle` shows what stuck.
    fn set_file_attributes(
        &'h self,
        _file_name: &U16CStr,
        _file_attributes: u32,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        Ok(())
    }

    /// Accepted and dropped, for the same reason as [`set_file_attributes`](Self::set_file_attributes).
    ///
    /// The store contract has no way to set a timestamp — [`SetAttr`](crate::fs::SetAttr)
    /// carries `mtime`/`atime` and `Posix` drops them too.
    fn set_file_time(
        &'h self,
        _file_name: &U16CStr,
        _creation_time: FileTimeOperation,
        _last_access_time: FileTimeOperation,
        _last_write_time: FileTimeOperation,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        Ok(())
    }

    /// May this file be deleted? The removal itself is [`cleanup`](Self::cleanup)'s.
    ///
    /// Also called with `delete_on_close` false, to *withdraw* a delete that was requested and
    /// then abandoned. There is nothing to undo — nothing has happened yet — and answering an
    /// error would fail the withdrawal, so that case is a plain `Ok`.
    fn delete_file(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        if !info.delete_on_close() {
            return Ok(());
        }
        let path = store_path(file_name)?;
        let stat = block_on(self.store.stat(&path)).map_err(|e| nt_status(&e))?;
        match stat.kind {
            DirentKind::File => Ok(()),
            DirentKind::Dir => Err(STATUS_FILE_IS_A_DIRECTORY),
        }
    }

    /// May this directory be deleted? Only if it is empty, which is this check's whole job:
    /// `cleanup` cannot report the `DirectoryNotEmpty` that `rmdir` would answer with.
    fn delete_directory(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        if !info.delete_on_close() {
            return Ok(());
        }
        let path = store_path(file_name)?;
        let children = block_on(self.store.list(&path)).map_err(|e| nt_status(&e))?;
        if children.is_empty() {
            Ok(())
        } else {
            Err(STATUS_DIRECTORY_NOT_EMPTY)
        }
    }

    /// Rename or move, which NT spells as one operation and so does the store.
    ///
    /// [`FileSystem::rename`] replaces whatever was at the destination, which is `rename(2)`'s
    /// rule and not NT's: `MoveFileEx` without `MOVEFILE_REPLACE_EXISTING` must fail on an
    /// occupied name. So the refusal is decided here, by looking first — which is a race the
    /// store contract gives no way to close, and the narrow one: a destination that appears
    /// between the check and the rename is overwritten.
    fn move_file(
        &'h self,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_existing: bool,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        let from = store_path(file_name)?;
        let to = store_path(new_file_name)?;
        if !replace_if_existing && from != to && block_on(self.store.stat(&to)).is_ok() {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        block_on(self.store.rename(&from, &to)).map_err(|e| nt_status(&e))
    }

    fn set_end_of_file(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        let path = store_path(file_name)?;
        let size = u64::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?;
        block_on(self.store.truncate(&path, size)).map_err(|e| nt_status(&e))
    }

    /// Shrink to the requested allocation, and otherwise do nothing.
    ///
    /// An allocation size is a *reservation*, which no store here has a notion of, so growing
    /// one is a promise nothing needs to keep. Shrinking is different: NT uses this to cut a
    /// file down, and ignoring it would leave the bytes past the new end readable.
    fn set_allocation_size(
        &'h self,
        file_name: &U16CStr,
        alloc_size: i64,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        let path = store_path(file_name)?;
        let size = u64::try_from(alloc_size).map_err(|_| STATUS_INVALID_PARAMETER)?;
        let stat = block_on(self.store.stat(&path)).map_err(|e| nt_status(&e))?;
        if stat.size > size {
            block_on(self.store.truncate(&path, size)).map_err(|e| nt_status(&e))?;
        }
        Ok(())
    }

    /// Synthetic capacity; see [`TOTAL_BLOCKS`].
    fn get_disk_free_space(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<DiskSpaceInfo> {
        let bytes = TOTAL_BLOCKS * BLOCK_SIZE;
        Ok(DiskSpaceInfo {
            byte_count: bytes,
            free_byte_count: bytes,
            available_byte_count: bytes,
        })
    }

    fn get_volume_information(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<VolumeInfo> {
        Ok(VolumeInfo {
            name: U16CString::from_str(VOLUME_NAME).expect("volume name has no interior nul"),
            serial_number: 0,
            max_component_length: NAME_MAX,
            // **Case-sensitive, which Windows software does not expect.** The stores compare
            // names by their bytes, so `README` and `readme` are two files on a cortex tree
            // whatever the platform convention is — and reporting otherwise would make the
            // shell hide one of them. A caller that needs the usual behaviour needs a store
            // that folds case; it is not something this binding can add.
            fs_flags: FILE_CASE_PRESERVED_NAMES | FILE_CASE_SENSITIVE_SEARCH | FILE_UNICODE_ON_DISK,
            fs_name: U16CString::from_str(FS_NAME).expect("fs name has no interior nul"),
        })
    }

    /// The volume is live. This is what [`DokanMount::try_new`] has been waiting for.
    fn mounted(
        &'h self,
        _mount_point: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<()> {
        self.report(Ok(()));
        Ok(())
    }

    fn unmounted(&'h self, _info: &OperationInfo<'c, 'h, Self>) -> OperationResult<()> {
        Ok(())
    }
}

/// Drive a store call to completion from Dokan's synchronous callback thread.
///
/// Thin alias for the shared [`block_on`](super::block_on), named locally so the callbacks read
/// as one operation rather than as a module path repeated forty times.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    super::block_on(fut)
}

/// The store path a Dokan file name denotes.
///
/// Dokan hands over a path from the volume root in NT spelling — `\` for the root,
/// `\src\main.rs` below it — and the stores want the same path in theirs: rooted, with `/`
/// separators, which is what `Posix` hands them too.
///
/// Split by hand rather than by handing the string to [`PathBuf`], which on Windows would
/// accept a backslash run but would also read a leading `C:` as a drive prefix — and a
/// component is not a place to discover that a name was really a path.
///
/// What is rejected:
///
/// * `.` and `..`, which the object manager resolves before an IRP is built. One arriving is
///   not a path to normalise, it is a name that cannot mean what it says.
/// * `:`, which separates an alternate data stream from the file carrying it. This binding
///   does not ask for [`MountFlags::ALT_STREAM`], so a name with one in it is a request for
///   something that is not there rather than a file whose name happens to contain a colon —
///   Windows does not allow that anyway.
fn store_path(name: &U16CStr) -> Result<PathBuf, NTSTATUS> {
    const SEPARATOR: u16 = b'\\' as u16;
    const COLON: u16 = b':' as u16;

    let mut path = PathBuf::from("/");
    for part in name.as_slice().split(|&unit| unit == SEPARATOR) {
        if part.is_empty() {
            continue;
        }
        if part.contains(&COLON) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        let component = OsString::from_wide(part);
        if component == "." || component == ".." {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        path.push(component);
    }
    Ok(path)
}

/// Translate a store error into the numbering *this* consumer expects, which on Windows is
/// `NTSTATUS`.
///
/// The counterpart of `host_errno`, and split from it rather than sharing a table: the two
/// answer different kernels, and there is no correspondence to factor out — `ENOTEMPTY` and
/// `STATUS_DIRECTORY_NOT_EMPTY` are the same *idea* and nothing more.
///
/// `raw_os_error` first, exactly as `host_errno` does it and for the same reason: a number is
/// here because this host's own syscall produced it — a passthrough store's `std::fs` call —
/// so it is a Win32 error from `GetLastError`, and Dokan's own converter maps it more
/// precisely than the kind it was classified into.
///
/// **No exhaustive match is possible.** [`io::ErrorKind`] is `#[non_exhaustive]`, so a kind
/// this crate starts producing lands on the `_` arm and reaches Windows as
/// `STATUS_UNSUCCESSFUL` — a valid status and therefore a silent wrong answer. A store that
/// answers with a kind not named below has to add it, here and in `host_errno` both.
fn nt_status(err: &io::Error) -> NTSTATUS {
    if let Some(code) = err.raw_os_error() {
        return map_win32_error_to_ntstatus(code as u32);
    }
    match err.kind() {
        io::ErrorKind::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        io::ErrorKind::NotADirectory => STATUS_NOT_A_DIRECTORY,
        io::ErrorKind::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        io::ErrorKind::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        io::ErrorKind::DirectoryNotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        io::ErrorKind::InvalidFilename => STATUS_OBJECT_NAME_INVALID,
        io::ErrorKind::InvalidInput => STATUS_INVALID_PARAMETER,
        io::ErrorKind::FileTooLarge => STATUS_FILE_TOO_LARGE,
        io::ErrorKind::PermissionDenied => STATUS_ACCESS_DENIED,
        io::ErrorKind::StorageFull => STATUS_DISK_FULL,
        io::ErrorKind::ReadOnlyFilesystem => STATUS_MEDIA_WRITE_PROTECTED,
        io::ErrorKind::CrossesDevices => STATUS_NOT_SAME_DEVICE,
        io::ErrorKind::Unsupported => STATUS_NOT_IMPLEMENTED,
        io::ErrorKind::WriteZero => STATUS_UNEXPECTED_IO_ERROR,
        _ => STATUS_UNSUCCESSFUL,
    }
}

/// The Windows attributes an entry of this kind reports.
///
/// The counterpart of `mode_for`, and as fixed as that one is: nothing under [`FileSystem`]
/// stores an attribute, so what is left is the type bit. `FILE_ATTRIBUTE_NORMAL` is
/// specifically "no other attribute set" and is not a flag to be OR'd with the directory bit,
/// which is why these are two arms rather than a base plus a modifier.
fn file_attributes(kind: DirentKind) -> u32 {
    match kind {
        DirentKind::Dir => FILE_ATTRIBUTE_DIRECTORY,
        DirentKind::File => FILE_ATTRIBUTE_NORMAL,
    }
}

/// A stable identity for a path, which is what `nFileIndex` is read as.
///
/// Hashed rather than counted, which is the whole reason this binding needs no inode table: a
/// number derived from the path is the same number every time that path is asked about, with
/// nothing to keep, evict or reference-count. What it gives up is what a table would have
/// bought — two hard links to one file sharing an index — and nothing under [`FileSystem`] has
/// hard links to begin with.
///
/// Collisions are possible and harmless here: this is advisory, and the applications that read
/// it compare two indexes to ask whether two *paths* are the same file, which is the question
/// the hash already answers.
fn file_index(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> U16CString {
        U16CString::from_str(s).unwrap()
    }

    #[test]
    fn the_volume_root_is_the_store_root() {
        assert_eq!(store_path(&wide("\\")).unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn a_path_keeps_its_components_and_changes_its_separator() {
        assert_eq!(
            store_path(&wide("\\src\\main.rs")).unwrap(),
            PathBuf::from("/").join("src").join("main.rs")
        );
    }

    #[test]
    fn a_relative_name_is_rooted_like_every_other() {
        assert_eq!(
            store_path(&wide("notes.md")).unwrap(),
            PathBuf::from("/").join("notes.md")
        );
    }

    #[test]
    fn dot_components_are_refused_rather_than_resolved() {
        assert_eq!(
            store_path(&wide("\\src\\..\\etc")),
            Err(STATUS_OBJECT_NAME_INVALID)
        );
        assert_eq!(store_path(&wide("\\.")), Err(STATUS_OBJECT_NAME_INVALID));
    }

    #[test]
    fn an_alternate_stream_is_not_a_file_this_mount_has() {
        assert_eq!(
            store_path(&wide("\\notes.md:hidden")),
            Err(STATUS_OBJECT_NAME_INVALID)
        );
    }

    #[test]
    fn a_kind_reports_one_attribute_and_not_a_pair() {
        // `FILE_ATTRIBUTE_NORMAL` means "nothing else is set", so it must never appear
        // alongside the directory bit.
        assert_eq!(file_attributes(DirentKind::Dir), FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(file_attributes(DirentKind::File), FILE_ATTRIBUTE_NORMAL);
    }

    #[test]
    fn an_index_is_the_same_every_time_it_is_asked_for() {
        let path = PathBuf::from("/src/main.rs");
        assert_eq!(file_index(&path), file_index(&path));
        assert_ne!(file_index(&path), file_index(Path::new("/src/lib.rs")));
    }

    #[test]
    fn a_kind_with_no_raw_number_still_lands_on_a_status() {
        assert_eq!(
            nt_status(&io::ErrorKind::NotFound.into()),
            STATUS_OBJECT_NAME_NOT_FOUND
        );
        assert_eq!(
            nt_status(&io::ErrorKind::DirectoryNotEmpty.into()),
            STATUS_DIRECTORY_NOT_EMPTY
        );
    }
}
