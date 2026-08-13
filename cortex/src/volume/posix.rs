//! POSIX inode bookkeeping over a path-addressed [`Mountable`] backend.
//!
//! [`PosixFs`] maps the backend's paths onto stable inode numbers and
//! tracks the kernel's per-inode reference counts. It carries no FUSE/`msb_krun`
//! coupling of its own — a concrete filesystem binding (the sibling `krun`
//! module) drives it.
//!
//! This layer exists *because* a kernel addresses files by number. A
//! path-addressed consumer — [`Workspace`](crate::volume::Workspace), or an HTTP binding
//! whose verbs all carry paths — reaches [`Mountable`] directly and never comes
//! through here. So with no kernel binding compiled in, nothing reads any of it.

// Which items a build actually reads depends on which bindings are enabled, and
// the subsets do not line up: with no kernel binding nothing here is read at all;
// `unix_time` serves only the two bindings that fill a C `stat` (`fuser`'s
// `FileAttr` carries `SystemTime` directly); `TTL` serves only the two that pass a
// timeout through Rust (the FUSE-T shim spells the same value as `CORTEX_TTL`,
// because libfuse-t wants a `double` on the reply path).
//
// Gating each item by the set of bindings that happens to want it would encode an
// implementation detail that moves whenever a binding does — and would break the
// tests, which exercise every item regardless of features. So the module opts out
// wholesale, and coverage is what keeps it honest.
#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::lock::lock;
use crate::volume::{
    Dirent, DirentKind, FileExt, FileHandle, Mountable, OpenOptions, SetAttr, Stat,
};
use crate::{CortexError, Result};

/// FUSE's fixed inode number for the root directory.
const ROOT_INODE: u64 = 1;

/// Adapts a path-addressed [`Mountable`] into stable inode numbers with the
/// reference-counted bookkeeping a FUSE binding needs.
///
/// It owns the backend, the inode table (`InodeTable`) that translates the
/// kernel's inode numbers back into backend paths, and the handle table
/// (`HandleTable`) that keeps a backend handle alive for each open file.
/// The fields are **private**, and that is the enforcement mechanism for "a
/// binding only translates": an adapter cannot reach the tables, so it cannot
/// re-derive an operation that belongs here. While they were `pub(super)`, three
/// of krun's write callbacks had quietly grown their own handle lookups.
pub struct PosixFs<T: Mountable> {
    mountable: T,

    inodes: Mutex<InodeTable>,

    handles: Mutex<HandleTable<T::Handle>>,
}

impl<T: Mountable> PosixFs<T> {
    pub fn new(mountable: T) -> Self {
        PosixFs {
            mountable,
            inodes: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HandleTable::new()),
        }
    }
}

/// How long a kernel may cache a lookup or attribute reply — also the window in
/// which a write stays invisible, hence short.
pub(super) const TTL: Duration = Duration::from_secs(1);

/// Reported block size. Shapes only `st_blocks`/`st_blksize` and `statfs`; no
/// backend has a block notion of its own.
pub(super) const BLOCK_SIZE: u64 = 512;

/// Longest single path component reported by `statfs`.
pub(super) const NAME_MAX: u32 = 255;

/// Synthetic `statfs` capacity, in [`BLOCK_SIZE`] blocks (1 TiB).
///
/// No backend has a capacity to report, but "unknown" cannot be spelled as zero:
/// zero total blocks reads as *full*, so `df` shows 100% used and installers
/// refuse to run. Reported entirely free.
pub(super) const TOTAL_BLOCKS: u64 = (1 << 40) / BLOCK_SIZE;

/// Synthetic inode budget. Zero free inodes would mean every `create` fails
/// before it is attempted.
pub(super) const TOTAL_INODES: u64 = 1 << 32;

// Identical on every POSIX system, and `libc` is only an optional dependency.
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// The attribute values every binding reports for one entry.
///
/// The foreign attribute structs (`stat64`, `FileAttr`, `struct stat`) cannot be
/// built here, but the *numbers* can, and deriving them once is what keeps the
/// bindings from drifting.
///
/// `uid`/`gid` are absent on purpose: a guest sees the ids inside the VM, while a
/// host mount must report the mounting user's or that user cannot traverse their
/// own mount. Each binding decides those, as it decides its errno numbering.
pub(super) struct Attr {
    pub size: u64,
    pub blocks: u64,
    pub blksize: u32,
    /// File type bits OR'd with the permission bits.
    pub mode: u32,
    pub nlink: u32,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    /// Birth time. Only the host-FUSE binding has a field for it — Linux's
    /// `stat64`, which the guest reads, has no birth time at all.
    #[cfg_attr(not(feature = "fuse"), allow(dead_code))]
    pub crtime: SystemTime,
}

/// `st_mode` for an entry of this kind: type bits plus fixed permission bits.
pub(super) fn mode_for(kind: DirentKind) -> u32 {
    match kind {
        DirentKind::Dir => S_IFDIR | 0o755,
        DirentKind::File => S_IFREG | 0o644,
    }
}

/// Translate a backend error into the *host* kernel's errno numbering.
///
/// Shared by both host-side bindings; the krun binding keeps its own hardcoded
/// Linux table because its reader is always a Linux guest, whatever this host is.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
pub(super) fn host_errno(err: &CortexError) -> i32 {
    match err {
        CortexError::NotFound => libc::ENOENT,
        CortexError::NotADirectory => libc::ENOTDIR,
        CortexError::IsADirectory => libc::EISDIR,
        CortexError::AlreadyExists => libc::EEXIST,
        CortexError::NotEmpty => libc::ENOTEMPTY,
        CortexError::InvalidName | CortexError::InvalidArgument => libc::EINVAL,
        CortexError::FileTooLarge => libc::EFBIG,
        CortexError::BadHandle => libc::EBADF,
        CortexError::PermissionDenied => libc::EACCES,
        CortexError::NoSpace => libc::ENOSPC,
        CortexError::ReadOnly => libc::EROFS,
        CortexError::CrossDevice => libc::EXDEV,
        CortexError::Unsupported => libc::ENOSYS,
        // Unreachable: a mounted workspace is one whose volumes were all realized, so no
        // request reaching this binding can produce it. Classified rather than left to the
        // catch-all so that adding a variant stays a compile error here.
        CortexError::UnsupportedVolume(_) => libc::EIO,
        CortexError::Io(err) => err.raw_os_error().unwrap_or(libc::EIO),
    }
}

/// Project a backend [`Stat`] onto the attributes every binding reports.
///
/// Backend timestamps are `Option` (a store may know only an mtime, or none), so
/// each missing one falls back to `mtime` then the epoch. Reporting a real
/// `mtime` is functional, not cosmetic: a guest negotiating `AUTO_INVAL_DATA`
/// watches it to decide when to drop cached pages, so one stuck at 0 never has
/// its cache invalidated.
pub(super) fn attr_for(stat: &Stat) -> Attr {
    // A directory's real link count is `2 + subdirs`, and `find`/`du` read
    // `nlink - 2` as that count and stop descending at zero. `1` is the
    // conventional "unreliable", which turns that optimisation off.
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

/// Split a [`SystemTime`] into the `(seconds, nanoseconds)` a `stat` carries.
/// Pre-epoch times clamp rather than wrap into the future.
pub(super) fn unix_time(time: SystemTime) -> (i64, i64) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => (since.as_secs() as i64, since.subsec_nanos() as i64),
        Err(_) => (0, 0),
    }
}

/// The numeric values a binding's kernel uses for the four non-portable open
/// flags. `O_TRUNC` alone is `0o1000` on Linux and `0o2000` on macOS, so each
/// binding supplies its own — same split as the errno tables. The access mode is
/// portable and decoded once below.
pub(super) struct OpenFlagBits {
    pub append: i32,
    pub truncate: i32,
    pub create: i32,
    pub create_new: i32,
}

/// Decode a POSIX open-flags word into [`OpenOptions`].
pub(super) fn decode_open_flags(flags: i32, bits: &OpenFlagBits) -> Result<OpenOptions> {
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
        _ => return Err(CortexError::InvalidArgument),
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
/// Each returns plain [`Result`]s over backend types, so a binding is left with
/// nothing but translation: decode the kernel's arguments, call one of these,
/// encode the reply in whatever shape its interface wants. That keeps the two
/// bindings from re-deriving the same sequences — and makes the sequences
/// testable on their own, which matters because `fuser`'s reply objects cannot
/// be constructed outside its crate.
///
/// Every method takes the inode/handle numbers the kernel speaks in and drops
/// each table lock before touching the (possibly slow) backend.
impl<T: Mountable> PosixFs<T> {
    /// Resolve `name` under `parent`, returning the child's inode and metadata.
    ///
    /// Takes a kernel reference on the inode, so it must be balanced by
    /// [`forget`](InodeTable::forget) — that is the `lookup` contract.
    pub(super) async fn lookup_child(&self, parent: u64, name: &OsStr) -> Result<(u64, Stat)> {
        let parent_path = self.path_of(parent)?;
        let child = parent_path.join(name);
        let stat = self.mountable.stat(&child).await?;
        // Only mint an inode once the entry is known to exist.
        let inode = lock(&self.inodes).intern(child);
        Ok((inode, stat))
    }

    /// Metadata for an inode already known to the kernel.
    pub(super) async fn stat_inode(&self, inode: u64) -> Result<Stat> {
        let path = self.path_of(inode)?;
        self.mountable.stat(&path).await
    }

    /// Open `inode` and park the backend handle, returning the `fh` the kernel
    /// will quote on later reads plus the entry's metadata as of the open. The
    /// backend's expensive setup happens here, once, rather than per read.
    ///
    /// The options are the caller's: a binding decodes them from whatever flags
    /// word its kernel speaks (see [`decode_open_flags`]).
    pub(super) async fn open_inode(&self, inode: u64, options: OpenOptions) -> Result<(u64, Stat)> {
        let path = self.path_of(inode)?;
        let (handle, stat) = self.mountable.open(&path, options).await?;
        Ok((lock(&self.handles).insert(handle), stat))
    }

    /// Read the `(offset, size)` window the kernel asked for. A short read is
    /// EOF, so the returned buffer is only as long as what actually arrived.
    pub(super) async fn read_handle(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let file = self.handle_of(fh)?;
        let mut buf = vec![0u8; size as usize];
        let n = file.read_at(&mut buf, offset).await?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Write `data` at `offset` through `fh`, returning the byte count.
    pub(super) async fn write_handle(&self, fh: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let file = self.handle_of(fh)?;
        // `write_all_at`, not one `write_at`: a short write is legal and the
        // kernel would resend, but looping here spares both bindings that.
        file.write_all_at(data, offset).await?;
        Ok(data.len())
    }

    /// Create — or open, if `options` allows — a child of `parent`, returning
    /// everything a `create` reply needs at once.
    pub(super) async fn create_child(
        &self,
        parent: u64,
        name: &OsStr,
        options: OpenOptions,
    ) -> Result<(u64, Stat, u64)> {
        let path = self.path_of(parent)?.join(name);
        let (handle, stat) = self.mountable.open(&path, options).await?;
        // `intern`, not `number_for`: a `create` reply carries an entry, which
        // takes a kernel reference just as `lookup` does. The reference-free path
        // would leave the count short and the inode evictable too early.
        let inode = lock(&self.inodes).intern(path);
        let fh = lock(&self.handles).insert(handle);
        Ok((inode, stat, fh))
    }

    /// Create a subdirectory of `parent`, returning its inode and metadata.
    pub(super) async fn mkdir_child(&self, parent: u64, name: &OsStr) -> Result<(u64, Stat)> {
        let path = self.path_of(parent)?.join(name);
        self.mountable.mkdir(&path).await?;
        let stat = self.mountable.stat(&path).await?;
        // Reference-taking, for the same reason as `create_child`.
        let inode = lock(&self.inodes).intern(path);
        Ok((inode, stat))
    }

    /// Remove the file named `name` under `parent`.
    pub(super) async fn unlink_child(&self, parent: u64, name: &OsStr) -> Result<()> {
        let path = self.path_of(parent)?.join(name);
        self.mountable.unlink(&path).await?;
        // Only after the backend agreed: evicting first would strand the mapping
        // if the removal failed.
        lock(&self.inodes).evict_path(&path);
        Ok(())
    }

    /// Remove the empty directory named `name` under `parent`.
    pub(super) async fn rmdir_child(&self, parent: u64, name: &OsStr) -> Result<()> {
        let path = self.path_of(parent)?.join(name);
        self.mountable.rmdir(&path).await?;
        lock(&self.inodes).evict_subtree(&path);
        Ok(())
    }

    /// Move `name` under `from_parent` to `to_name` under `to_parent`.
    ///
    /// The table is rewritten only after the backend agrees, like every other
    /// mutation here — an eager rekey would strand numbers on paths that never
    /// changed. But unlike `unlink`/`rmdir` this *rewrites* rather than evicts: the
    /// object is still there under a new name, and the kernel goes on quoting the
    /// inode it was given, so dropping the mapping would turn its next `getattr`
    /// into `ESTALE`.
    pub(super) async fn rename_child(
        &self,
        from_parent: u64,
        name: &OsStr,
        to_parent: u64,
        to_name: &OsStr,
    ) -> Result<()> {
        let from = self.path_of(from_parent)?.join(name);
        let to = self.path_of(to_parent)?.join(to_name);
        self.mountable.rename(&from, &to).await?;
        lock(&self.inodes).rekey_subtree(&from, &to);
        Ok(())
    }

    /// Apply a `setattr` and report the resulting metadata.
    ///
    /// Only `size` is acted on. Mode, ownership, and timestamps are accepted and
    /// dropped — nothing stores them, and the attribute policy reports fixed
    /// permission bits. Failing instead would break `cp -p`, `tar -x`, and
    /// `touch` for no gain; the caller's next `getattr` shows what stuck.
    pub(super) async fn setattr_inode(
        &self,
        inode: u64,
        fh: Option<u64>,
        attr: SetAttr,
    ) -> Result<Stat> {
        if let Some(size) = attr.size {
            match fh {
                // Prefer the open handle: it may hold state a path alone cannot
                // reach (a range writer, an in-flight upload), and a `setattr`
                // carrying an `fh` is the kernel saying it has one.
                Some(fh) => self.handle_of(fh)?.truncate(size).await?,
                None => {
                    let path = self.path_of(inode)?;
                    let (handle, _) = self
                        .mountable
                        .open(&path, OpenOptions::read_write())
                        .await?;
                    handle.truncate(size).await?;
                }
            }
        }
        self.stat_inode(inode).await
    }

    /// Push `fh`'s buffered writes out without finalizing it.
    ///
    /// Serves both FLUSH and FSYNC. Not
    /// [`release_handle`](Self::release_handle): FLUSH arrives on every `close()`
    /// while RELEASE arrives only for the last, so finalizing here would cut off
    /// writes still coming through a `dup`ed descriptor.
    pub(super) async fn flush_handle(&self, fh: u64) -> Result<()> {
        self.handle_of(fh)?.flush().await
    }

    /// Drop the table's reference to `fh` and finalize it. Outstanding `Arc`
    /// clones from in-flight reads keep the handle alive until they finish.
    pub(super) async fn release_handle(&self, fh: u64) -> Result<()> {
        let file = lock(&self.handles).remove(fh);
        match file {
            // `commit`, not `flush`: this is the last descriptor, so a backend
            // that has been holding a multipart upload open may now complete it.
            Some(file) => file.commit().await,
            // A release for a handle we never issued is the kernel tidying up;
            // there is nothing to commit and nothing to complain about.
            None => Ok(()),
        }
    }

    /// The entries a `readdir` of `inode` should stream, in order: `.`, `..`,
    /// then the backend's children, each already assigned the inode number a
    /// later `lookup` will return.
    ///
    /// The kernel resumes a partial listing by quoting the last offset it
    /// consumed, and offsets here are the 1-based position in this vector, so a
    /// binding skips what it has already sent and emits the rest.
    ///
    /// `..` reuses this directory's inode: resolving the real parent buys
    /// nothing for traversal, which goes through `lookup`.
    pub(super) async fn dir_entries(&self, inode: u64) -> Result<Vec<(u64, Dirent)>> {
        let dir = self.path_of(inode)?;
        let children = self.mountable.list(&dir).await?;

        let mut entries = Vec::with_capacity(children.len() + 2);
        entries.push((inode, Dirent::new(".", DirentKind::Dir)));
        entries.push((inode, Dirent::new("..", DirentKind::Dir)));

        // One lock for the whole batch, so the numbers stay consistent.
        let mut inodes = lock(&self.inodes);
        for child in children {
            // `number_for`, not `intern`: readdir must not take a kernel
            // reference, only agree with what a later `lookup` would assign.
            let ino = inodes.number_for(dir.join(&child.name));
            entries.push((ino, child));
        }
        Ok(entries)
    }

    /// Stream `inode`'s entries to `emit`, resuming after `offset` and stopping
    /// once `emit` reports the consumer's buffer is full.
    ///
    /// The cursor protocol is here because every binding must agree on it
    /// exactly: offsets are 1-based positions in the listing, and the kernel
    /// resumes by quoting the last one it consumed.
    pub(super) async fn for_each_dirent<E>(
        &self,
        inode: u64,
        offset: u64,
        mut emit: E,
    ) -> Result<()>
    where
        E: FnMut(u64, &Dirent, u64) -> Result<bool>,
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
    pub(super) fn forget_inode(&self, inode: u64, count: u64) {
        lock(&self.inodes).forget(inode, count);
    }

    /// The path behind an inode, or [`CortexError::NotFound`] if we have
    /// forgotten it (or never issued it). Drops the lock before returning so
    /// callers never hold it across backend work.
    fn path_of(&self, inode: u64) -> Result<PathBuf> {
        lock(&self.inodes)
            .path_of(inode)
            .ok_or(CortexError::NotFound)
    }

    /// The open handle behind an `fh`, or [`CortexError::BadHandle`] if it is
    /// closed. A cheap `Arc` clone, so the lock is released immediately.
    ///
    /// `BadHandle`, not `NotFound`: the two mean different things to a caller, and
    /// getting it right here is what keeps every binding from having to patch it up
    /// on its own.
    fn handle_of(&self, fh: u64) -> Result<Arc<T::Handle>> {
        lock(&self.handles).get(fh).ok_or(CortexError::BadHandle)
    }
}

/// One live inode: the path it maps to and how many outstanding kernel
/// references (successful `lookup`s not yet balanced by `forget`) it holds.
struct InodeData {
    path: PathBuf,

    lookup_count: u64,
}

/// The bidirectional inode<->path map plus a monotonic number allocator.
///
/// `next` only ever increases, so a number is never reused even after its entry
/// is forgotten. That keeps every *live* inode unique within the mount and
/// sidesteps generation churn — u64 won't wrap in any realistic lifetime.
pub(super) struct InodeTable {
    /// inode -> path + reference count. The authority for "what does this inode
    /// mean"; every FUSE call that receives only an inode resolves through it.
    fwd: HashMap<u64, InodeData>,

    /// path -> inode, so a repeated `lookup` of the same path reuses its inode
    /// instead of minting a second one (which would break dedup by `st_ino`).
    rev: HashMap<PathBuf, u64>,

    /// The next number to hand out.
    next: u64,

    /// Numbers [`number_for`](Self::number_for) minted, oldest first, so the
    /// ones nothing ever claimed can be recycled.
    provisional: VecDeque<u64>,
}

/// How many advertised-but-unclaimed numbers to keep before recycling the
/// oldest. At roughly 200 bytes an entry this caps them near 13 MB, which holds
/// a full walk of most single repositories — inside it, the number `readdir`
/// advertised is still the one a later `lookup` returns.
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

    /// The path an inode maps to, or `None` if we've forgotten it (or never
    /// issued it).
    pub(super) fn path_of(&self, inode: u64) -> Option<PathBuf> {
        self.fwd.get(&inode).map(|data| data.path.clone())
    }

    /// Drop the *name* → inode mapping for `path`, keeping the inode itself.
    ///
    /// A file later created at the same path is a different file and must get a
    /// different number, or the kernel's cache conflates the two.
    ///
    /// The forward entry stays on purpose: the kernel may still hold references
    /// to an unlinked-but-open file, and POSIX requires `fstat` through the
    /// surviving descriptor to keep working. Dropping it would make
    /// [`path_of`](Self::path_of) fail and turn every such `getattr` into
    /// `ENOENT`, breaking essentially every tempfile implementation. It is
    /// reclaimed when its `forget` arrives, as it would have been anyway.
    pub(super) fn evict_path(&mut self, path: &Path) {
        self.rev.remove(path);
    }

    /// [`evict_path`](Self::evict_path) for `prefix` and everything beneath it.
    ///
    /// `rmdir` succeeding means the *backend* sees an empty directory; this table
    /// can still hold descendants interned by an earlier listing, and leaving
    /// them would let a rebuilt subtree resolve to the old numbers.
    pub(super) fn evict_subtree(&mut self, prefix: &Path) {
        self.rev.retain(|path, _| !path.starts_with(prefix));
    }

    /// Move the inode↔path mapping for `from`, and everything beneath it, onto
    /// `to`, keeping every number.
    ///
    /// Not an eviction, and that distinction is the whole point. `unlink` and
    /// `rmdir` may drop a mapping because the entry is *gone* and the kernel will
    /// never quote its number again. A rename moves a live object: the kernel
    /// updates its own dentry cache and goes on using the same inode, so dropping
    /// the mapping would turn its next `getattr` into `ESTALE`.
    ///
    /// The destination's own names are dropped first — whatever was there has been
    /// replaced. `evict_subtree` rather than [`evict_path`](Self::evict_path) for
    /// the same reason `rmdir` uses it: an earlier listing may have interned
    /// descendants the backend no longer sees.
    ///
    /// One operation rather than two, because two cannot be sequenced safely:
    /// evicting `to` first *also* wipes the source whenever the paths overlap,
    /// leaving `fwd` populated and `rev` empty — after which the next `lookup` mints
    /// a second number for a path the table already knew, which is exactly what
    /// `rev` exists to prevent.
    pub(super) fn rekey_subtree(&mut self, from: &Path, to: &Path) {
        // Overlapping moves do nothing. A backend refuses them all (`EINVAL` for a
        // directory into its own descendant, `ENOTEMPTY` for the reverse, a no-op
        // for a self-rename), so this is a backstop, not the rule — and leaving the
        // table untouched is the only safe answer when the two subtrees are not
        // disjoint. `from` being the root is covered for free: every path starts
        // with `/`, so `to` is always below it.
        if from == to || to.starts_with(from) || from.starts_with(to) {
            return;
        }
        self.evict_subtree(to);

        // Rebase a path that lives under `from`. `strip_prefix` yields `""` for
        // `from` itself, and `to.join("")` would append a separator — harmless,
        // since `Path` equality ignores one, but it makes every later `Debug` and
        // error message read wrong.
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

    /// Return the inode for `path`, allocating a fresh number the first time,
    /// and record one more kernel reference. Pairs with [`forget`](Self::forget).
    pub(super) fn intern(&mut self, path: PathBuf) -> u64 {
        if let Some(&inode) = self.rev.get(&path) {
            if let Some(data) = self.fwd.get_mut(&inode) {
                data.lookup_count += 1;
                return inode;
            }
            // The two maps drifted. Mint a new number rather than unwrapping:
            // a panic here happens under the table's lock, and see `crate::lock`
            // for why one such panic can take the whole mount with it.
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

    /// The inode number `lookup` would assign to `path`, allocating a fresh
    /// number if it's new — but **without** taking a kernel reference.
    ///
    /// `readdir` needs this: each listed child's `ino` must match the inode a
    /// later `lookup` returns, yet readdir (unlike lookup) must not bump the
    /// reference count.
    ///
    /// Entries start at `lookup_count == 0`, and the kernel never sends
    /// `forget` for an inode it did not look up, so nothing else would ever
    /// reclaim them — one `ls` of a large directory would strand an entry per
    /// child. Hence the queue: past [`MAX_PROVISIONAL_INODES`] the oldest
    /// unclaimed number is recycled.
    ///
    /// A recycled number means a later `lookup` of that path answers with a
    /// different one than `readdir` advertised. Numbers themselves are still
    /// never reused, which is what lets each binding report `generation: 0`.
    pub(super) fn number_for(&mut self, path: PathBuf) -> u64 {
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

    /// Drop `count` kernel references to `inode`, evicting it once none remain.
    /// A no-op for inodes we don't know (already forgotten, or never issued).
    ///
    /// The root is exempt: the kernel holds it for the life of the mount, and
    /// evicting it would strand every path that resolves through it.
    pub(super) fn forget(&mut self, inode: u64, count: u64) {
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
        // `evict_path` drops the name and keeps the entry, so a second inode may
        // hold this path by now. Taking its name would leave it live and
        // unreachable, and the next lookup would mint a third.
        if self.rev.get(&path) == Some(&inode) {
            self.rev.remove(&path);
        }
    }
}

/// The open-file table: one backend handle per FUSE file handle (`fh`).
///
/// Handles are kept behind `Arc` so a caller can clone one out, drop the table
/// lock, and then do the (possibly slow) backend I/O without blocking other
/// opens — and so concurrent reads on the same `fh` share one handle.
pub(super) struct HandleTable<H> {
    open: HashMap<u64, Arc<H>>,
    next: u64,
}

impl<H> HandleTable<H> {
    fn new() -> Self {
        // Start at 1; 0 is a convenient "no handle" sentinel.
        HandleTable {
            open: HashMap::new(),
            next: 1,
        }
    }

    /// Register `handle`, returning the `fh` the kernel will quote on later
    /// read/write/release calls.
    pub(super) fn insert(&mut self, handle: H) -> u64 {
        let fh = self.next;
        self.next += 1;
        self.open.insert(fh, Arc::new(handle));
        fh
    }

    /// The handle for `fh`, if still open (cheap `Arc` clone).
    pub(super) fn get(&self, fh: u64) -> Option<Arc<H>> {
        self.open.get(&fh).cloned()
    }

    /// Drop the table's reference to `fh`, returning the handle so the caller
    /// can flush/close it. Outstanding `Arc` clones keep it alive until done.
    pub(super) fn remove(&mut self, fh: u64) -> Option<Arc<H>> {
        self.open.remove(&fh)
    }
}

// Tests live beside this file rather than inside it: they had grown longer than
// the implementation, so a reader opening it had to scroll past them to find the
// code. They are still a child module, so private items stay reachable.
#[cfg(test)]
#[path = "posix_tests.rs"]
mod tests;
