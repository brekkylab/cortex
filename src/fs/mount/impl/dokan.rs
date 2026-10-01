//! Binds a [`FileSystem`] to Dokany's user-mode filesystem API.
//!
//! **It does not go through [`Posix`](crate::fs::Posix).** Every callback carries the full
//! path from the volume root (`\`, `\src\main.rs`) because the NT I/O manager resolves names
//! itself, so there is no lookup or inode, and mapping paths to numbers and back would cost a
//! round trip per callback for nothing.
//!
//! Shares [`attr_for`] (attribute policy, timestamp fallbacks) and the synthetic capacity
//! constants with the other bindings, so Windows readers see the same numbers; errors are
//! numbered by [`nt_status`] and attributes filled into [`FileInfo`].

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
    time::{Duration, Instant},
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
    um::{
        fileapi::GetVolumeInformationW,
        winnt::{
            ACCESS_MASK, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
            FILE_CASE_PRESERVED_NAMES, FILE_CASE_SENSITIVE_SEARCH, FILE_UNICODE_ON_DISK,
        },
    },
};

use crate::fs::{
    DirentKind, FileSystem, Mount,
    filesystem::posix::{BLOCK_SIZE, NAME_MAX, TOTAL_BLOCKS, attr_for},
};

/// The volume label Explorer shows beside the drive letter.
const VOLUME_NAME: &str = "virtx";

/// What the volume reports as its *format*.
///
/// `NTFS` on purpose: UAC refuses to elevate a process whose image is on a filesystem it does
/// not recognise, so a custom name breaks running an installer off the mount.
const FS_NAME: &str = "NTFS";

/// How long the driver waits for one of these callbacks before it gives up on the operation.
///
/// Generous, since a store may answer across a network while the driver's default assumes a
/// local disk. It is a hard ceiling: past it the driver abandons the operation and the caller
/// sees an I/O error.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long [`DokanMount::try_new`] waits for the driver to report the mount is live.
///
/// A backstop for a driver that accepts the filesystem but never calls
/// [`mounted`](FileSystemHandler::mounted); most failures fail `mount()` outright. It also
/// bounds the wait after that for the mount point to lead to the volume (see [`answers`]).
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often [`DokanMount::try_new`] asks whether the mount point leads to the volume yet.
/// Short, because what is being waited out is measured in tens of milliseconds.
const ANSWER_POLL: Duration = Duration::from_millis(5);

/// A live Dokan mount: constructing one mounts, dropping it unmounts.
///
/// Needs Dokany: the driver (`dokan2.sys`) comes from its installer, and this links the
/// user-mode DLL. `dokan-sys` links the *installed* library when
/// `DokanLibrary2_LibraryPath_x64` is set (the installer sets it) and otherwise builds its
/// vendored sources, whose version may not match the driver, which surfaces as
/// [`FileSystemMountError::Version`](::dokan::FileSystemMountError::Version) at mount time.
///
/// **No [`Claim`](super::super::claim::Claim)**: Dokany's driver tears the volume down when
/// the registering process dies, however it dies, so nothing is left for a later run to
/// reclaim.
pub struct DokanMount {
    mountpoint: PathBuf,

    /// The mount point as the driver names it. `Drop` unmounts by name
    /// (`DokanRemoveMountPoint`), since the filesystem's handle belongs to the serving thread,
    /// which is blocked inside it.
    wide: U16CString,

    /// The serving thread. `None` once [`join`](Self::join) or `Drop` took it.
    serving: Option<JoinHandle<()>>,
}

impl DokanMount {
    /// Mount `fs` at `mountpoint` and serve it from a background thread.
    ///
    /// `mountpoint` is a drive letter (`Z:\`) or an existing empty directory on an NTFS
    /// volume.
    ///
    /// Returns once the path answers as the mounted volume, so it is openable by the time the
    /// caller has the guard. That takes two waits, as on FUSE-T: `mount()` returns on
    /// registration, before the volume exists, and the driver reports the volume live before
    /// the mount point is sure to lead to it (see [`answers`]).
    ///
    /// `'static` because the store is served from that thread for the mount's lifetime.
    pub fn try_new<T: FileSystem + 'static>(fs: T, mountpoint: &Path) -> io::Result<Self> {
        Self::try_new_with(fs, mountpoint, MountFlags::empty())
    }

    /// [`try_new`](Self::try_new) with the mount flags spelled out.
    ///
    /// [`MountFlags::WRITE_PROTECT`] makes the *driver* reject writes before any store sees
    /// them, including stores that would have answered `Ok`.
    ///
    /// [`MountFlags::CURRENT_SESSION`] matters because a drive letter belongs to a logon
    /// session: a service's mount is invisible to the desktop unless the mount manager
    /// publishes it.
    ///
    /// [`MountFlags::CASE_SENSITIVE`] is added whatever `flags` says: the stores tell names
    /// apart by case, and a driver that does not mistakes one file for another.
    pub fn try_new_with<T: FileSystem + 'static>(
        fs: T,
        mountpoint: &Path,
        flags: MountFlags,
    ) -> io::Result<Self> {
        // Before the first call into delay-loaded `dokan2.dll`: a missing DLL fails that call
        // with an SEH exception no `Result` catches.
        crate::fs::mount_support()?;

        let wide = U16CString::from_os_str(mountpoint).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidFilename,
                "mount point contains an interior nul",
            )
        })?;

        // One slot: readiness is reported once, and a send must never block the serving
        // thread if nobody is listening.
        let (ready, mounted) = mpsc::sync_channel(1);
        let serving = {
            let wide = wide.clone();
            std::thread::Builder::new()
                .name("virtx-dokan".into())
                .spawn(move || serve(fs, wide, flags, ready))?
        };

        let deadline = Instant::now() + MOUNT_TIMEOUT;
        match mounted.recv_timeout(MOUNT_TIMEOUT) {
            Ok(Ok(())) => {
                let mount = DokanMount {
                    mountpoint: mountpoint.to_path_buf(),
                    wide,
                    serving: Some(serving),
                };
                loop {
                    match answers(&mount.wide) {
                        Ok(()) => return Ok(mount),
                        Err(last) if Instant::now() >= deadline => {
                            // The guard exists, so its drop is what takes the volume down.
                            drop(mount);
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!(
                                    "dokan reported the mount live, but the mount point did \
                                     not answer as the volume within {MOUNT_TIMEOUT:?}; its \
                                     last answer: {last}"
                                ),
                            ));
                        }
                        Err(_) => std::thread::sleep(ANSWER_POLL),
                    }
                }
            }
            // Never mounted, so nothing to take down.
            Ok(Err(err)) => {
                let _ = serving.join();
                Err(err)
            }
            // Dropped without reporting: the thread unwound.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = serving.join();
                Err(io::Error::other("virtx dokan serving thread ended"))
            }
            // Accepted but silent. It may yet register, so take it down by name rather than
            // leave an unguarded volume.
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

    /// Serve until something else ends the mount (`dokanctl /u`, an eject from Explorer, or
    /// the driver). **It does not unmount**; to end the mount, drop the guard.
    ///
    /// `Err` if the serving thread panicked.
    pub fn join(mut self) -> io::Result<()> {
        match self.serving.take() {
            Some(serving) => serving
                .join()
                .map_err(|_| io::Error::other("virtx dokan serving thread panicked")),
            None => Ok(()),
        }
    }
}

impl Mount for DokanMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// `file:///Z:/...`. Plain `file://` + path would give `file://Z:\x`, whose authority is
    /// `Z:`, since a Windows path lacks the leading `/`.
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
    /// Order is forced: the thread blocks in Dokan's wait-for-closed until the volume is gone,
    /// so joining first would deadlock.
    ///
    /// Never panics (a panic mid-unwind aborts, hiding a failing test's real assertion): a
    /// refused unmount is reported and the thread left running rather than joined into a hang.
    fn drop(&mut self) {
        let Some(serving) = self.serving.take() else {
            return;
        };
        if !::dokan::unmount(&self.wide) {
            eprintln!(
                "virtx: unmounting {} failed; the volume is left for dokanctl to clear",
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
    /// **`unmount` leaves a reparse point onto the gone volume** where the empty directory was:
    /// invisible to a parent listing, unopenable, and refused with `ERROR_ALREADY_EXISTS` by
    /// the next `create_dir_all`, so a second mount of the path would fail.
    ///
    /// `symlink_metadata`, not `exists`, which follows the reparse point and answers that
    /// nothing is there. `remove_dir` does not follow it, so it removes only the junction.
    ///
    /// Failures are ignored: the mount is already down, and the next mount reports the
    /// leftover itself.
    fn reclaim_mountpoint(&self) {
        use std::os::windows::fs::MetadataExt as _;

        /// `FILE_ATTRIBUTE_REPARSE_POINT`, inlined.
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

/// Whether the mount point leads to the mounted volume yet, and if not, what it answered.
///
/// [`mounted`](FileSystemHandler::mounted) says the volume exists, not that the mount point
/// leads to it; with several mounts coming up at once the two are tens of milliseconds apart,
/// and a file opened in between fails with `ERROR_INVALID_FUNCTION` or
/// `ERROR_INVALID_PARAMETER`.
///
/// The volume answers [`VOLUME_NAME`] and a serial of 0, which no host volume answers both of.
/// The query opens the root first, so each poll reaches the store as a `create_file("\")` and
/// a `stat("/")`. A store whose root `stat` fails never answers, and the error is the reason.
fn answers(mountpoint: &U16CStr) -> io::Result<()> {
    const BACKSLASH: u16 = b'\\' as u16;

    // A mounted folder is named as a root, with its trailing separator.
    let mut root = mountpoint.as_slice().to_vec();
    if root.last() != Some(&BACKSLASH) {
        root.push(BACKSLASH);
    }
    root.push(0);

    let mut label = [0u16; 64];
    let mut serial = 0u32;
    // SAFETY: `root` is nul-terminated, and `label` is as long as the length passed with it.
    // The outputs not asked for are null, which the call allows.
    let answered = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if answered == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = label
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(label.len());
    let label = String::from_utf16_lossy(&label[..len]);
    if serial == 0 && label == VOLUME_NAME {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "a volume labelled {label:?} with serial {serial:#010x}, not this one"
        )))
    }
}

/// Initialise the Dokan library, once per process.
///
/// `DokanShutdown` is **never called**: it is undefined while any mount is up, and callers
/// decide mount lifetimes, so no moment is safe. Process exit releases it anyway.
fn library() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(::dokan::init);
}

/// The body of the serving thread: build the handler, mount, and stay until it is unmounted.
///
/// The mounter borrows the handler, mount point and options for the filesystem's life, which
/// a guard struct could only hold self-referentially; so they are this thread's locals and the
/// guard holds only the name to unmount by.
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
        // Whatever the caller asked for: the stores compare names by their bytes, as
        // `get_volume_information` already says. Without it the driver matches its searches and
        // open files without regard to case, so two open files differing only in case were one.
        flags: flags | MountFlags::CASE_SENSITIVE,
        timeout: OPERATION_TIMEOUT,
        // Matches the free-space reply's block; the library default (`0`) would not.
        allocation_unit_size: BLOCK_SIZE as u32,
        sector_size: BLOCK_SIZE as u32,
        ..Default::default()
    };

    let mut mounter = FileSystemMounter::new(&handler, &wide, &options);
    match mounter.mount() {
        // Dropping the filesystem blocks until the volume is gone, keeping this thread and
        // everything the mounter borrowed alive for the mount.
        Ok(fs) => drop(fs),
        Err(err) => handler.report(Err(io::Error::other(format!("dokan mount failed: {err}")))),
    }
}

/// The store, plus the one-shot channel that tells `try_new` the mount is live.
struct Handler<T: FileSystem> {
    store: T,

    /// Taken by the first reporter ([`mounted`](FileSystemHandler::mounted) or a failed
    /// `mount()`); `None` afterwards, so a second report is dropped.
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
    /// Nothing: every callback carries the full path, and a cached path would go stale after
    /// [`move_file`](Self::move_file).
    type Context = ();

    /// Open, create, `mkdir` and "open a directory to list it", decomposed onto the store.
    ///
    /// The disposition decides whether an absent name may be made and whether a present one
    /// is emptied:
    ///
    /// * **Creation first**, since [`FileSystem::create`] is exclusive: `FILE_OPEN_IF` treats
    ///   `AlreadyExists` as success, so there is no stat-then-create race.
    /// * **Truncation second, only for a name that already existed**; a new file is empty.
    ///
    /// `desired_access` is not checked: the NT kernel checks it before building the IRP, and
    /// paging I/O (memory-mapped reads) arrives on handles whose access is meaningless, so a
    /// second check here would reject allowed work.
    ///
    /// `new_file_created` must be accurate: for the "or create" dispositions Dokan turns
    /// `false` into the informational `STATUS_OBJECT_NAME_COLLISION` (Win32's
    /// `ERROR_ALREADY_EXISTS` from a successful `CreateFile`).
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

        // `exclusive` is NT's `O_EXCL`: an existing name is a failure, not a fallback.
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

        // Read before the match consumes it: Dokan must know whether *this* call made the file.
        let new_file_created = created.is_some();

        let stat = match created {
            Some(stat) => stat,
            None => {
                // A missing name fails here, which gives `FILE_OPEN` and `FILE_OVERWRITE`
                // their "must exist".
                let stat = block_on(self.store.stat(&path)).map_err(|err| nt_status(&err))?;
                // Directories are never emptied; the kind checks below produce NT's error.
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
    /// Windows decides a delete at open or mark time and performs it when the last handle
    /// closes; `delete_file`/`delete_directory` are the check, this is the act. Since the name
    /// outlives every handle, no silly-rename is needed and the store never sees a file vanish
    /// under an open handle.
    ///
    /// No return value: a store refusing here can only report on stderr, after the caller was
    /// told the delete would succeed, so those checks must be thorough.
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
            eprintln!("virtx: removing {} failed: {err}", path.display());
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
        // A short read means EOF in both the store contract and NT, so it passes through.
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
        // `write_to_eof` is `FILE_WRITE_TO_END_OF_FILE`, whose offset is meaningless. The store
        // contract has no append, so the end is resolved here at a `stat` per append.
        let offset = if info.write_to_eof() {
            block_on(self.store.stat(&path))
                .map_err(|e| nt_status(&e))?
                .size
        } else {
            u64::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?
        };

        // `write_at` may write short, and NT will not resend the remainder.
        let mut written = 0usize;
        while written < buffer.len() {
            let n = block_on(self.store.write_at(
                &path,
                &buffer[written..],
                offset + written as u64,
            ))
            .map_err(|e| nt_status(&e))?;
            if n == 0 {
                // No progress on a non-empty buffer would loop forever.
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
            // Nothing here has hard links.
            number_of_links: 1,
            file_index: file_index(&path),
        })
    }

    /// No `.` or `..`: by NT convention a filesystem driver does not synthesise them.
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
            // Missing metadata is fetched rather than zeroed: Explorer sorts and filters on it.
            let stat = match child.stat() {
                Some(stat) => stat.clone(),
                None => match block_on(self.store.stat(&path.join(&child.name))) {
                    Ok(stat) => stat,
                    // Removed between the listing and the stat; omit it.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(nt_status(&err)),
                },
            };
            let Ok(name) = U16CString::from_str(&child.name) else {
                // An interior nul cannot be a Windows name; skip it, keep the rest.
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
                // Too long for `WIN32_FIND_DATAW`; skip it, keep the rest.
                Err(FillDataError::NameTooLong) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    /// Accepted and dropped: nothing stores Windows attributes ([`file_attributes`] is fixed),
    /// and failing would break `copy`, `xcopy` and `attrib`. The caller's next
    /// `GetFileInformationByHandle` shows what stuck.
    fn set_file_attributes(
        &'h self,
        _file_name: &U16CStr,
        _file_attributes: u32,
        _info: &OperationInfo<'c, 'h, Self>,
        _context: &'c Self::Context,
    ) -> OperationResult<()> {
        Ok(())
    }

    /// Accepted and dropped: the store contract cannot set timestamps, and failing would break
    /// copy tools.
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
    /// Also called with `delete_on_close` false to withdraw a requested delete; nothing has
    /// happened yet, so that is a plain `Ok`.
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

    /// May this directory be deleted? Only if empty, since `cleanup` cannot report
    /// `DirectoryNotEmpty`.
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

    /// Rename or move.
    ///
    /// [`FileSystem::rename`] replaces the destination (`rename(2)`'s rule), but `MoveFileEx`
    /// without `MOVEFILE_REPLACE_EXISTING` must fail on an occupied name, so this checks first.
    /// The store contract cannot close the race: a destination appearing in between is
    /// overwritten.
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
    /// Stores have no reservations, so growing is a no-op; but NT shrinks files this way, and
    /// ignoring it would leave bytes past the new end readable.
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
            // **Case-sensitive, unlike Windows convention.** Stores compare bytes, so `README`
            // and `readme` are two files; reporting otherwise would make the shell hide one.
            // Case folding would have to come from the store.
            fs_flags: FILE_CASE_PRESERVED_NAMES | FILE_CASE_SENSITIVE_SEARCH | FILE_UNICODE_ON_DISK,
            fs_name: U16CString::from_str(FS_NAME).expect("fs name has no interior nul"),
        })
    }

    /// The volume is live; [`DokanMount::try_new`] waits for this.
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

/// Shorthand for [`block_on`](super::block_on), to keep the callbacks short.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    super::block_on(fut)
}

/// The store path a Dokan file name denotes.
///
/// Dokan's NT spelling (`\`, `\src\main.rs`) becomes a rooted path built with
/// [`PathBuf::push`], which on Windows separates with `\` too, so stores split it by component
/// rather than on `/`.
///
/// Split by hand: [`PathBuf`] on Windows would read a leading `C:` as a drive prefix.
///
/// Rejected:
///
/// * `.` and `..`, which the object manager resolves before an IRP is built, so one arriving
///   cannot mean what it says.
/// * `:`, the alternate data stream separator. [`MountFlags::ALT_STREAM`] is not requested by
///   `try_new`, and Windows forbids colons in names anyway.
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

/// A store error as the `NTSTATUS` a Windows caller expects.
///
/// Its own table, not shared with errno: NTSTATUS and errno values only correspond as ideas.
///
/// `raw_os_error` first: a raw number came from this host's own syscall (e.g. a passthrough
/// store's `std::fs`), so it is a Win32 error Dokan's converter maps more precisely than its
/// kind.
///
/// **No exhaustive match is possible.** [`io::ErrorKind`] is `#[non_exhaustive]`, so an
/// unlisted kind silently becomes `STATUS_UNSUCCESSFUL`. A store producing a new kind must add
/// it here and in `host_errno`.
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
/// Fixed, since nothing under [`FileSystem`] stores attributes. Two arms rather than a base
/// plus a flag: `FILE_ATTRIBUTE_NORMAL` means "nothing else set" and must not be OR'd with the
/// directory bit.
fn file_attributes(kind: DirentKind) -> u32 {
    match kind {
        DirentKind::Dir => FILE_ATTRIBUTE_DIRECTORY,
        DirentKind::File => FILE_ATTRIBUTE_NORMAL,
    }
}

/// A stable identity for a path, which is what `nFileIndex` is read as.
///
/// Hashed from the path, so there is no inode table to keep, evict or reference-count; hard
/// links would get distinct indices, but nothing under [`FileSystem`] has any. Collisions are
/// harmless: the index is advisory, used to ask whether two paths are the same file.
fn file_index(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// Only the pure helpers (name translation, attributes, index, status table): callbacks answer
/// through objects only Dokan constructs, so the rest needs a real mount against a real driver.
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
