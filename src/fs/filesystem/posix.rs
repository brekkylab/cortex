//! POSIX bookkeeping over a path-addressed [`FileSystem`] store: [`Posix`], and the kernel's
//! requests ([`OpenOptions`], [`SetAttr`]), which stop here since no store sees them.

// What a build reads depends on which bindings are enabled, in non-aligned subsets (none
// without a kernel binding; `unix_time` only for C-`stat` bindings; `TTL` only for those
// passing a timeout from Rust). Per-item gating would track binding internals, so the module
// opts out wholesale.
#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    fs::{Dirent, DirentKind, FileSystem, Stat},
    lock::lock,
};

/// How a file should be opened.
///
/// The options travel *with* the open because two are atomicity requirements:
///
/// * `create_new` is `O_EXCL`; "stat, then create if absent" is a race, and for a local
///   backend (`O_CREAT|O_EXCL`) or object store (`If-None-Match: *`) the atomic form is the
///   only one.
/// * `truncate` must take effect before anything observes the file, so returned metadata
///   already shows it empty; [`FileSystem::truncate`] is the separate, non-atomic resize.
///
/// There is deliberately no `append`: a kernel resolves `O_APPEND` itself and sends the
/// absolute end offset, and the flag would oblige every store to find-the-end-and-write
/// atomically. A caller wanting it seeks to the end first.
///
/// Built like an `open(2)` call: an access mode, then modifying flags. Fields are public for
/// bindings to read; `#[non_exhaustive]` stops callers outside the crate constructing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub truncate: bool,
    /// Create the file if it is absent. The parent directory is never created.
    pub create: bool,
    /// Fail with [`AlreadyExists`] if the file is there; honoured only with `create`, as `O_EXCL`.
    ///
    /// [`AlreadyExists`]: io::ErrorKind::AlreadyExists
    pub create_new: bool,
}

impl OpenOptions {
    pub fn read_only() -> Self {
        OpenOptions {
            read: true,
            ..Default::default()
        }
    }

    pub fn write_only() -> Self {
        OpenOptions {
            write: true,
            ..Default::default()
        }
    }

    pub fn read_write() -> Self {
        OpenOptions {
            read: true,
            write: true,
            ..Default::default()
        }
    }

    /// A file that must not exist yet, opened read-write: `O_CREAT | O_EXCL | O_RDWR`, as
    /// [`File::create_new`](std::fs::File::create_new).
    ///
    /// The most common open, spelled once so every caller gets the exclusive form. No setter
    /// pairs with it; narrow access instead, e.g. `create_new().read(false)`.
    pub fn create_new() -> Self {
        OpenOptions {
            create: true,
            create_new: true,
            ..Self::read_write()
        }
    }

    pub fn read(self, yes: bool) -> Self {
        OpenOptions { read: yes, ..self }
    }

    pub fn write(self, yes: bool) -> Self {
        OpenOptions { write: yes, ..self }
    }

    pub fn truncate(self, yes: bool) -> Self {
        OpenOptions {
            truncate: yes,
            ..self
        }
    }

    pub fn create(self, yes: bool) -> Self {
        OpenOptions {
            create: yes,
            ..self
        }
    }

    /// What a read-only backend refuses. `create` counts: it modifies the parent.
    pub fn intends_write(&self) -> bool {
        self.write || self.truncate || self.create || self.create_new
    }

    /// Reject the one self-contradictory combination.
    ///
    /// Not called in this crate: `decode_open_flags` rejects an invalid access mode itself.
    /// Stores never see these.
    pub fn validate(&self) -> io::Result<()> {
        if !self.read && !self.write {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    }
}

/// The attribute changes a `setattr` asks for; optional because the kernel's validity mask
/// names only the fields to change.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    /// The only field acted on, via [`FileSystem::truncate`].
    pub size: Option<u64>,
    pub mtime: Option<std::time::SystemTime>,
    pub atime: Option<std::time::SystemTime>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// FUSE's fixed inode number for the root directory.
const ROOT_INODE: u64 = 1;

/// Answer for an unknown file handle, or one used beyond what its open allowed.
///
/// Raw because `io::ErrorKind` has no name for it; the same on every POSIX system.
const EBADF: i32 = 9;

/// `EBADF`, as the [`io::Error`] this layer answers with.
///
/// Not [`NotFound`](io::ErrorKind::NotFound), which is about a name: a caller told "no such
/// file" for a closed descriptor retries the open forever.
fn bad_handle() -> io::Error {
    io::Error::from_raw_os_error(EBADF)
}

/// A store, in the terms a kernel speaks: inode numbers and file handles.
///
/// A path-addressed consumer (e.g. HTTP) uses the [`FileSystem`] directly.
///
/// The store answers only about names and bytes, so everything an open means lives here, where
/// no store names it:
///
/// * **Numbers**: inode ↔ path, and the kernel's reference counts that say when a number may be
///   reclaimed.
/// * **Descriptors**: a file handle resolves to an inode and its open options, never to a path,
///   so an open follows its file through a rename and outlives its name's unlink.
/// * **Decomposition**: `O_CREAT|O_EXCL` becomes [`create`](FileSystem::create), `O_TRUNC`
///   becomes [`truncate`](FileSystem::truncate), and the access mode is checked here rather
///   than in every store.
///
/// Fields are **private** so a binding can only translate: it cannot reach the tables to
/// re-derive an operation that belongs here.
pub struct Posix<T: FileSystem> {
    store: T,

    inodes: Mutex<InodeTable>,

    opens: Mutex<OpenTable>,

    /// Files unlinked while something had them open, by inode: the name each was moved aside
    /// to, waiting for its last handle to close. See [`unlink_child`](Self::unlink_child).
    held: Mutex<HashMap<u64, PathBuf>>,
}

impl<T: FileSystem> Posix<T> {
    pub fn new(store: T) -> Self {
        Posix {
            store,
            inodes: Mutex::new(InodeTable::new()),
            opens: Mutex::new(OpenTable::new()),
            held: Mutex::new(HashMap::new()),
        }
    }
}

/// The name a file unlinked while open is moved aside to, before its inode number.
///
/// It must live in the same directory, hence in the caller's namespace (as NFS's `.nfsXXXX`
/// does), so a *real* file with this prefix is hidden from listings.
const HELD_PREFIX: &str = ".virtx-unlinked-";

/// How long a kernel may cache a lookup or attribute reply; short because a write stays
/// invisible that long.
pub(in crate::fs) const TTL: Duration = Duration::from_secs(1);

/// Reported block size, for `st_blocks`/`st_blksize` and `statfs` only; stores have none.
pub(in crate::fs) const BLOCK_SIZE: u64 = 512;

/// Longest single path component reported by `statfs`.
pub(in crate::fs) const NAME_MAX: u32 = 255;

/// Synthetic `statfs` capacity, in [`BLOCK_SIZE`] blocks (1 TiB).
///
/// Stores have no capacity, but zero total blocks reads as *full* (`df` shows 100%,
/// installers refuse to run), so this is reported entirely free.
pub(in crate::fs) const TOTAL_BLOCKS: u64 = (1 << 40) / BLOCK_SIZE;

/// Synthetic inode budget; zero free inodes would fail every `create` up front.
pub(in crate::fs) const TOTAL_INODES: u64 = 1 << 32;

// The same on every POSIX system.
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// The attribute values every binding reports for one entry.
///
/// The foreign structs (`FileAttr`, `struct virtx_stat`, `FileInfo`) are built per binding, but
/// the numbers are derived once here so bindings cannot drift.
///
/// No `uid`/`gid`: a guest sees ids inside the VM, while a host mount must report the mounting
/// user's or that user cannot traverse it, so each binding decides.
pub(in crate::fs) struct Attr {
    pub size: u64,
    pub blocks: u64,
    pub blksize: u32,
    /// File type bits OR'd with the permission bits.
    pub mode: u32,
    pub nlink: u32,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    /// Birth time; only `fuser` and Dokan have a field for it (FUSE-T's `struct virtx_stat` has
    /// none).
    #[cfg_attr(
        not(all(feature = "mount", unix, not(target_os = "macos"))),
        allow(dead_code)
    )]
    pub crtime: SystemTime,
}

/// `st_mode` for an entry of this kind: type bits plus fixed permission bits.
pub(in crate::fs) fn mode_for(kind: DirentKind) -> u32 {
    match kind {
        DirentKind::Dir => S_IFDIR | 0o755,
        DirentKind::File => S_IFREG | 0o644,
    }
}

/// Translate a store error into the *host* kernel's errno numbering.
///
/// `raw_os_error` wins over [`kind`](io::Error::kind): a number came from this host's own
/// syscall (a passthrough `std::fs` call, or [`bad_handle`]), so it is already in host
/// numbering and more precise; `EBADF` and `ENOTBLK` have no `ErrorKind` and would become
/// `EIO`.
///
/// A binding read by a *guest* must use its own table instead (`ENOTEMPTY` is 66 on macOS,
/// 39 on Linux).
///
/// **`io::ErrorKind` is `#[non_exhaustive]`**, so a newly produced kind silently lands on `_`
/// as `EIO` instead of failing the build. A store answering with a kind not named here must
/// add it.
#[cfg(all(feature = "mount", unix))]
pub(in crate::fs) fn host_errno(err: &io::Error) -> i32 {
    if let Some(errno) = err.raw_os_error() {
        return errno;
    }
    match err.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::NotADirectory => libc::ENOTDIR,
        io::ErrorKind::IsADirectory => libc::EISDIR,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::DirectoryNotEmpty => libc::ENOTEMPTY,
        io::ErrorKind::InvalidFilename | io::ErrorKind::InvalidInput => libc::EINVAL,
        io::ErrorKind::FileTooLarge => libc::EFBIG,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::StorageFull => libc::ENOSPC,
        io::ErrorKind::ReadOnlyFilesystem => libc::EROFS,
        io::ErrorKind::CrossesDevices => libc::EXDEV,
        io::ErrorKind::Unsupported => libc::ENOSYS,
        io::ErrorKind::WriteZero => libc::EIO,
        _ => libc::EIO,
    }
}

/// Project a store [`Stat`] onto the attributes every binding reports.
///
/// Missing timestamps fall back to `mtime`, then the epoch. A real `mtime` matters: a guest
/// with `AUTO_INVAL_DATA` watches it to drop cached pages, so one stuck at 0 never
/// invalidates.
pub(in crate::fs) fn attr_for(stat: &Stat) -> Attr {
    // `find`/`du` read `nlink - 2` as a directory's subdir count and stop descending at
    // zero; `1` is the conventional "unreliable", disabling that.
    let nlink = 1;
    let mode = mode_for(stat.kind);
    let mtime = stat.mtime.unwrap_or(UNIX_EPOCH);
    Attr {
        size: stat.size,
        blocks: stat.size.div_ceil(BLOCK_SIZE),
        blksize: BLOCK_SIZE as u32,
        mode,
        nlink,
        mtime,
        atime: stat.atime.unwrap_or(mtime),
        ctime: stat.ctime.unwrap_or(mtime),
        crtime: stat.created.unwrap_or(mtime),
    }
}

/// Split a [`SystemTime`] into the `(seconds, nanoseconds)` a `stat` carries. Pre-epoch
/// times clamp rather than wrap into the future.
pub(in crate::fs) fn unix_time(time: SystemTime) -> (i64, i64) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => (since.as_secs() as i64, since.subsec_nanos() as i64),
        Err(_) => (0, 0),
    }
}

/// A binding kernel's values for the three non-portable open flags (`O_TRUNC` is `0o1000` on
/// Linux, `0o2000` on macOS). The access mode is portable and decoded once.
pub(in crate::fs) struct OpenFlagBits {
    pub truncate: i32,
    pub create: i32,
    pub create_new: i32,
}

/// Decode a POSIX open-flags word into [`OpenOptions`].
///
/// Rejects neither-read-nor-write here, the last place it can be, since stores never see
/// these options.
pub(in crate::fs) fn decode_open_flags(flags: i32, bits: &OpenFlagBits) -> io::Result<OpenOptions> {
    // `O_ACCMODE` is a two-bit *field* and `O_RDONLY` is 0, so a bitmask test would misread
    // `O_RDWR` as write-only; match it as a value.
    const O_ACCMODE: i32 = 3;
    const O_RDONLY: i32 = 0;
    const O_WRONLY: i32 = 1;
    const O_RDWR: i32 = 2;

    let (read, write) = match flags & O_ACCMODE {
        O_RDONLY => (true, false),
        O_WRONLY => (false, true),
        O_RDWR => (true, true),
        _ => return Err(io::ErrorKind::InvalidInput.into()),
    };
    Ok(OpenOptions {
        read,
        write,
        truncate: flags & bits.truncate != 0,
        create: flags & bits.create != 0,
        create_new: flags & bits.create_new != 0,
    })
}

/// The filesystem operations themselves, in terms the bindings share.
///
/// Each returns a plain [`io::Result`] over store types, leaving a binding only translation
/// (decode arguments, call one, encode the reply). The sequences are then shared and testable
/// on their own, which matters since `fuser`'s reply objects cannot be built outside its crate.
///
/// Methods take kernel inode/handle numbers and drop each table lock before touching the
/// (possibly slow) store.
impl<T: FileSystem> Posix<T> {
    /// Resolve `name` under `parent`, returning the child's inode and metadata.
    ///
    /// Takes a kernel reference, which [`forget`](InodeTable::forget) must balance (the
    /// `lookup` contract).
    pub(in crate::fs) async fn lookup_child(
        &self,
        parent: u64,
        name: &OsStr,
    ) -> io::Result<(u64, Stat)> {
        let parent_path = self.path_of(parent)?;
        let child = parent_path.join(name);
        let stat = self.store.stat(&child).await?;
        // Only mint an inode once the entry is known to exist.
        let inode = lock(&self.inodes).intern(child);
        Ok((inode, stat))
    }

    /// Metadata for an inode already known to the kernel.
    pub(in crate::fs) async fn stat_inode(&self, inode: u64) -> io::Result<Stat> {
        let path = self.path_of(inode)?;
        self.store.stat(&path).await
    }

    /// Apply what `options` ask of the store, record the open, and return its `fh`.
    ///
    /// **A plain read open does not touch the store**: the kernel has attributes from its
    /// `lookup` and nothing is acquired, which keeps opens cheap over a network store.
    ///
    /// No kind check: a kernel rejects write-opening a directory itself, and read-opening one
    /// is legal POSIX where the `read` fails, which [`read_handle`](Self::read_handle) gets
    /// from the store.
    ///
    /// Bindings decode `options` from their kernel's flags word (see [`decode_open_flags`]).
    pub(in crate::fs) async fn open_inode(
        &self,
        inode: u64,
        options: OpenOptions,
    ) -> io::Result<u64> {
        let path = self.path_of(inode)?;
        self.realize_open(&path, options).await?;
        Ok(lock(&self.opens).insert(Open { inode, options }))
    }

    /// Create — or open, if `options` allows — a child of `parent`, returning everything
    /// a `create` reply needs at once.
    pub(in crate::fs) async fn create_child(
        &self,
        parent: u64,
        name: &OsStr,
        options: OpenOptions,
    ) -> io::Result<(u64, Stat, u64)> {
        let path = self.path_of(parent)?.join(name);
        let stat = match self.realize_open(&path, options).await? {
            Some(stat) => stat,
            // The name already existed, so its metadata was not free.
            None => self.store.stat(&path).await?,
        };
        // `intern`, not `number_for`: a `create` reply carries an entry, which takes a kernel
        // reference; without it the inode would be evictable too early.
        let inode = lock(&self.inodes).intern(path);
        let fh = lock(&self.opens).insert(Open { inode, options });
        Ok((inode, stat, fh))
    }

    /// Apply an open to the store in POSIX order, returning any metadata that came free.
    ///
    /// `Some` is the stat of a file this call *created*; `None` means "not free", not
    /// "unknown", so opening an existing name costs the caller a `stat`.
    ///
    /// * Creation first, since `O_EXCL` is decided against the store alone.
    ///   [`create`](FileSystem::create) is exclusive, so a non-exclusive open accepts its
    ///   `AlreadyExists`.
    /// * Truncation second, only for a pre-existing name; a just-created file is already empty.
    async fn realize_open(&self, path: &Path, options: OpenOptions) -> io::Result<Option<Stat>> {
        let created = if options.create {
            match self.store.create(path).await {
                Ok(stat) => Some(stat),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && !options.create_new => {
                    None
                }
                Err(err) => return Err(err),
            }
        } else {
            None
        };
        if options.truncate && created.is_none() {
            self.store.truncate(path, 0).await?;
        }
        Ok(created)
    }

    /// Read the `(offset, size)` window; a short read is EOF, so the buffer is truncated to it.
    pub(in crate::fs) async fn read_handle(
        &self,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> io::Result<Vec<u8>> {
        let (path, options) = self.open_of(fh)?;
        if !options.read {
            return Err(bad_handle());
        }
        let mut buf = vec![0u8; size as usize];
        let n = self.store.read_at(&path, &mut buf, offset).await?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Write `data` at `offset` through `fh`, returning the byte count.
    ///
    /// Loops because a store's `write_at` may write short; one loop here spares every store a
    /// whole-buffer variant.
    ///
    /// `O_APPEND` never reaches here (the kernel sends the absolute offset), so write
    /// permission is the access mode alone.
    pub(in crate::fs) async fn write_handle(
        &self,
        fh: u64,
        offset: u64,
        data: &[u8],
    ) -> io::Result<usize> {
        let (path, options) = self.open_of(fh)?;
        if !options.write {
            return Err(bad_handle());
        }
        let mut written = 0usize;
        while written < data.len() {
            let n = self
                .store
                .write_at(&path, &data[written..], offset + written as u64)
                .await?;
            if n == 0 {
                // No progress on a non-empty buffer would loop forever.
                return Err(io::ErrorKind::WriteZero.into());
            }
            written += n;
        }
        Ok(written)
    }

    /// Create a subdirectory of `parent`, returning its inode and metadata.
    pub(in crate::fs) async fn mkdir_child(
        &self,
        parent: u64,
        name: &OsStr,
    ) -> io::Result<(u64, Stat)> {
        let path = self.path_of(parent)?.join(name);
        let stat = self.store.mkdir(&path).await?;
        // `intern`: the reply carries an entry, which takes a kernel reference.
        let inode = lock(&self.inodes).intern(path);
        Ok((inode, stat))
    }

    /// Remove the file named `name` under `parent`.
    ///
    /// **A file something has open is moved aside rather than removed**, and removed once its
    /// last handle closes. POSIX keeps an unlinked-but-open file usable through its descriptor
    /// (the basis of every tempfile), which a path-addressed store cannot do alone.
    ///
    /// It is renamed to [`HELD_PREFIX`] plus its inode number: in the same directory because a
    /// cross-mount rename is `EXDEV`, and by inode number because that is unique and never
    /// reused. The inode table is rekeyed onto it so the open handle's reads, writes and
    /// `getattr`s follow. The original name is free at once; a file created there is a
    /// different file with a different number.
    ///
    /// This is NFS's silly rename. Through FUSE-T's `nfs` backend the macOS client does it
    /// itself (a `.nfs<hex>` rename arrives instead of the `unlink`); this covers transports
    /// whose client does not (FSKit, kernel FUSE), so both behave alike. Inherited caveats:
    ///
    /// * **A held file can be left behind** if the mount goes away with a handle open.
    /// * **The hidden name is only hidden from listings** ([`dir_entries`](Self::dir_entries));
    ///   naming it directly still reaches it.
    ///
    /// Only *open* files are held, not names the kernel merely cached, so `rm` of an unopened
    /// file is one store call. A store that cannot rename gets a plain removal.
    pub(in crate::fs) async fn unlink_child(&self, parent: u64, name: &OsStr) -> io::Result<()> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);

        // Two statements, so `inodes` is released before `opens` is taken: nothing holds both.
        let numbered = lock(&self.inodes).number_of(&path);
        let held_by = numbered.filter(|&inode| lock(&self.opens).any_on(inode));

        if let Some(inode) = held_by {
            let aside = dir.join(format!("{HELD_PREFIX}{inode}"));
            match self.store.rename(&path, &aside).await {
                Ok(()) => {
                    // The kernel's number now means the hidden name; the original resolves
                    // to nothing.
                    lock(&self.inodes).rekey_subtree(&path, &aside);
                    lock(&self.held).insert(inode, aside);
                    return Ok(());
                }
                // The store cannot rename; the caller asked for a removal, so do that.
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::Unsupported
                    ) => {}
                Err(err) => return Err(err),
            }
        }

        self.store.unlink(&path).await?;
        // After the store agreed, so a failed removal keeps its mapping.
        lock(&self.inodes).evict_path(&path);
        Ok(())
    }

    /// Remove the empty directory named `name` under `parent`.
    pub(in crate::fs) async fn rmdir_child(&self, parent: u64, name: &OsStr) -> io::Result<()> {
        let path = self.path_of(parent)?.join(name);
        self.store.rmdir(&path).await?;
        lock(&self.inodes).evict_subtree(&path);
        Ok(())
    }

    /// Move `name` under `from_parent` to `to_name` under `to_parent`.
    ///
    /// The table is rekeyed only after the store agrees; an eager rekey would strand numbers on
    /// paths that never changed.
    pub(in crate::fs) async fn rename_child(
        &self,
        from_parent: u64,
        name: &OsStr,
        to_parent: u64,
        to_name: &OsStr,
    ) -> io::Result<()> {
        let from = self.path_of(from_parent)?.join(name);
        let to = self.path_of(to_parent)?.join(to_name);
        self.store.rename(&from, &to).await?;
        lock(&self.inodes).rekey_subtree(&from, &to);
        Ok(())
    }

    /// Apply a `setattr` and report the resulting metadata.
    ///
    /// Only `size` is acted on. Mode, ownership and timestamps are accepted and dropped, since
    /// nothing stores them and permissions are fixed; failing would break `cp -p`, `tar -x`
    /// and `touch`. The next `getattr` shows what stuck.
    ///
    /// No file handle: a resize names a path either way, and the quoted inode already is it.
    pub(in crate::fs) async fn setattr_inode(&self, inode: u64, attr: SetAttr) -> io::Result<Stat> {
        if let Some(size) = attr.size {
            let path = self.path_of(inode)?;
            self.store.truncate(&path, size).await?;
        }
        self.stat_inode(inode).await
    }

    /// Push `fh`'s writes out without ending it.
    ///
    /// Serves FLUSH (every `close()`) and FSYNC (mid-stream), so it must be repeatable and
    /// leave the handle usable.
    pub(in crate::fs) async fn flush_handle(&self, fh: u64) -> io::Result<()> {
        let (path, _) = self.open_of(fh)?;
        self.store.flush(&path).await
    }

    /// Drop the open `fh` names.
    ///
    /// Nothing is finalized: writes were durable on return, and the FLUSH preceding every
    /// RELEASE already asked for more. Only this layer's entry (and any held file) goes.
    ///
    /// An unknown handle is the kernel tidying up, and is ignored.
    pub(in crate::fs) async fn release_handle(&self, fh: u64) -> io::Result<()> {
        let Some(open) = lock(&self.opens).remove(fh) else {
            return Ok(());
        };
        // POSIX ties removal to the *last* handle; a `dup`ed one may still be open.
        if lock(&self.opens).any_on(open.inode) {
            return Ok(());
        }
        let Some(aside) = lock(&self.held).get(&open.inode).cloned() else {
            return Ok(());
        };
        self.store.unlink(&aside).await?;
        // After the store agreed, so a failed removal still knows the hidden name.
        lock(&self.held).remove(&open.inode);
        Ok(())
    }

    /// The entries a `readdir` of `inode` should stream, in order: `.`, `..`, then the
    /// store's children, each already assigned the inode number a later `lookup` will
    /// return.
    ///
    /// `..` reuses this directory's inode: traversal goes through `lookup`, so the real parent
    /// buys nothing.
    pub(in crate::fs) async fn dir_entries(&self, inode: u64) -> io::Result<Vec<(u64, Dirent)>> {
        let dir = self.path_of(inode)?;
        let children = self.store.list(&dir).await?;

        let mut entries = Vec::with_capacity(children.len() + 2);
        entries.push((inode, Dirent::new(".", DirentKind::Dir)));
        entries.push((inode, Dirent::new("..", DirentKind::Dir)));

        // One lock for the whole batch, so the numbers stay consistent.
        let mut inodes = lock(&self.inodes);
        for child in children {
            // Held aside by `unlink_child`; no longer part of the tree.
            if child.name.starts_with(HELD_PREFIX) {
                continue;
            }
            // `number_for`, not `intern`: readdir takes no kernel reference, but must agree
            // with a later `lookup`.
            let ino = inodes.number_for(dir.join(&child.name));
            entries.push((ino, child));
        }
        Ok(entries)
    }

    /// Stream `inode`'s entries to `emit`, resuming after `offset` and stopping once
    /// `emit` reports the consumer's buffer is full.
    ///
    /// The cursor protocol lives here so every binding agrees on it: 1-based offsets, resumed
    /// after the last one the kernel consumed.
    pub(in crate::fs) async fn for_each_dirent<E>(
        &self,
        inode: u64,
        offset: u64,
        mut emit: E,
    ) -> io::Result<()>
    where
        E: FnMut(u64, &Dirent, u64) -> io::Result<bool>,
    {
        for (position, (child_inode, child)) in self.dir_entries(inode).await?.iter().enumerate() {
            let cursor = position as u64 + 1;
            if cursor <= offset {
                continue;
            }
            if emit(*child_inode, child, cursor)? {
                break;
            }
        }
        Ok(())
    }

    /// Release the inode's kernel references, evicting it once none remain.
    pub(in crate::fs) fn forget_inode(&self, inode: u64, count: u64) {
        lock(&self.inodes).forget(inode, count);
    }

    /// The path behind an inode, or [`NotFound`](io::ErrorKind::NotFound) if forgotten or never
    /// issued. Drops the lock so callers never hold it across store work.
    fn path_of(&self, inode: u64) -> io::Result<PathBuf> {
        lock(&self.inodes)
            .path_of(inode)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    /// The path and options behind an `fh`, or [`bad_handle`] if it is closed.
    fn open_of(&self, fh: u64) -> io::Result<(PathBuf, OpenOptions)> {
        let open = lock(&self.opens).get(fh).ok_or_else(bad_handle)?;
        Ok((self.path_of(open.inode)?, open.options))
    }
}

/// One live inode: its path and outstanding kernel references (`lookup`s not yet balanced by
/// `forget`).
struct InodeData {
    path: PathBuf,

    lookup_count: u64,
}

/// The bidirectional inode<->path map plus a monotonic number allocator.
///
/// `next` only increases, so a number is never reused, even after being forgotten; live inodes
/// stay unique without generation churn, and u64 won't wrap in practice.
pub(in crate::fs) struct InodeTable {
    /// inode -> path + reference count; the authority every inode-only call resolves through.
    fwd: HashMap<u64, InodeData>,

    /// path -> inode, so a repeated `lookup` reuses its inode; a second one would break dedup
    /// by `st_ino`.
    rev: HashMap<PathBuf, u64>,

    next: u64,

    /// Numbers [`number_for`](Self::number_for) minted, oldest first, so unclaimed ones can be
    /// recycled.
    provisional: VecDeque<u64>,
}

/// Advertised-but-unclaimed numbers kept before recycling the oldest. At ~200 bytes each this
/// caps them near 13 MB, enough for a full walk of most repositories, within which `readdir`'s
/// numbers still match a later `lookup`.
const MAX_PROVISIONAL_INODES: usize = 64 * 1024;

impl InodeTable {
    fn new() -> Self {
        let root = PathBuf::from("/");
        let mut fwd = HashMap::new();
        fwd.insert(
            ROOT_INODE,
            InodeData {
                path: root.clone(),
                lookup_count: 1,
            },
        );
        let mut rev = HashMap::new();
        rev.insert(root, ROOT_INODE);
        InodeTable {
            fwd,
            rev,
            next: ROOT_INODE + 1,
            provisional: VecDeque::new(),
        }
    }

    /// The number `path` already has, without minting one.
    ///
    /// Answers "does the kernel know this name", so it must be able to say no.
    pub(in crate::fs) fn number_of(&self, path: &Path) -> Option<u64> {
        self.rev.get(path).copied()
    }

    /// The path an inode maps to, or `None` if forgotten or never issued.
    pub(in crate::fs) fn path_of(&self, inode: u64) -> Option<PathBuf> {
        self.fwd.get(&inode).map(|data| data.path.clone())
    }

    /// Drop the *name* → inode mapping for `path`, keeping the inode itself.
    ///
    /// A file later created at the path must get a different number, or the kernel's cache
    /// conflates the two.
    ///
    /// The forward entry stays: the kernel may still hold the number, which must not be reused
    /// before its `forget` arrives and reclaims it.
    ///
    /// For unopened files, and open ones a store cannot rename; any other open one is moved
    /// aside instead (see [`Posix::unlink_child`]).
    pub(in crate::fs) fn evict_path(&mut self, path: &Path) {
        self.rev.remove(path);
    }

    /// [`evict_path`](Self::evict_path) for `prefix` and everything beneath it.
    ///
    /// A successful `rmdir` means the *store* saw an empty directory, but this table may still
    /// hold descendants from an earlier listing, which a rebuilt subtree would resolve to.
    pub(in crate::fs) fn evict_subtree(&mut self, prefix: &Path) {
        self.rev.retain(|path, _| !path.starts_with(prefix));
    }

    /// Move the inode↔path mapping for `from`, and everything beneath it, onto `to`,
    /// keeping every number.
    ///
    /// Not an eviction: a renamed object is live, and the kernel keeps using its inode, so
    /// dropping the mapping would make its next `getattr` `ESTALE`.
    ///
    /// The destination's names are dropped first, as replaced, with `evict_subtree` because an
    /// earlier listing may have interned descendants the store no longer has.
    ///
    /// One operation, because evicting `to` separately would also wipe an overlapping source,
    /// leaving `fwd` populated and `rev` empty so the next `lookup` mints a duplicate number.
    pub(in crate::fs) fn rekey_subtree(&mut self, from: &Path, to: &Path) {
        // Overlapping moves do nothing: a backstop, since stores refuse them all (`EINVAL` into
        // a descendant, `ENOTEMPTY` the reverse, no-op for self-rename), and untouched is the
        // only safe answer. A root `from` is covered, as every `to` starts with `/`.
        if from == to || to.starts_with(from) || from.starts_with(to) {
            return;
        }
        self.evict_subtree(to);

        // `from` itself strips to `""`, and `to.join("")` would add a trailing separator that
        // `Path` equality ignores but `Debug` and error messages show.
        let rebase = |path: &Path| -> Option<PathBuf> {
            let rest = path.strip_prefix(from).ok()?;
            Some(if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            })
        };

        // The path is a field here, so rewrite in place...
        for data in self.fwd.values_mut() {
            if let Some(moved) = rebase(&data.path) {
                data.path = moved;
            }
        }
        // ...but a key here, so reinsert.
        let moved: Vec<(PathBuf, u64)> = self
            .rev
            .iter()
            .filter_map(|(path, &inode)| rebase(path).map(|moved| (moved, inode)))
            .collect();
        self.rev.retain(|path, _| !path.starts_with(from));
        self.rev.extend(moved);
    }

    /// Return the inode for `path`, allocating a fresh number the first time, and record
    /// one more kernel reference. Pairs with [`forget`](Self::forget).
    pub(in crate::fs) fn intern(&mut self, path: PathBuf) -> u64 {
        if let Some(&inode) = self.rev.get(&path) {
            if let Some(data) = self.fwd.get_mut(&inode) {
                data.lookup_count += 1;
                return inode;
            }
            // The maps drifted. Mint a new number rather than panic under the table's lock.
            self.rev.remove(&path);
        }
        let inode = self.next;
        self.next += 1;
        self.fwd.insert(
            inode,
            InodeData {
                path: path.clone(),
                lookup_count: 1,
            },
        );
        self.rev.insert(path, inode);
        inode
    }

    /// The inode `lookup` would assign to `path`, minting one if new, **without** taking a
    /// kernel reference, so `readdir`'s `ino`s match later `lookup`s.
    ///
    /// Entries start at `lookup_count == 0` and the kernel never `forget`s what it did not look
    /// up, so one `ls` of a large directory would strand an entry per child. Hence past
    /// [`MAX_PROVISIONAL_INODES`] the oldest unclaimed number is recycled.
    ///
    /// A recycled entry makes a later `lookup` answer a different number than `readdir`
    /// advertised, but numbers are never reused, so bindings can report `generation: 0`.
    pub(in crate::fs) fn number_for(&mut self, path: PathBuf) -> u64 {
        if let Some(&inode) = self.rev.get(&path) {
            return inode;
        }
        let inode = self.next;
        self.next += 1;
        self.fwd.insert(
            inode,
            InodeData {
                path: path.clone(),
                lookup_count: 0,
            },
        );
        self.rev.insert(path, inode);
        // One in, at most one out, so the queue stays at the cap.
        self.provisional.push_back(inode);
        if self.provisional.len() > MAX_PROVISIONAL_INODES
            && let Some(oldest) = self.provisional.pop_front()
        {
            self.reclaim_if_unreferenced(oldest);
        }
        inode
    }

    /// Drop `count` kernel references to `inode`, evicting it once none remain; unknown
    /// inodes are ignored.
    ///
    /// The root is exempt: the kernel holds it for the mount's life, and every path resolves
    /// through it.
    pub(in crate::fs) fn forget(&mut self, inode: u64, count: u64) {
        if inode == ROOT_INODE {
            return;
        }
        let Some(data) = self.fwd.get_mut(&inode) else {
            return;
        };
        data.lookup_count = data.lookup_count.saturating_sub(count);
        self.reclaim_if_unreferenced(inode);
    }

    /// Drop `inode` from both maps, if nothing holds it.
    fn reclaim_if_unreferenced(&mut self, inode: u64) {
        let Some(data) = self.fwd.get(&inode) else {
            return;
        };
        if data.lookup_count != 0 {
            return;
        }
        let path = data.path.clone();
        self.fwd.remove(&inode);
        // After `evict_path` another inode may own this path; removing its name would leave it
        // unreachable and the next lookup would mint a third.
        if self.rev.get(&path) == Some(&inode) {
            self.rev.remove(&path);
        }
    }
}

/// One open file: which inode it is an open *of*, and what it was opened for.
///
/// The inode, not the path, so a rename carries the open. The options because the access mode
/// is enforced here; a store cannot tell which descriptor asked.
#[derive(Clone, Copy)]
struct Open {
    inode: u64,

    options: OpenOptions,
}

/// The open-file table: one entry per file handle (`fh`) the kernel holds.
///
/// Entries are `Copy` and tiny, so a caller copies one out and drops the lock before slow
/// store I/O.
pub(in crate::fs) struct OpenTable {
    open: HashMap<u64, Open>,
    next: u64,
}

impl OpenTable {
    fn new() -> Self {
        // 0 is reserved as a "no handle" sentinel.
        OpenTable {
            open: HashMap::new(),
            next: 1,
        }
    }

    /// Register `open`, returning the `fh` the kernel quotes on later calls.
    fn insert(&mut self, open: Open) -> u64 {
        let fh = self.next;
        self.next += 1;
        self.open.insert(fh, open);
        fh
    }

    /// The entry for `fh`, if still open.
    fn get(&self, fh: u64) -> Option<Open> {
        self.open.get(&fh).copied()
    }

    /// Forget `fh`, returning what it opened so a caller can tell whether a held-aside file
    /// may now go.
    fn remove(&mut self, fh: u64) -> Option<Open> {
        self.open.remove(&fh)
    }

    /// Whether any open handle names `inode`.
    ///
    /// A linear scan: asked only once per `unlink` and `release`, which does not justify a
    /// second index to keep in step.
    fn any_on(&self, inode: u64) -> bool {
        self.open.values().any(|open| open.inode == inode)
    }

    /// How many handles are open, for checking that `release` keeps the table balanced.
    fn len(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any host's values serve; numbering is a binding's concern.
    const BITS: OpenFlagBits = OpenFlagBits {
        truncate: 0o1000,
        create: 0o100,
        create_new: 0o200,
    };

    /// `O_RDONLY | O_APPEND` is read-only: leaving `O_APPEND` unmodelled must not grant write,
    /// which [`Posix::write_handle`] enforces.
    #[test]
    fn an_append_style_open_is_not_permission_to_write() {
        const O_APPEND: i32 = 0o2000;
        let options = decode_open_flags(O_APPEND, &BITS).expect("`O_RDONLY` is an access mode");
        assert!(options.read && !options.write);
        assert!(!options.intends_write());
    }

    /// Each flag but `create_new` (whose constructor also sets `create`) is checked alone, since
    /// nothing else in the crate would notice a dropped term.
    #[test]
    fn every_flag_but_read_means_modification() {
        let ro = OpenOptions::read_only();
        assert!(!ro.intends_write(), "reading modifies nothing");
        for (flag, options) in [
            ("write", ro.write(true)),
            ("truncate", ro.truncate(true)),
            ("create", ro.create(true)),
            ("create_new", OpenOptions::create_new()),
        ] {
            assert!(options.intends_write(), "{flag}");
        }
    }
}
