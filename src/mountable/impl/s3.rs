//! A read-only [`Mountable`] backend over an object store (S3 and compatibles).
//!
//! Object keys map to paths and a directory is a key prefix — that is the whole of
//! the mapping. There is no directory object to create or remove; two keys sharing
//! a prefix *are* a directory. Metadata comes from `head`, listings from
//! `list_with_delimiter`, and file data from ranged GETs on an open handle.
//!
//! # Read-only, and why that is a design rather than an omission
//!
//! [`FileExt::write_at`] addresses a byte offset. Answering that over S3 means
//! reading the whole object, patching it, and putting it back — `object_store` does
//! not expose a byte-range patch, and its multipart API requires parts of at least
//! 5 MiB where a guest's writes are at most 1 MiB. So a write needs staging for as
//! long as a handle is open, with a ceiling on it (a guest picks the offset, so an
//! allocation sized from one is a guest-chosen allocation — see
//! [`CortexError::FileTooLarge`]). That is a separate piece of work; every write
//! here answers [`ReadOnly`](CortexError::ReadOnly).
//!
//! `ReadOnly` and not [`Unsupported`](CortexError::Unsupported): the store *could*
//! write, it is this backend that will not, and userspace acts on the difference.
//!
//! # The runtime, and where you may not call this
//!
//! `object_store` is async and [`Mountable`] is not, so each operation blocks on a
//! Tokio runtime. That is safe for the callers that reach a backend today — the
//! FUSE and krun bindings drive it on threads that exist in order to block.
//!
//! **It is not safe from a thread that is already driving a Tokio runtime**: a
//! `block_on` there panics with "Cannot start a runtime from within a runtime". A
//! future async consumer (an HTTP handler, say) must bridge — `block_in_place` on a
//! multi-thread runtime, `spawn_blocking` otherwise — before touching any method on
//! this type.
//!
//! The runtime is one per crate rather than one per volume: `block_on` occupies the
//! calling thread either way, so a worker pool is only worth anything to tasks
//! spawned inside it, while `Runtime::new` costs a worker per logical core — three
//! mounted volumes measured 37 threads against a shared runtime's 12.

use std::collections::BTreeSet;
use std::io;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, OnceLock};

use object_store::aws::AmazonS3Builder;
use object_store::path::Path as OsPath;
use object_store::{GetOptions, GetRange, ObjectMeta, ObjectStore, ObjectStoreExt};
use tokio::runtime::Runtime;

use crate::lock::lock;
use crate::mountable::{FileExt, FileHandle};
use crate::{CortexError, Dirent, DirentKind, Mountable, OpenOptions, Result, Stat};

/// The one Tokio runtime this crate drives object-store calls on.
///
/// Fallible rather than a `static` initialiser: building a runtime spawns threads
/// and can fail, and a volume failing to open is not a reason to take the process
/// down. `OnceLock::get_or_try_init` would say this in one call but is unstable, so
/// the fallible build happens outside the cell and a lost race simply drops the
/// runtime it built — safe, and at most once per process.
fn runtime() -> Result<&'static Runtime> {
    static RT: OnceLock<Runtime> = OnceLock::new();
    if let Some(rt) = RT.get() {
        return Ok(rt);
    }
    let built = Runtime::new()?;
    Ok(RT.get_or_init(|| built))
}

/// Connection settings for [`S3Volume`].
///
/// [`Debug`] is written by hand rather than derived, and it redacts
/// `secret_access_key`. Deriving it would put the key in plain text into whatever
/// `{:?}` reaches — a log line, a panic message, an error wrapping this config — and
/// nothing about the derive would announce that.
///
/// The built store needs no such care from us, though it is also printable:
/// `ObjectStore` requires `Debug` and `AmazonS3` derives it, but `AwsCredential`
/// writes its own that prints `"******"` for the secret and the session token.
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Custom endpoint (MinIO / R2 / localstack); `None` for real AWS.
    pub endpoint: Option<String>,
    /// Key prefix every path is rooted under. Composes with whatever mount path a
    /// [`Workspace`](crate::Workspace) puts this volume at: the workspace strips
    /// its mount path first, then this prefix is prepended.
    pub key_prefix: Option<String>,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[redacted]")
            .field("endpoint", &self.endpoint)
            .field("key_prefix", &self.key_prefix)
            .finish()
    }
}

/// A read-only volume over an object store.
pub struct S3Volume {
    store: Arc<dyn ObjectStore>,
    /// Normalised to carry no leading or trailing `/`, so [`Self::key`] can join
    /// with `/` unconditionally.
    prefix: String,
}

impl S3Volume {
    /// Build an S3 client from `cfg`.
    pub fn new(cfg: &S3Config) -> Result<Self> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region)
            .with_access_key_id(&cfg.access_key_id)
            .with_secret_access_key(&cfg.secret_access_key);
        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.with_endpoint(endpoint).with_allow_http(true);
        }
        // Inside the runtime: the client builds an async HTTP stack that wants a
        // reactor in scope.
        let store = runtime()?
            .block_on(async { builder.build() })
            .map_err(to_cortex_error)?;
        Ok(Self::with_store(
            Arc::new(store),
            cfg.key_prefix.clone().unwrap_or_default(),
        ))
    }

    /// Wrap an already-built store.
    ///
    /// Deliberately not `pub`. Widening it would let this type be assembled over
    /// any `ObjectStore` — GCS, Azure, even `LocalFileSystem` — which may well be
    /// worth doing, but not under the name `S3Volume` and not without its own
    /// design pass. Tests reach it as a sibling module.
    fn with_store(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        S3Volume {
            prefix: prefix.trim_matches('/').to_string(),
            store,
        }
    }

    /// Confirm the bucket answers, for a caller that wants to fail at mount time.
    ///
    /// One listing. A misconfigured bucket, region, endpoint or key is otherwise
    /// invisible until something reads the mount, and it arrives there as `EIO` on
    /// every `stat` — an object store reports a failed listing without a status code,
    /// so there is nothing finer to say later either.
    ///
    /// Deliberately not folded into [`Self::new`]. Building the client is offline and
    /// stays that way: a caller choosing among credentials — or probing what a
    /// principal may read — needs to construct a volume without a request going out,
    /// and folding this in would make "cannot build a client" and "cannot read the
    /// bucket" the same `Err`.
    ///
    /// It does not cover credentials that expire mid-session; nothing at mount time
    /// can.
    pub fn check_reachable(&self) -> Result<()> {
        self.list(Path::new(""))?;
        Ok(())
    }

    /// Map a request path to an object key, rooted under [`S3Config::key_prefix`].
    ///
    /// `..` and OS prefixes are rejected rather than resolved, so a request can
    /// never address a key outside the configured prefix — the same rule
    /// [`PassthroughVolume`](super::PassthroughVolume) applies to its root.
    fn key(&self, path: &Path) -> Result<String> {
        let mut parts: Vec<&str> = Vec::new();
        if !self.prefix.is_empty() {
            parts.extend(self.prefix.split('/').filter(|s| !s.is_empty()));
        }
        for comp in path.components() {
            match comp {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    parts.push(name.to_str().ok_or(CortexError::InvalidName)?)
                }
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(CortexError::InvalidName);
                }
            }
        }
        Ok(parts.join("/"))
    }
}

/// Parse a key into the store's own path type.
///
/// `Path::parse` preserves bytes that are special in a URL (`%`, `#`, …), so a key
/// surfaced by a listing round-trips to the same object on read.
fn os_path(key: &str) -> Result<OsPath> {
    OsPath::parse(key).map_err(|_| CortexError::InvalidName)
}

/// Translate an object-store error into this crate's vocabulary.
///
/// Every variant that exists today is named, the way the errno tables in the
/// bindings are. Unlike those, the wildcard is **required**: `object_store::Error`
/// is `#[non_exhaustive]`, so an external crate cannot match it exhaustively. The
/// cost is that a variant added upstream falls silently to `Io` instead of breaking
/// the build — which is why `s3_tests.rs` asserts every arm.
///
/// Anything mapped to `Io` must be wrapped with `io::Error::other`, never with a
/// raw OS error: `host_errno` forwards `raw_os_error()` when there is one, so a
/// preserved transport errno would reach userspace as itself rather than as `EIO`.
fn to_cortex_error(err: object_store::Error) -> CortexError {
    use object_store::Error;
    match err {
        Error::NotFound { .. } => CortexError::NotFound,
        Error::AlreadyExists { .. } => CortexError::AlreadyExists,
        Error::InvalidPath { .. } => CortexError::InvalidName,
        Error::PermissionDenied { .. } => CortexError::PermissionDenied,
        // Credentials missing or expired. A claim about the caller, not about the
        // filesystem, so not `Unsupported`; `EACCES` is what userspace can act on.
        Error::Unauthenticated { .. } => CortexError::PermissionDenied,
        Error::UnknownConfigurationKey { .. } => CortexError::InvalidArgument,
        // These two *are* claims about the filesystem — the store cannot do the
        // operation at all — which is what `Unsupported` means.
        Error::NotSupported { .. } => CortexError::Unsupported,
        Error::NotImplemented { .. } => CortexError::Unsupported,
        // Only reachable once this backend sends conditional requests, which it does
        // not. Forwarded like the other `Io` arms rather than replaced with a label:
        // both variants carry a `source` (and a `path`), so there is something to
        // forward. There is no `ESTALE` in `CortexError` today, which is what the
        // right answer would need.
        Error::Precondition { source, .. } | Error::NotModified { source, .. } => {
            CortexError::Io(io::Error::other(source))
        }
        // Our own bug: a task we spawned panicked or the runtime went away.
        Error::JoinError { source } => CortexError::Io(io::Error::other(source)),
        Error::Generic { source, .. } => CortexError::Io(io::Error::other(source)),
        other => CortexError::Io(io::Error::other(other)),
    }
}

/// Carry an error into the `io::Error` that the data plane speaks.
///
/// The kind matters: `From<io::Error> for CortexError` is what turns it back on the
/// way up through `PosixFs`, and a kind that conversion does not recognise arrives as
/// a bare `Io` — losing the classification the errno tables depend on.
///
/// One name changes on the way back: `InvalidName` shares `InvalidInput` with
/// `InvalidArgument`, so it returns as the latter. Nothing observes the difference —
/// both errno tables already render either as `EINVAL`.
fn to_io_error(err: CortexError) -> io::Error {
    // Already one; unwrapping keeps its message and any errno it carries.
    if let CortexError::Io(inner) = err {
        return inner;
    }
    let kind = match &err {
        CortexError::NotFound => io::ErrorKind::NotFound,
        CortexError::PermissionDenied => io::ErrorKind::PermissionDenied,
        CortexError::ReadOnly => io::ErrorKind::ReadOnlyFilesystem,
        CortexError::Unsupported => io::ErrorKind::Unsupported,
        CortexError::IsADirectory => io::ErrorKind::IsADirectory,
        CortexError::NotADirectory => io::ErrorKind::NotADirectory,
        CortexError::InvalidName | CortexError::InvalidArgument => io::ErrorKind::InvalidInput,
        // These have a kind the conversion recognises too, so naming them keeps the
        // classification rather than flattening it. `AlreadyExists` is reachable
        // today: a store answering 409 turns into it upstream.
        CortexError::AlreadyExists => io::ErrorKind::AlreadyExists,
        CortexError::NotEmpty => io::ErrorKind::DirectoryNotEmpty,
        CortexError::FileTooLarge => io::ErrorKind::FileTooLarge,
        CortexError::NoSpace => io::ErrorKind::StorageFull,
        // `BadHandle` and `CrossDevice` are the two that cannot survive: std has no
        // kind for either, so they arrive as `Io` and render EIO.
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err)
}

/// Read-ahead window size. One miss fetches this much and serves subsequent reads
/// inside it from memory, which is the difference between one round trip and one
/// per request — a guest asks in 128 KiB pieces at most (32 KiB through FUSE-T).
const READAHEAD_CHUNK: u64 = 8 << 20;

/// An open object.
pub struct S3Handle {
    store: Arc<dyn ObjectStore>,
    key: OsPath,
    /// Size as of the open, from the same `head` that produced the [`Stat`].
    ///
    /// Held so a read past the end answers without a round trip, and so ranges can
    /// be clamped rather than sent and rejected — an out-of-range GET comes back as
    /// the generic error variant, indistinguishable from a transport failure.
    ///
    /// **Deliberately immutable.** It goes stale if the object is replaced while the
    /// handle is open, which a read-only mount already tolerates in its cache, and a
    /// plain `u64` is readable without taking `cache` — which keeps the lock this
    /// handle holds across a network wait to the cache alone.
    size: u64,
    /// Taken through [`lock`], which ignores poisoning: a panic anywhere else must
    /// not turn every later read on this handle into a panic of its own. Under krun
    /// there is one worker thread, so that would take the whole virtio-fs device
    /// down. `Debug` below declines to block for the same reason.
    cache: Mutex<ReadCache>,
}

/// What a handle remembers between reads.
#[derive(Default)]
struct ReadCache {
    /// The last fetched window as `(start, bytes)`.
    window: Option<(u64, Vec<u8>)>,
    /// Where the previous read ended, or `None` before the first one.
    ///
    /// This is how sequential access is recognised — a read that starts where the
    /// last one ended earns read-ahead, anything else is served at exactly the size
    /// asked for. Request size cannot serve as the signal: one consumer path sends
    /// a uniform size whether access is sequential or random.
    ///
    /// Assigned, not folded with `max`. A monotonic value would pin itself to the
    /// highest offset ever read, and then nothing below that can ever equal it — so
    /// a reader that peeks at the tail once and then streams from the front (a zip's
    /// central directory, an ELF's section headers) loses read-ahead for the whole
    /// file. What assignment gives up is rolling back progress another reader made
    /// while this one had the lock dropped, which costs that reader a single
    /// read-ahead.
    last_end: Option<u64>,
}

/// Everything an object's metadata says, not just its length.
///
/// `etag` and `version` are here because a consumer that caches needs them to
/// revalidate, and `mtime` because a guest negotiating `AUTO_INVAL_DATA` watches it
/// to decide when to drop cached pages — a timestamp stuck at the epoch is never
/// invalidated.
fn file_stat(meta: &ObjectMeta) -> Stat {
    let mut stat = Stat::new(DirentKind::File, meta.size);
    stat.mtime = Some(meta.last_modified.into());
    stat.etag = meta.e_tag.clone();
    stat.version = meta.version.clone();
    stat
}

/// A directory, which an object store never stores. Nothing to report but the kind:
/// a prefix has no size, and no timestamp of its own to take one from.
fn dir_stat() -> Stat {
    Stat::new(DirentKind::Dir, 0)
}

/// What a key turned out to be. Carries the metadata when it is a file, so a caller
/// that needs both the verdict and the size pays for one `head`, not two.
enum Entry {
    File(ObjectMeta),
    Dir,
}

/// Read a listing as an answer about children — and refuse to read a failure as one.
///
/// Separated from the request so the decision can be tested without a store: what
/// matters here is not the listing but the rule that **an error is not an answer**.
///
/// A failed listing cannot tell a missing prefix from an unreachable store, and the
/// two want opposite things from userspace: `ENOENT` is a durable claim it acts on
/// permanently, `EIO` a transient one it retries. Swallowing the error and reporting
/// "no children" would turn a store that is briefly unreachable into a file that does
/// not exist — the same mistake the read path avoids by clamping a range rather than
/// catching what comes back.
fn children_from(listed: object_store::Result<object_store::ListResult>) -> Result<bool> {
    let listed = listed.map_err(to_cortex_error)?;
    Ok(!listed.common_prefixes.is_empty() || !listed.objects.is_empty())
}

impl S3Volume {
    /// What a path is, in as few requests as the answer allows.
    ///
    /// Three answers from up to two requests:
    ///
    /// * an object with a body — a file, from `head` alone;
    /// * a key that also names a prefix with children — a directory, whether or not
    ///   an object of that exact name exists;
    /// * neither — absent.
    ///
    /// The second case is why a successful `head` is not the end of it. An object
    /// store console writes a 0-byte object to stand for a folder, and `head`
    /// succeeds on that, so reporting what `head` said would call a directory an
    /// empty file and leave the guest unable to descend into it. A body of any
    /// length settles the question, which keeps the extra request to keys that are
    /// genuinely ambiguous.
    ///
    /// Shared by `stat` and `open` so the two cannot disagree about a name — the
    /// same reason `list` decides collisions from the object's size rather than by
    /// consulting `stat`.
    fn classify(&self, path: &Path) -> Result<Entry> {
        let key = self.key(path)?;
        // The volume's own root is a directory by construction — there need not be
        // any object or prefix of that name, and every mount opens by asking for it.
        if key.is_empty() {
            return Ok(Entry::Dir);
        }
        let os = os_path(&key)?;
        runtime()?.block_on(async {
            match self.store.head(&os).await {
                Ok(meta) if meta.size > 0 => Ok(Entry::File(meta)),
                // Ambiguous: empty body, so it may be standing in for a prefix.
                Ok(meta) => {
                    if self.has_children(&os).await? {
                        Ok(Entry::Dir)
                    } else {
                        Ok(Entry::File(meta))
                    }
                }
                // No object with that exact key. Still a directory if keys live
                // under it — a prefix is not an object.
                Err(object_store::Error::NotFound { .. }) => {
                    if self.has_children(&os).await? {
                        Ok(Entry::Dir)
                    } else {
                        Err(CortexError::NotFound)
                    }
                }
                Err(err) => Err(to_cortex_error(err)),
            }
        })
    }

    /// Whether any key lives under `prefix`.
    ///
    /// A listing is how this is asked, and an absent prefix answers with an empty
    /// result rather than an error — so emptiness, not failure, is what says "no".
    /// Without that distinction every path would look like a directory.
    ///
    /// An error is not read as "no children" — [`children_from`] carries that rule.
    /// What follows from it reaches callers: a bad credential or a missing bucket
    /// surfaces as `EIO` on any `stat` that gets here, because an object store reports
    /// a failed listing generically whatever status came back. `head` and `get` do
    /// carry the status, so a name that exists still answers `EACCES`.
    async fn has_children(&self, prefix: &OsPath) -> Result<bool> {
        children_from(self.store.list_with_delimiter(Some(prefix)).await)
    }

    /// Turn one listing into entries, emitting every name exactly once.
    ///
    /// Where a prefix and an object share a name, **the object's size decides**:
    ///
    /// * empty — the object is standing in for the directory, so the object goes;
    /// * non-empty — the object is real content that cannot be hidden, so the
    ///   *prefix* goes, and its subtree becomes unreachable.
    ///
    /// That asymmetry is the price of a flat namespace, and it is paid this way round
    /// because a hidden empty file costs less than an unreachable subtree. The
    /// alternative — always preferring the directory — would leave `stat` disagreeing
    /// with the listing unless it re-checked for children on *every* successful
    /// `head`, which is a second round trip on every ordinary file.
    ///
    /// `stat` reaches the same verdict from the same fact without consulting this
    /// code: an empty body sends it to the children check, a non-empty one does not.
    ///
    /// Entries come out sorted. The kernel resumes a `readdir` by quoting a position
    /// in this list, so the order has to be the same on the next call.
    fn resolve_collision(listed: object_store::ListResult, marker: &str) -> Vec<Dirent> {
        let mut dirs: BTreeSet<String> = listed
            .common_prefixes
            .iter()
            .filter_map(|prefix| prefix.filename().map(str::to_owned))
            .collect();

        let mut files: Vec<(String, ObjectMeta)> = Vec::new();
        for meta in listed.objects {
            // The listed prefix itself, arriving as a marker object.
            if meta.location.as_ref() == marker {
                continue;
            }
            let Some(name) = meta.location.filename().map(str::to_owned) else {
                continue;
            };
            if dirs.contains(&name) {
                if meta.size == 0 {
                    continue;
                }
                dirs.remove(&name);
            }
            files.push((name, meta));
        }

        let mut out: Vec<Dirent> = dirs
            .into_iter()
            .map(|name| Dirent::new(name, DirentKind::Dir))
            .chain(
                files
                    .into_iter()
                    .map(|(name, meta)| Dirent::with_stat(name, file_stat(&meta))),
            )
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

impl Mountable for S3Volume {
    type Handle = S3Handle;

    /// Metadata for one key, or for the prefix of that name — see `classify`.
    fn stat(&self, path: &Path) -> Result<Stat> {
        Ok(match self.classify(path)? {
            Entry::File(meta) => file_stat(&meta),
            Entry::Dir => dir_stat(),
        })
    }

    /// The entries directly under `path`.
    ///
    /// One request. `list_with_delimiter` rolls deeper keys into the prefix they sit
    /// in and hands back the objects at this level with their metadata already
    /// attached, which is what lets [`Dirent::with_stat`] answer a `readdirplus`
    /// without a `stat` per name.
    ///
    /// Two things have to be untangled first, both consequences of a flat key space
    /// pretending to be a tree:
    ///
    /// * The listed prefix can come back as an object of its own — a folder marker.
    ///   That is *this* directory, not something in it, so it is dropped.
    /// * A name can arrive from both sides at once: as a prefix (because keys live
    ///   under it) and as an object (because a key of exactly that name exists).
    ///   A `readdir` may not repeat a name, so one side has to go — see
    ///   `resolve_collision`.
    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let key = self.key(path)?;
        let prefix = if key.is_empty() {
            None
        } else {
            Some(os_path(&key)?)
        };
        runtime()?.block_on(async {
            let listed = self
                .store
                .list_with_delimiter(prefix.as_ref())
                .await
                .map_err(to_cortex_error)?;
            Ok(Self::resolve_collision(
                listed,
                prefix.as_ref().map(|p| p.as_ref()).unwrap_or(""),
            ))
        })
    }

    /// Open a file for reading.
    ///
    /// Directories are refused rather than opened: a kernel reaches a directory
    /// through `opendir`, so an `open` that lands on one is a caller mistake, and the
    /// marker case (a 0-byte object standing for a prefix) has to be refused too or
    /// the guest would read a directory as an empty file.
    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        options.validate()?;
        // Refused before the network is touched: nothing about the object changes the
        // answer, and a caller that meant to write should hear so at once.
        if options.intends_write() {
            return Err(CortexError::ReadOnly);
        }
        match self.classify(path)? {
            Entry::Dir => Err(CortexError::IsADirectory),
            Entry::File(meta) => {
                let stat = file_stat(&meta);
                let handle = S3Handle {
                    store: self.store.clone(),
                    // Taken from the `head` that classified it rather than mapped
                    // again, so the handle cannot end up pointed at a different key
                    // than the one that was inspected.
                    key: meta.location,
                    // The same size the `Stat` reports. Clamping a read depends on the
                    // two agreeing.
                    size: meta.size,
                    cache: Mutex::default(),
                };
                Ok((handle, stat))
            }
        }
    }

    fn mkdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    fn unlink(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    fn rmdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    // `rename` keeps the trait's default, which is already `ReadOnly`.
}

/// Reports the window's extent, never its contents.
///
/// Deriving this would print a read-ahead window's worth of file data — megabytes —
/// into whatever `{:?}` reaches. The extent is what is worth seeing anyway: where the
/// cache sits and whether the handle is in a sequence.
impl std::fmt::Debug for S3Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("S3Handle");
        out.field("key", &self.key.as_ref())
            .field("size", &self.size);
        match self.cache.try_lock() {
            Ok(cache) => out
                .field(
                    "window",
                    &cache
                        .window
                        .as_ref()
                        .map(|(start, data)| (*start, data.len())),
                )
                .field("last_end", &cache.last_end),
            // A `Debug` that blocks, or that panics on a poisoned lock, is worse than
            // one that admits it could not look.
            Err(_) => out.field("cache", &"<locked>"),
        }
        .finish()
    }
}

impl S3Handle {
    /// Copy out of the cached window, returning how much of `out` it could fill.
    ///
    /// Zero means the window does not cover `at` — it may still cover *part* of what
    /// was asked for, which is why the caller loops rather than treating a partial
    /// answer as the end.
    fn from_window(window: &Option<(u64, Vec<u8>)>, at: u64, out: &mut [u8]) -> usize {
        let Some((start, data)) = window else {
            return 0;
        };
        if at < *start || at >= *start + data.len() as u64 {
            return 0;
        }
        let from = (at - *start) as usize;
        let n = (data.len() - from).min(out.len());
        out[..n].copy_from_slice(&data[from..from + n]);
        n
    }

    /// One ranged GET. No lock is held here — see the loop in [`FileExt::read_at`].
    fn fetch(&self, at: u64, end: u64) -> io::Result<Vec<u8>> {
        let options = GetOptions {
            range: Some(GetRange::Bounded(at..end)),
            ..Default::default()
        };
        let rt = runtime().map_err(to_io_error)?;
        rt.block_on(async {
            let got = self
                .store
                .get_opts(&self.key, options)
                .await
                .map_err(|err| to_io_error(to_cortex_error(err)))?;
            let bytes = got
                .bytes()
                .await
                .map_err(|err| to_io_error(to_cortex_error(err)))?;
            Ok(bytes.to_vec())
        })
    }
}

impl FileExt for S3Handle {
    /// Fill `buf` from `offset`, fetching only what the cache cannot answer.
    ///
    /// Three things are load-bearing:
    ///
    /// **The range is clamped, never rejected.** A GET that starts at or past the end
    /// comes back as the store's generic error, which is indistinguishable from a
    /// transport failure — so the end is decided here, from the size the open
    /// recorded, and a read beyond it answers `Ok(0)` without a round trip.
    ///
    /// **A short return means EOF and nothing else.** That is the contract this trait
    /// states, and `PosixFs` believes it — it calls this once and passes the length
    /// on. So a request the cached window only partly covers must not stop there: the
    /// loop fills the remainder, or a file would appear to end at a window boundary.
    ///
    /// **Read-ahead is earned by continuity, not by request size.** A read that starts
    /// where the last one ended fetches a whole window; anything else fetches exactly
    /// what was asked. Request size cannot serve as the signal — one consumer path
    /// sends the same size whether access is sequential or random.
    ///
    /// The lock is never held across a fetch, so `last_end` is re-read after each one.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // `Bounded(0..0)` is an error rather than an empty answer, so an empty request
        // is settled here — and without disturbing `last_end`, which a zero-length
        // read in the middle of a stream would otherwise push off the sequence.
        if buf.is_empty() || offset >= self.size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.size - offset) as usize;
        let buf = &mut buf[..want];

        let mut filled = 0usize;
        while filled < want {
            let at = offset + filled as u64;

            // One acquisition for both questions: nothing waits on the network between
            // them, so splitting it would only widen the chance of an answer from one
            // state and a decision from another.
            let (from_cache, sequential) = {
                let cache = lock(&self.cache);
                (
                    Self::from_window(&cache.window, at, &mut buf[filled..]),
                    cache.last_end == Some(at),
                )
            };
            if from_cache > 0 {
                filled += from_cache;
                continue;
            }

            let span = if sequential {
                READAHEAD_CHUNK
            } else {
                (want - filled) as u64
            };
            let data = self.fetch(at, (at + span).min(self.size))?;
            if data.is_empty() {
                // Defensive. A store answering a non-empty range with no bytes has no
                // defined meaning here, and looping on it would not terminate.
                //
                // Not the stale-size path: an object that shrank makes the range
                // invalid, so the store errors and `?` carries that out of `fetch` —
                // which is right, because shortening the return instead would tell
                // `PosixFs` this was EOF.
                break;
            }
            let n = data.len().min(want - filled);
            buf[filled..filled + n].copy_from_slice(&data[..n]);
            filled += n;
            lock(&self.cache).window = Some((at, data));
        }

        if filled > 0 {
            // Assigned, not folded — see `ReadCache::last_end`.
            lock(&self.cache).last_end = Some(offset + filled as u64);
        }
        Ok(filled)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
        // `ReadOnlyFilesystem`, not `Unsupported`: this is the only way a handle can
        // say "read-only", and the conversion in `error.rs` turns exactly this kind
        // back into `CortexError::ReadOnly`. `Unsupported` here would reach the
        // guest as ENOSYS — a claim that the filesystem has no write at all.
        Err(io::Error::from(io::ErrorKind::ReadOnlyFilesystem))
    }
}

impl FileHandle for S3Handle {
    fn truncate(&self, _size: u64) -> Result<()> {
        Err(CortexError::ReadOnly)
    }
}

#[cfg(test)]
#[path = "s3_tests.rs"]
mod tests;
