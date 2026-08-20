//! POSIX bookkeeping over a path-addressed [`FileSystem`] store.
//!
//! [`Posix`] maps the store's paths onto stable inode numbers, tracks the kernel's
//! per-inode reference counts, and keeps the open-file table a file handle is an index
//! into. It carries no coupling to any one interface of its own — a concrete filesystem
//! binding drives it.
//!
//! This layer exists *because* a kernel addresses files by number, and by descriptor.
//! A path-addressed consumer — an HTTP binding whose verbs all carry paths — reaches
//! [`FileSystem`] directly and never comes through here.
//!
//! # What it holds that the store does not
//!
//! The store answers about names and bytes only (see *No opens, only paths* on
//! [`FileSystem`]), so everything an open means lives here:
//!
//! * **The numbers.** inode ↔ path, and the reference counts that say when a number
//!   may be reclaimed.
//! * **The descriptors.** A file handle resolves to an inode and the options it was
//!   opened with — never to a path directly, so an open follows its file through a
//!   rename the way a descriptor does.
//! * **The decomposition.** `O_CREAT|O_EXCL` becomes [`create`](FileSystem::create),
//!   `O_TRUNC` becomes [`truncate`](FileSystem::truncate), and the access mode becomes a check
//!   made here rather than in every store. [`OpenOptions`] and [`SetAttr`] are what a caller's
//!   open and a kernel's `setattr` say, and they stop here — a store sees neither.
//!
//! What it does *not* hold is a way for a descriptor to outlive its name. See
//! [`unlink_child`](Posix::unlink_child).

// Which items a build actually reads depends on which bindings are enabled, and the
// subsets do not line up: with no kernel binding compiled in nothing here is read at
// all; `unix_time` serves only the bindings that fill a C `stat`; `TTL` serves only the
// ones that pass a timeout through Rust.
//
// Gating each item by the set of bindings that happens to want it would encode an
// implementation detail that moves whenever a binding does, so the module opts out
// wholesale.
#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::fs::{Dirent, DirentKind, FileSystem, Stat};
use crate::lock::lock;

/// How a file should be opened.
///
/// The options travel *with* the open rather than being a separate `create`,
/// because two of them are atomicity requirements only the backend can meet:
///
/// * `create_new` is `O_EXCL`. Decomposing it into "stat, then create if absent"
///   is a race, not a contract — and for a local backend (`O_CREAT|O_EXCL`) or an
///   object store (`If-None-Match: *`) the atomic form is the only one there is.
/// * `truncate` must take effect *before* anything can observe the file, so the metadata the
///   caller gets back already reflects it as empty. [`FileSystem::truncate`](crate::fs::FileSystem::truncate) is the other,
///   non-atomic resize; a store must not treat one as the other.
///
/// The only meaningless combination — neither `read` nor `write` — is rejected by
/// [`validate`](Self::validate). `O_RDONLY | O_CREAT` is ordinary POSIX and stays
/// legal.
///
/// One is built the way `open(2)` is called: an access mode, then the flags that
/// modify it. The fields stay public because a backend has to read them, and
/// `#[non_exhaustive]` is what keeps a caller outside the crate from writing
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    /// Writes land at the end regardless of the offset given. Mostly inert under
    /// FUSE (the kernel resolves `O_APPEND` and sends absolute offsets), but a
    /// library caller means it, so a handle must honour rather than ignore it.
    pub append: bool,
    pub truncate: bool,
    /// Create the file if it is absent. The parent directory is never created.
    pub create: bool,
    /// Create the file, failing with [`AlreadyExists`] if it is already there.
    ///
    /// Meaningless without `create`, as `O_EXCL` is without `O_CREAT`. A backend
    /// decides *whether* to create from `create`, and reads this only inside that
    /// branch, to decide whether the creation has to be exclusive.
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

    /// Create a file that must not already exist, opened for reading and
    /// writing — `O_CREAT | O_EXCL | O_RDWR`, and the shape of
    /// [`File::create_new`](std::fs::File::create_new).
    ///
    /// Here because "make a new file" is the most common thing a caller wants
    /// and, without it, the only way to say it is a six-line struct update. The
    /// combination is also the one that has to be exclusive, so spelling it once
    /// keeps every caller on the atomic form.
    ///
    /// No setter pairs with it, because the name cannot be both. A combination
    /// wanting `O_EXCL` without both access modes narrows instead:
    /// `create_new().read(false)`.
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

    pub fn append(self, yes: bool) -> Self {
        OpenOptions {
            append: yes,
            ..self
        }
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

    /// What a read-only backend refuses on. `create` counts: it writes no bytes
    /// to the file, but modifies its parent.
    pub fn intends_write(&self) -> bool {
        self.write || self.append || self.truncate || self.create || self.create_new
    }

    /// Reject the one self-contradictory combination.
    ///
    /// Called by whatever decodes a flags word — [`decode_open_flags`](crate::fs::Posix), in
    /// practice — and not by a store, which never sees these at all.
    pub fn validate(&self) -> io::Result<()> {
        if !self.read && !self.write {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    }
}

/// The attribute changes a `setattr` asks for. Optional because the kernel sends
/// a validity mask: only the named fields are meant to move.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    /// Resize the file. The only field this crate can actually act on, via
    /// [`FileSystem::truncate`](crate::fs::FileSystem::truncate).
    pub size: Option<u64>,
    pub mtime: Option<std::time::SystemTime>,
    pub atime: Option<std::time::SystemTime>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// FUSE's fixed inode number for the root directory.
const ROOT_INODE: u64 = 1;

/// What a descriptor answers when asked to do what its open did not allow, and what an
/// unknown file handle answers.
///
/// Identical on every POSIX system, and `libc` is only an optional dependency. Built
/// as a raw number because `io::ErrorKind` has no name for it — see
/// [`host_errno`] for what a translating consumer does with that.
const EBADF: i32 = 9;

/// `EBADF`, as the [`io::Error`] this layer answers with.
///
/// Not [`NotFound`](io::ErrorKind::NotFound), which is about a *name*: a caller told
/// "no such file" for a closed descriptor retries the open forever, where "bad
/// descriptor" makes it fix its own bookkeeping.
fn bad_handle() -> io::Error {
    io::Error::from_raw_os_error(EBADF)
}

/// A store, in the terms a kernel speaks: inode numbers and file handles.
///
/// The name is the vocabulary, not an interface — nothing here is shaped by the binding
/// that drives it.
/// What a binding does with one is translate; see the module doc.
///
/// The fields are **private**, and that is the enforcement mechanism for "a binding only
/// translates": a binding cannot reach the tables, so it cannot re-derive an operation that
/// belongs here.
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
/// Distinctive on purpose, and the cost is stated plainly: a *real* file whose name starts with
/// this is hidden from a listing for as long as the prefix does. NFS made the same trade with
/// `.nfsXXXX`, for the same reason — the name has to live in the same directory, so it has to
/// live in the caller's namespace.
const HELD_PREFIX: &str = ".cortex-unlinked-";

/// How long a kernel may cache a lookup or attribute reply — also the window in which a
/// write stays invisible, hence short.
pub(in crate::fs) const TTL: Duration = Duration::from_secs(1);

/// Reported block size. Shapes only `st_blocks`/`st_blksize` and `statfs`; no store has
/// a block notion of its own.
pub(in crate::fs) const BLOCK_SIZE: u64 = 512;

/// Longest single path component reported by `statfs`.
pub(in crate::fs) const NAME_MAX: u32 = 255;

/// Synthetic `statfs` capacity, in [`BLOCK_SIZE`] blocks (1 TiB).
///
/// No store has a capacity to report, but "unknown" cannot be spelled as zero: zero
/// total blocks reads as *full*, so `df` shows 100% used and installers refuse to run.
/// Reported entirely free.
pub(in crate::fs) const TOTAL_BLOCKS: u64 = (1 << 40) / BLOCK_SIZE;

/// Synthetic inode budget. Zero free inodes would mean every `create` fails before it
/// is attempted.
pub(in crate::fs) const TOTAL_INODES: u64 = 1 << 32;

// Identical on every POSIX system, and `libc` is only an optional dependency.
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// The attribute values every binding reports for one entry.
///
/// The foreign attribute structs (`stat64`, `FileAttr`, `struct stat`) cannot be built
/// here, but the *numbers* can, and deriving them once is what keeps the bindings from
/// drifting.
///
/// `uid`/`gid` are absent on purpose: a guest sees the ids inside the VM, while a host
/// mount must report the mounting user's or that user cannot traverse their own mount.
/// Each binding decides those, as it decides its errno numbering.
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
    /// Birth time. Only the host-FUSE binding has a field for it — Linux's `stat64`,
    /// which the guest reads, has no birth time at all.
    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
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
/// `raw_os_error` first, and only then [`kind`](io::Error::kind). A number is here
/// because this host's own syscall produced it — a passthrough store's `std::fs` call,
/// or a [`bad_handle`] built above — so it is already in this table's numbering and is
/// more precise than the kind it was classified into: `EBADF` and `ENOTBLK` have no
/// `ErrorKind` at all, and would otherwise collapse onto `EIO`.
///
/// A binding whose reader is a *guest* must do the opposite and carry a table of its own:
/// the numbering that matters is the guest's, whatever this host is, and `ENOTEMPTY` is 66
/// on macOS against 39 on Linux.
///
/// **No exhaustive match is possible**, so the arms are a standing obligation rather
/// than a checked one. `io::ErrorKind` is `#[non_exhaustive]`: a kind this crate starts
/// producing does not fail to build here the way a new enum variant would — it lands on
/// the `_` arm and reaches userspace as `EIO`, which is a valid number and therefore a
/// silent wrong answer. A store that answers with a kind not named below has to add it.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
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
/// Store timestamps are `Option` (a store may know only an mtime, or none), so each
/// missing one falls back to `mtime` then the epoch. Reporting a real `mtime` is
/// functional, not cosmetic: a guest negotiating `AUTO_INVAL_DATA` watches it to decide
/// when to drop cached pages, so one stuck at 0 never has its cache invalidated.
pub(in crate::fs) fn attr_for(stat: &Stat) -> Attr {
    // A directory's real link count is `2 + subdirs`, and `find`/`du` read `nlink - 2`
    // as that count and stop descending at zero. `1` is the conventional "unreliable",
    // which turns that optimisation off.
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

/// The numeric values a binding's kernel uses for the four non-portable open flags.
/// `O_TRUNC` alone is `0o1000` on Linux and `0o2000` on macOS, so each binding supplies
/// its own — same split as the errno tables. The access mode is portable and decoded
/// once below.
pub(in crate::fs) struct OpenFlagBits {
    pub append: i32,
    pub truncate: i32,
    pub create: i32,
    pub create_new: i32,
}

/// Decode a POSIX open-flags word into [`OpenOptions`].
///
/// The one self-contradictory combination — neither read nor write — is rejected here,
/// which is the last place it can be: no store sees these options at all.
pub(in crate::fs) fn decode_open_flags(flags: i32, bits: &OpenFlagBits) -> io::Result<OpenOptions> {
    // `O_ACCMODE` is a two-bit *field*, not a bitmask, and `O_RDONLY` is 0, so
    // `flags & O_WRONLY != 0` would misread `O_RDWR` as write-only. Match it as a
    // value. Getting this wrong silently opens files with the wrong access.
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
        append: flags & bits.append != 0,
        truncate: flags & bits.truncate != 0,
        create: flags & bits.create != 0,
        create_new: flags & bits.create_new != 0,
    })
}

/// The filesystem operations themselves, in terms the bindings share.
///
/// Each returns a plain [`io::Result`] over store types, so a binding is left with
/// nothing but translation: decode the kernel's arguments, call one of these, encode the
/// reply in whatever shape its interface wants. That keeps the bindings from re-deriving
/// the same sequences — and makes the sequences testable on their own, which matters
/// because `fuser`'s reply objects cannot be constructed outside its crate.
///
/// Every method takes the inode/handle numbers the kernel speaks in and drops each table
/// lock before touching the (possibly slow) store.
impl<T: FileSystem> Posix<T> {
    /// Resolve `name` under `parent`, returning the child's inode and metadata.
    ///
    /// Takes a kernel reference on the inode, so it must be balanced by
    /// [`forget`](InodeTable::forget) — that is the `lookup` contract.
    pub(in crate::fs) async fn lookup_child(&self, parent: u64, name: &OsStr) -> io::Result<(u64, Stat)> {
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

    /// Record the open `inode` names and return the `fh` the kernel will quote on later
    /// reads, after applying whatever the options ask of the store.
    ///
    /// **A plain read open touches the store not at all.** The kernel already has the
    /// attributes from the `lookup` that got it here, nothing is acquired that would
    /// have to be released, and so there is nothing to ask for — which is what keeps an
    /// open cheap over a store that answers across a network.
    ///
    /// No kind check either. A kernel rejects a write-mode open of a directory itself,
    /// from the attributes it was given, and a *read* open of one is legal POSIX whose
    /// `read` is the thing that fails — which [`read_handle`](Self::read_handle) gets
    /// from the store for free.
    ///
    /// The options are the caller's: a binding decodes them from whatever flags word its
    /// kernel speaks (see [`decode_open_flags`]).
    pub(in crate::fs) async fn open_inode(&self, inode: u64, options: OpenOptions) -> io::Result<u64> {
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
            // The name was already there, so this is the one open that pays for its
            // metadata — see `realize_open`.
            None => self.store.stat(&path).await?,
        };
        // `intern`, not `number_for`: a `create` reply carries an entry, which takes a
        // kernel reference just as `lookup` does. The reference-free path would leave
        // the count short and the inode evictable too early.
        let inode = lock(&self.inodes).intern(path);
        let fh = lock(&self.opens).insert(Open { inode, options });
        Ok((inode, stat, fh))
    }

    /// What an open does to the store, in the order POSIX requires, and whatever
    /// metadata that produced for free.
    ///
    /// `Some` is the file this call *created*, reported by the store that made it.
    /// `None` is not "unknown" — it is "not free from this sequence", the same
    /// distinction [`Dirent::stat`] draws — and a caller that needs a [`Stat`] anyway
    /// pays for one, which is why an open of an existing name costs a round trip where
    /// creating a new one does not.
    ///
    /// The two steps are ordered, and both orderings matter:
    ///
    /// * Creation first, because `O_EXCL` is decided against what is on the store and
    ///   nothing else. [`create`](FileSystem::create) is exclusive, so a non-exclusive
    ///   open reads its `AlreadyExists` as the answer it wanted.
    /// * Truncation second, and only for a name that was already there: a file this call
    ///   just made is empty already, and asking a store to resize what it has just
    ///   reported as empty is a round trip for nothing.
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

    /// Read the `(offset, size)` window the kernel asked for. A short read is EOF, so
    /// the returned buffer is only as long as what actually arrived.
    pub(in crate::fs) async fn read_handle(&self, fh: u64, offset: u64, size: u32) -> io::Result<Vec<u8>> {
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
    /// The loop is here because a store's `write_at` may legally write less than it was
    /// given: the kernel would resend, but one loop above every store spares each of
    /// them a whole-buffer variant of its own — and spares the two from disagreeing
    /// about which one a consumer calls.
    ///
    /// `O_APPEND` is not resolved here. A kernel resolves it before the request arrives,
    /// sending the absolute offset it decided on; the flag is kept only because it is
    /// permission to write, which the check below reads.
    pub(in crate::fs) async fn write_handle(
        &self,
        fh: u64,
        offset: u64,
        data: &[u8],
    ) -> io::Result<usize> {
        let (path, options) = self.open_of(fh)?;
        if !options.write && !options.append {
            return Err(bad_handle());
        }
        let mut written = 0usize;
        while written < data.len() {
            let n = self
                .store
                .write_at(&path, &data[written..], offset + written as u64)
                .await?;
            if n == 0 {
                // A store reporting no progress on a non-empty buffer has no defined
                // meaning here, and looping on it would not terminate.
                return Err(io::ErrorKind::WriteZero.into());
            }
            written += n;
        }
        Ok(written)
    }

    /// Create a subdirectory of `parent`, returning its inode and metadata.
    pub(in crate::fs) async fn mkdir_child(&self, parent: u64, name: &OsStr) -> io::Result<(u64, Stat)> {
        let path = self.path_of(parent)?.join(name);
        // One call, not a `mkdir` followed by a `stat`: the store reports what it just
        // made, which it knows for free and a second request would have to ask for.
        let stat = self.store.mkdir(&path).await?;
        // Reference-taking, for the same reason as `create_child`.
        let inode = lock(&self.inodes).intern(path);
        Ok((inode, stat))
    }

    /// Remove the file named `name` under `parent`.
    ///
    /// **A file something has open is moved aside rather than removed**, and removed for real
    /// once its last handle closes. POSIX keeps an unlinked-but-open file readable, writable and
    /// `fstat`-able through its descriptor — the basis of every tempfile — and a store addressed
    /// by path cannot honour that on its own: the descriptor resolves to a path, and the path
    /// would be gone.
    ///
    /// So the name goes and the file does not. It is renamed to [`HELD_PREFIX`] plus its inode
    /// number, in the same directory — the same directory because a rename across mounts is
    /// `EXDEV`, and the inode number because it is unique and never reused, so the hidden name
    /// cannot collide. The inode table is rekeyed onto it, which is what makes every later read,
    /// write and `getattr` through the open handle find the file where it went. The original
    /// name is free immediately, and a file created there next is a different file with a
    /// different number.
    ///
    /// This is NFS's silly rename, for NFS's reason — and on one transport it is literally
    /// NFS's: measured through FUSE-T's `nfs` backend, the macOS client does this itself, so the
    /// `unlink` never arrives and a `.nfs<hex>` rename does. What is here is what answers on the
    /// transports whose client does not (FSKit, a kernel FUSE mount), which is why the two end
    /// up behaving alike.
    ///
    /// Two things it inherits from NFS:
    ///
    /// * **A held file can be left behind.** If the mount goes away while a handle is still
    ///   open, the hidden name stays in the store with nothing to clean it up.
    /// * **The hidden name is only hidden from listings** ([`dir_entries`](Self::dir_entries)
    ///   drops it). A caller that names it directly can still reach it.
    ///
    /// Only files something has open are held: whether the kernel merely *cached* the name is
    /// not the question, so an ordinary `rm` of an unopened file is one store call as before.
    /// That is the difference between this and a table of every name the kernel ever looked up.
    ///
    /// A store that cannot rename cannot hold anything aside either, so it gets the plain
    /// removal and the old behaviour with it.
    pub(in crate::fs) async fn unlink_child(&self, parent: u64, name: &OsStr) -> io::Result<()> {
        let dir = self.path_of(parent)?;
        let path = dir.join(name);

        // Two statements, not one: `open_of` takes `opens` and then `inodes`, so a lookup
        // holding `inodes` while it reaches for `opens` is the one ordering that can deadlock.
        let numbered = lock(&self.inodes).number_of(&path);
        let held_by = numbered.filter(|&inode| lock(&self.opens).any_on(inode));

        if let Some(inode) = held_by {
            let aside = dir.join(format!("{HELD_PREFIX}{inode}"));
            match self.store.rename(&path, &aside).await {
                Ok(()) => {
                    // The number the kernel is holding now means the hidden name, and the
                    // original resolves to nothing — both from this one rewrite.
                    lock(&self.inodes).rekey_subtree(&path, &aside);
                    lock(&self.held).insert(inode, aside);
                    return Ok(());
                }
                // Not a failure to report: the store simply cannot do this, and the caller
                // asked for a removal. It gets one, and loses only what it never had.
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::Unsupported
                    ) => {}
                Err(err) => return Err(err),
            }
        }

        self.store.unlink(&path).await?;
        // Only after the store agreed: evicting first would strand the mapping if the removal
        // failed.
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
    /// The table is rewritten only after the store agrees, like every other mutation
    /// here — an eager rekey would strand numbers on paths that never changed. But
    /// unlike `unlink`/`rmdir` this *rewrites* rather than evicts: the object is still
    /// there under a new name, and the kernel goes on quoting the inode it was given, so
    /// dropping the mapping would turn its next `getattr` into `ESTALE`.
    ///
    /// Open handles follow, and get that for free: a handle resolves to an inode, and
    /// the inode's path is what moves.
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
    /// Only `size` is acted on. Mode, ownership, and timestamps are accepted and dropped
    /// — nothing stores them, and the attribute policy reports fixed permission bits.
    /// Failing instead would break `cp -p`, `tar -x`, and `touch` for no gain; the
    /// caller's next `getattr` shows what stuck.
    ///
    /// No file handle argument, unlike the calls above. A `setattr` may carry one, but a
    /// resize names a path either way, and the inode the kernel quotes alongside it is
    /// already that path.
    pub(in crate::fs) async fn setattr_inode(&self, inode: u64, attr: SetAttr) -> io::Result<Stat> {
        if let Some(size) = attr.size {
            let path = self.path_of(inode)?;
            self.store.truncate(&path, size).await?;
        }
        self.stat_inode(inode).await
    }

    /// Push `fh`'s writes out without ending it.
    ///
    /// Serves both FLUSH and FSYNC, which arrive on every `close()` and mid-stream
    /// respectively, so it must be repeatable and must leave the handle usable.
    pub(in crate::fs) async fn flush_handle(&self, fh: u64) -> io::Result<()> {
        let (path, _) = self.open_of(fh)?;
        self.store.flush(&path).await
    }

    /// Drop the open `fh` names.
    ///
    /// Nothing is finalized, because nothing was held: a store's writes were durable
    /// when they returned, and the FLUSH that precedes every RELEASE has already asked
    /// for whatever more it could. What is released is this layer's own entry.
    ///
    /// A release for a handle we never issued is the kernel tidying up; there is nothing
    /// to drop and nothing to complain about.
    pub(in crate::fs) async fn release_handle(&self, fh: u64) -> io::Result<()> {
        let Some(open) = lock(&self.opens).remove(fh) else {
            return Ok(());
        };
        // The *last* handle, which is what POSIX ties the removal to — a `dup`ed descriptor
        // still open here means the file is still being used.
        if lock(&self.opens).any_on(open.inode) {
            return Ok(());
        }
        let Some(aside) = lock(&self.held).get(&open.inode).cloned() else {
            return Ok(());
        };
        self.store.unlink(&aside).await?;
        // Only after the store agreed, as everywhere else here: forgetting the hidden name
        // first would leave a failed removal with nothing left that knows what to remove.
        lock(&self.held).remove(&open.inode);
        Ok(())
    }

    /// The entries a `readdir` of `inode` should stream, in order: `.`, `..`, then the
    /// store's children, each already assigned the inode number a later `lookup` will
    /// return.
    ///
    /// The kernel resumes a partial listing by quoting the last offset it consumed, and
    /// offsets here are the 1-based position in this vector, so a binding skips what it
    /// has already sent and emits the rest.
    ///
    /// `..` reuses this directory's inode: resolving the real parent buys nothing for
    /// traversal, which goes through `lookup`.
    pub(in crate::fs) async fn dir_entries(&self, inode: u64) -> io::Result<Vec<(u64, Dirent)>> {
        let dir = self.path_of(inode)?;
        let children = self.store.list(&dir).await?;

        let mut entries = Vec::with_capacity(children.len() + 2);
        entries.push((inode, Dirent::new(".", DirentKind::Dir)));
        entries.push((inode, Dirent::new("..", DirentKind::Dir)));

        // One lock for the whole batch, so the numbers stay consistent.
        let mut inodes = lock(&self.inodes);
        for child in children {
            // Held aside by an `unlink` that had to keep the file: it is not in the tree any
            // more, so it is not in a listing of it either.
            if child.name.starts_with(HELD_PREFIX) {
                continue;
            }
            // `number_for`, not `intern`: readdir must not take a kernel reference, only
            // agree with what a later `lookup` would assign.
            let ino = inodes.number_for(dir.join(&child.name));
            entries.push((ino, child));
        }
        Ok(entries)
    }

    /// Stream `inode`'s entries to `emit`, resuming after `offset` and stopping once
    /// `emit` reports the consumer's buffer is full.
    ///
    /// The cursor protocol is here because every binding must agree on it exactly:
    /// offsets are 1-based positions in the listing, and the kernel resumes by quoting
    /// the last one it consumed.
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

    /// The path behind an inode, or [`NotFound`](io::ErrorKind::NotFound) if we have
    /// forgotten it (or never issued it). Drops the lock before returning so callers
    /// never hold it across store work.
    fn path_of(&self, inode: u64) -> io::Result<PathBuf> {
        lock(&self.inodes)
            .path_of(inode)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    /// The path and options behind an `fh`, or [`bad_handle`] if it is closed.
    ///
    /// Resolved through the *inode*, never from a path recorded at open time. That is
    /// what makes an open follow its file: a rename rewrites the inode's path, and the
    /// next read through this handle finds the file where it went.
    fn open_of(&self, fh: u64) -> io::Result<(PathBuf, OpenOptions)> {
        let open = lock(&self.opens).get(fh).ok_or_else(bad_handle)?;
        Ok((self.path_of(open.inode)?, open.options))
    }
}

/// One live inode: the path it maps to and how many outstanding kernel references
/// (successful `lookup`s not yet balanced by `forget`) it holds.
struct InodeData {
    path: PathBuf,

    lookup_count: u64,
}

/// The bidirectional inode<->path map plus a monotonic number allocator.
///
/// `next` only ever increases, so a number is never reused even after its entry is
/// forgotten. That keeps every *live* inode unique within the mount and sidesteps
/// generation churn — u64 won't wrap in any realistic lifetime.
pub(in crate::fs) struct InodeTable {
    /// inode -> path + reference count. The authority for "what does this inode mean";
    /// every FUSE call that receives only an inode resolves through it.
    fwd: HashMap<u64, InodeData>,

    /// path -> inode, so a repeated `lookup` of the same path reuses its inode instead
    /// of minting a second one (which would break dedup by `st_ino`).
    rev: HashMap<PathBuf, u64>,

    /// The next number to hand out.
    next: u64,

    /// Numbers [`number_for`](Self::number_for) minted, oldest first, so the ones
    /// nothing ever claimed can be recycled.
    provisional: VecDeque<u64>,
}

/// How many advertised-but-unclaimed numbers to keep before recycling the oldest. At
/// roughly 200 bytes an entry this caps them near 13 MB, which holds a full walk of most
/// single repositories — inside it, the number `readdir` advertised is still the one a
/// later `lookup` returns.
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
    /// [`number_for`](Self::number_for) is the other spelling and mints; this one answers "does
    /// the kernel know this name" and has to be able to say no.
    pub(in crate::fs) fn number_of(&self, path: &Path) -> Option<u64> {
        self.rev.get(path).copied()
    }

    /// The path an inode maps to, or `None` if we've forgotten it (or never issued it).
    pub(in crate::fs) fn path_of(&self, inode: u64) -> Option<PathBuf> {
        self.fwd.get(&inode).map(|data| data.path.clone())
    }

    /// Drop the *name* → inode mapping for `path`, keeping the inode itself.
    ///
    /// A file later created at the same path is a different file and must get a
    /// different number, or the kernel's cache conflates the two.
    ///
    /// The forward entry stays on purpose: the kernel may still hold references to the removed
    /// file and will send its `forget` eventually, and a number it is holding must not be handed
    /// to something else in the meantime. It is reclaimed when that `forget` arrives, as it
    /// would have been anyway.
    ///
    /// This is the path for a file nothing had open. One that was open is *moved* rather than
    /// removed and so keeps both mappings — see [`Posix::unlink_child`].
    pub(in crate::fs) fn evict_path(&mut self, path: &Path) {
        self.rev.remove(path);
    }

    /// [`evict_path`](Self::evict_path) for `prefix` and everything beneath it.
    ///
    /// `rmdir` succeeding means the *store* sees an empty directory; this table can still
    /// hold descendants interned by an earlier listing, and leaving them would let a
    /// rebuilt subtree resolve to the old numbers.
    pub(in crate::fs) fn evict_subtree(&mut self, prefix: &Path) {
        self.rev.retain(|path, _| !path.starts_with(prefix));
    }

    /// Move the inode↔path mapping for `from`, and everything beneath it, onto `to`,
    /// keeping every number.
    ///
    /// Not an eviction, and that distinction is the whole point. `unlink` and `rmdir` may
    /// drop a mapping because the entry is *gone* and the kernel will never quote its
    /// number again. A rename moves a live object: the kernel updates its own dentry
    /// cache and goes on using the same inode, so dropping the mapping would turn its
    /// next `getattr` into `ESTALE`.
    ///
    /// The destination's own names are dropped first — whatever was there has been
    /// replaced. `evict_subtree` rather than [`evict_path`](Self::evict_path) for the
    /// same reason `rmdir` uses it: an earlier listing may have interned descendants the
    /// store no longer sees.
    ///
    /// One operation rather than two, because two cannot be sequenced safely: evicting
    /// `to` first *also* wipes the source whenever the paths overlap, leaving `fwd`
    /// populated and `rev` empty — after which the next `lookup` mints a second number
    /// for a path the table already knew, which is exactly what `rev` exists to prevent.
    pub(in crate::fs) fn rekey_subtree(&mut self, from: &Path, to: &Path) {
        // Overlapping moves do nothing. A store refuses them all (`EINVAL` for a
        // directory into its own descendant, `ENOTEMPTY` for the reverse, a no-op for a
        // self-rename), so this is a backstop, not the rule — and leaving the table
        // untouched is the only safe answer when the two subtrees are not disjoint.
        // `from` being the root is covered for free: every path starts with `/`, so `to`
        // is always below it.
        if from == to || to.starts_with(from) || from.starts_with(to) {
            return;
        }
        self.evict_subtree(to);

        // Rebase a path that lives under `from`. `strip_prefix` yields `""` for `from`
        // itself, and `to.join("")` would append a separator — harmless, since `Path`
        // equality ignores one, but it makes every later `Debug` and error message read
        // wrong.
        let rebase = |path: &Path| -> Option<PathBuf> {
            let rest = path.strip_prefix(from).ok()?;
            Some(if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            })
        };

        // The path is a *field* here, so rewriting it in place keeps the number.
        for data in self.fwd.values_mut() {
            if let Some(moved) = rebase(&data.path) {
                data.path = moved;
            }
        }
        // ...and the *key* there, so the entries have to be reinserted.
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
            // The two maps drifted. Mint a new number rather than unwrapping: a panic
            // here happens under the table's lock, and see `crate::lock` for why one
            // such panic can take the whole mount with it.
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

    /// The inode number `lookup` would assign to `path`, allocating a fresh number if
    /// it's new — but **without** taking a kernel reference.
    ///
    /// `readdir` needs this: each listed child's `ino` must match the inode a later
    /// `lookup` returns, yet readdir (unlike lookup) must not bump the reference count.
    ///
    /// Entries start at `lookup_count == 0`, and the kernel never sends `forget` for an
    /// inode it did not look up, so nothing else would ever reclaim them — one `ls` of a
    /// large directory would strand an entry per child. Hence the queue: past
    /// [`MAX_PROVISIONAL_INODES`] the oldest unclaimed number is recycled.
    ///
    /// A recycled number means a later `lookup` of that path answers with a different
    /// one than `readdir` advertised. Numbers themselves are still never reused, which is
    /// what lets each binding report `generation: 0`.
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
        // One in, at most one out, so the queue never passes the cap.
        self.provisional.push_back(inode);
        if self.provisional.len() > MAX_PROVISIONAL_INODES
            && let Some(oldest) = self.provisional.pop_front()
        {
            self.reclaim_if_unreferenced(oldest);
        }
        inode
    }

    /// Drop `count` kernel references to `inode`, evicting it once none remain. A no-op
    /// for inodes we don't know (already forgotten, or never issued).
    ///
    /// The root is exempt: the kernel holds it for the life of the mount, and evicting it
    /// would strand every path that resolves through it.
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
        // `evict_path` drops the name and keeps the entry, so a second inode may hold
        // this path by now. Taking its name would leave it live and unreachable, and the
        // next lookup would mint a third.
        if self.rev.get(&path) == Some(&inode) {
            self.rev.remove(&path);
        }
    }
}

/// One open file: which inode it is an open *of*, and what it was opened for.
///
/// The inode rather than the path, so a rename carries the open with it. The options
/// rather than nothing, because the access mode is enforced here now: a store sees a
/// read and a write and has no way to know which descriptor asked.
#[derive(Clone, Copy)]
struct Open {
    inode: u64,

    options: OpenOptions,
}

/// The open-file table: one entry per file handle (`fh`) the kernel holds.
///
/// Entries are `Copy` and tiny, so a caller takes one out, drops the lock, and then does
/// the (possibly slow) store I/O without blocking other opens.
pub(in crate::fs) struct OpenTable {
    open: HashMap<u64, Open>,
    next: u64,
}

impl OpenTable {
    fn new() -> Self {
        // Start at 1; 0 is a convenient "no handle" sentinel.
        OpenTable {
            open: HashMap::new(),
            next: 1,
        }
    }

    /// Register `open`, returning the `fh` the kernel will quote on later
    /// read/write/release calls.
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

    /// Forget `fh`, and say what it was an open of — which is what tells a caller whether a
    /// file held aside by an `unlink` may now go.
    fn remove(&mut self, fh: u64) -> Option<Open> {
        self.open.remove(&fh)
    }

    /// Whether any open handle names `inode`.
    ///
    /// A scan, over one entry per open file handle. The alternative is a second index kept in
    /// step with this one, for a question asked once per `unlink` and once per `release`.
    fn any_on(&self, inode: u64) -> bool {
        self.open.values().any(|open| open.inode == inode)
    }

    /// How many handles are open — the invariant a `release` is supposed to keep.
    fn len(&self) -> usize {
        self.open.len()
    }
}
