//! A read-only [`FileSystem`] store over an object store (S3 and compatibles).

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    io,
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use object_store::{
    GetOptions, GetRange, ObjectMeta, ObjectStore, ObjectStoreExt, aws::AmazonS3Builder,
    path::Path as OsPath,
};

use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, Stat},
    lock::lock,
};

/// Connection settings for an [`S3Fs`].
///
/// `Serialize` so a caller can keep it in a file; `Debug` redacts the secret, since a log is
/// not that file.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Custom endpoint (MinIO / R2 / localstack); `None` for real AWS.
    pub endpoint: Option<String>,
    /// Key prefix every path is rooted under. Composes with a mount table's path: the table
    /// strips its own path first, then this prefix is prepended.
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

/// An object store's keys, served as a read-only tree.
///
/// Keys map to paths and a directory is a key prefix: there is no directory object to create or
/// remove. Metadata comes from `head`, listings from `list_with_delimiter`, and file data from
/// ranged GETs, each `.await`ing the `object_store` client directly; no runtime lives here.
///
/// # Read-only
///
/// A write at a byte offset over S3 means reading the whole object, patching it, and putting it
/// back: `object_store` has no byte-range patch, and its multipart parts must be at least 5 MiB
/// where a guest writes at most 1 MiB. So a write needs staging, with a ceiling (the guest
/// picks the offset, so the allocation is guest-chosen). And staging needs an end: a multipart
/// upload must be *completed*, and nothing signals when a writer is done.
///
/// So every write answers `ReadOnlyFilesystem`, not `Unsupported`: the store *could* write,
/// this implementation will not.
pub struct S3Fs {
    store: Arc<dyn ObjectStore>,
    /// No leading or trailing `/`, so [`Self::key`] can join with `/` unconditionally.
    prefix: String,
    /// What the store answered when a directory was last listed.
    listings: Mutex<Listings>,
    /// What reads have learned about keys, and what they read ahead into.
    ///
    /// Taken through [`lock`], which ignores poisoning: a binding serving its device from one
    /// worker thread would otherwise lose the whole mount to one panic.
    windows: Mutex<Windows>,
}

impl S3Fs {
    /// Build an S3 client from `cfg`, without sending a request.
    pub fn new(cfg: &S3Config) -> io::Result<Self> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region)
            .with_access_key_id(&cfg.access_key_id)
            .with_secret_access_key(&cfg.secret_access_key);
        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.with_endpoint(endpoint).with_allow_http(true);
        }
        let store = builder.build().map_err(to_io_error)?;
        Ok(Self::with_store(
            Arc::new(store),
            cfg.key_prefix.clone().unwrap_or_default(),
        ))
    }

    /// Wrap an already-built store.
    ///
    /// Not `pub`: it would let `S3Fs` wrap any `ObjectStore` (GCS, Azure, `LocalFileSystem`),
    /// which the name does not promise. Tests reach it as a child module.
    fn with_store(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        S3Fs {
            prefix: prefix.trim_matches('/').to_string(),
            store,
            listings: Mutex::default(),
            windows: Mutex::default(),
        }
    }

    /// Confirm the bucket answers, for a caller that wants to fail at mount time.
    ///
    /// One listing. A misconfigured bucket, region, endpoint or key otherwise shows up only
    /// when the mount is read, as `EIO` on every `stat`: a failed listing carries no status
    /// code, so there is nothing finer to say later either.
    ///
    /// Separate from [`Self::new`] so building stays offline: a caller choosing among
    /// credentials, or probing what a principal may read, can construct a volume with no
    /// request, and "cannot build a client" stays distinct from "cannot read the bucket".
    ///
    /// Credentials that expire mid-session are not covered; nothing at mount time can be.
    pub async fn check_reachable(&self) -> io::Result<()> {
        self.list(Path::new("")).await?;
        Ok(())
    }

    /// Map a request path to an object key, rooted under [`S3Config::key_prefix`].
    ///
    /// `..` and OS prefixes are rejected rather than resolved, so a request can never address
    /// a key outside the configured prefix.
    fn key(&self, path: &Path) -> io::Result<String> {
        let mut parts: Vec<&str> = Vec::new();
        if !self.prefix.is_empty() {
            parts.extend(self.prefix.split('/').filter(|s| !s.is_empty()));
        }
        for comp in path.components() {
            match comp {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => parts.push(
                    name.to_str()
                        .ok_or(io::Error::from(io::ErrorKind::InvalidFilename))?,
                ),
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::ErrorKind::InvalidFilename.into());
                }
            }
        }
        Ok(parts.join("/"))
    }
}

/// Parse a key into the store's own path type.
///
/// `Path::parse` preserves bytes special in a URL (`%`, `#`, …), so a listed key round-trips
/// to the same object on read.
fn os_path(key: &str) -> io::Result<OsPath> {
    OsPath::parse(key).map_err(|_| io::ErrorKind::InvalidFilename.into())
}

/// Translate an object-store error into the kind this crate answers with.
///
/// Every variant with its own answer is named, but the wildcard is required:
/// `object_store::Error` is `#[non_exhaustive]`, so a variant added upstream reaches userspace
/// as `EIO`.
///
/// Unclassified errors become `Other`, never a raw OS error: `host_errno` forwards
/// `raw_os_error()`, so a transport errno would reach userspace as itself rather than `EIO`.
/// Wrapping also keeps the upstream error as the `source`.
fn to_io_error(err: object_store::Error) -> io::Error {
    use object_store::Error;
    let kind = match &err {
        Error::NotFound { .. } => io::ErrorKind::NotFound,
        Error::AlreadyExists { .. } => io::ErrorKind::AlreadyExists,
        Error::InvalidPath { .. } => io::ErrorKind::InvalidFilename,
        Error::PermissionDenied { .. } => io::ErrorKind::PermissionDenied,
        // Credentials missing or expired: a claim about the caller, not the filesystem, so
        // `EACCES` rather than `Unsupported`.
        Error::Unauthenticated { .. } => io::ErrorKind::PermissionDenied,
        Error::UnknownConfigurationKey { .. } => io::ErrorKind::InvalidInput,
        // The store cannot do the operation at all.
        Error::NotSupported { .. } => io::ErrorKind::Unsupported,
        Error::NotImplemented { .. } => io::ErrorKind::Unsupported,
        // `Precondition`/`NotModified` need conditional requests, which this store does not
        // send.
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err)
}

/// Read-ahead window size. One miss fetches this much so later reads inside it cost no round
/// trip; a guest asks in 128 KiB pieces at most (32 KiB through FUSE-T).
const READAHEAD_CHUNK: u64 = 8 << 20;

/// How many keys may hold a window at once.
///
/// Windows are keyed by path, not by an open, so no open's lifetime bounds them; this does.
///
/// Not a memory bound: a window is as large as the read that filled it, and a caller reading
/// a whole file in one call gets a window the size of the file. [`MAX_CACHED_BYTES`] is the
/// memory bound.
const MAX_CACHED_KEYS: usize = 8;

/// How many bytes of object bodies are held across every entry.
///
/// Whole bodies make a file opened twice cost one `head` and no transfer, but six large
/// documents should not be six large documents of memory. Past this, the oldest entries give
/// up their windows.
///
/// Never the entry just filled, however large: the read in progress is served from it, and
/// dropping it would refetch the bytes just fetched.
const MAX_CACHED_BYTES: u64 = 64 << 20;

/// What the store remembers about one key between reads.
///
/// Keyed by path, so no open ends its life (see [`FileSystem`]). Staleness is bounded by
/// [`MAX_CACHED_KEYS`] on how many are kept and [`S3Fs::revalidate`] on whether a kept one
/// still describes the object.
struct ReadCache {
    /// Size as of the `head` that filled this entry.
    ///
    /// Lets a read past the end answer without a round trip, and ranges be clamped rather
    /// than rejected: an out-of-range GET returns the generic error, indistinguishable from a
    /// transport failure.
    ///
    /// Because it clamps, staleness matters: a grown object read against a smaller remembered
    /// size answers `Ok(0)` at the old end, which means EOF. Hence [`Self::describes`].
    size: u64,

    /// The object's identity when this entry was filled, for [`Self::describes`].
    ///
    /// `etag` is a content fingerprint and settles it alone; `mtime` serves stores that
    /// report no tag, where size and timestamp are all there is to compare.
    etag: Option<String>,

    mtime: SystemTime,

    /// The last fetched window as `(start, bytes)`.
    window: Option<(u64, Vec<u8>)>,

    /// Where the previous read of this key ended, or `None` before the first one.
    ///
    /// Recognises sequential access: a read starting where the last ended earns read-ahead,
    /// anything else is served at exactly the size asked. Request size cannot be the signal,
    /// since one consumer path sends a uniform size either way.
    ///
    /// Assigned, not folded with `max`: a monotonic value would pin to the highest offset, so
    /// a reader that peeks at the tail then streams from the front (a zip's central directory,
    /// an ELF's section headers) would lose read-ahead for the whole file.
    ///
    /// Per *key*, so two consumers reading one object cost each other read-ahead. A path is
    /// not an open (see [`FileSystem`]), and a wrong guess costs one round trip, never a
    /// wrong byte.
    last_end: Option<u64>,
}

impl ReadCache {
    /// A new entry for what a `head` just reported, with nothing read ahead yet.
    fn fresh(meta: &ObjectMeta) -> Self {
        ReadCache {
            size: meta.size,
            etag: meta.e_tag.clone(),
            mtime: meta.last_modified.into(),
            window: None,
            last_end: None,
        }
    }

    /// Whether `meta` still describes the object this entry was filled from.
    ///
    /// A tag on both sides settles it, catching a same-length replacement. Otherwise only
    /// size and timestamp remain, which miss a same-length rewrite within the store's
    /// timestamp resolution.
    fn describes(&self, meta: &ObjectMeta) -> bool {
        match (&self.etag, &meta.e_tag) {
            (Some(held), Some(fresh)) => held == fresh,
            _ => self.size == meta.size && self.mtime == SystemTime::from(meta.last_modified),
        }
    }
}

/// How long a listing is handed out again without asking the store.
///
/// A listing is one request, sometimes a few, so this is for the same directory asked for
/// twice in a row: a reader walking a tree, or an agent running `ls` in a loop. This store
/// never writes, so only someone else changes a listing; a reader who sees that has
/// [`FileSystem::forget`].
const LISTING_TTL: Duration = Duration::from_secs(30);

/// How many directories' listings are kept.
const MAX_CACHED_LISTINGS: usize = 256;

/// The listings kept, and the order to give them up in.
#[derive(Default)]
struct Listings {
    by_prefix: HashMap<String, (Instant, Arc<Vec<Dirent>>)>,
    /// Prefixes in first-listed order. Insertion rather than true LRU: a wrong eviction costs
    /// one round trip, and a use-ordered queue would be touched under the mutex on every hit.
    order: VecDeque<String>,
}

impl Listings {
    /// What was listed for `prefix`, while it is still worth handing out.
    fn get(&self, prefix: &str) -> Option<Arc<Vec<Dirent>>> {
        let (at, listed) = self.by_prefix.get(prefix)?;
        (at.elapsed() < LISTING_TTL).then(|| listed.clone())
    }

    fn admit(&mut self, prefix: String, listed: Arc<Vec<Dirent>>) {
        if self
            .by_prefix
            .insert(prefix.clone(), (Instant::now(), listed))
            .is_none()
        {
            self.order.push_back(prefix);
        }
        while self.order.len() > MAX_CACHED_LISTINGS {
            if let Some(oldest) = self.order.pop_front() {
                self.by_prefix.remove(&oldest);
            }
        }
    }

    fn clear(&mut self) {
        self.by_prefix.clear();
        self.order.clear();
    }
}

/// The windows, and the order to give them up in.
#[derive(Default)]
struct Windows {
    by_key: HashMap<String, ReadCache>,

    /// Keys in first-cached order, oldest evicted first. Insertion rather than true LRU: a
    /// wrong eviction costs one round trip, and a use-ordered queue would be touched on every
    /// read under the mutex every read already contends for.
    ///
    /// Holds exactly `by_key`'s keys, so its length is the cap; a stale key would waste a slot
    /// and later evict a live entry.
    order: VecDeque<String>,
}

impl Windows {
    /// Make room for `key` and record it, evicting the oldest entry once the cap is passed.
    ///
    /// Only a key new to the map joins the queue: two reads that both missed can race to
    /// admit it, and a duplicate would be evicted while the entry is in use.
    fn admit(&mut self, key: String, entry: ReadCache) {
        if self.by_key.insert(key.clone(), entry).is_none() {
            self.order.push_back(key);
        }
        if self.order.len() > MAX_CACHED_KEYS
            && let Some(oldest) = self.order.pop_front()
        {
            self.by_key.remove(&oldest);
        }
    }

    /// Put `data` in `key`'s window, and give up what does not fit.
    ///
    /// Eviction lives here because this is the only place a window grows.
    fn store_window(&mut self, key: &str, at: u64, data: Vec<u8>) {
        let Some(cache) = self.by_key.get_mut(key) else {
            return;
        };
        cache.window = Some((at, data));
        while self.held_bytes() > MAX_CACHED_BYTES {
            // The oldest entry other than the one just filled, which is never dropped.
            let Some(oldest) = self
                .order
                .iter()
                .find(|held| {
                    held.as_str() != key
                        && self.by_key.get(*held).is_some_and(|c| c.window.is_some())
                })
                .cloned()
            else {
                return;
            };
            if let Some(cache) = self.by_key.get_mut(&oldest) {
                // Only the window goes: the entry's size and etag, cheap to keep, let the next
                // read clamp and revalidate.
                cache.window = None;
            }
        }
    }

    fn held_bytes(&self) -> u64 {
        self.by_key
            .values()
            .filter_map(|c| c.window.as_ref())
            .map(|(_, data)| data.len() as u64)
            .sum()
    }

    /// Forget `key` entirely, so the next read of it starts from a `head`.
    fn forget(&mut self, key: &str) {
        if self.by_key.remove(key).is_some() {
            self.order.retain(|held| held != key);
        }
    }
}

/// Everything an object's metadata says, not just its length.
///
/// `etag` and `version` let a caching consumer revalidate; `mtime` is what a guest
/// negotiating `AUTO_INVAL_DATA` watches to drop cached pages, and one stuck at the epoch
/// never invalidates.
fn file_stat(meta: &ObjectMeta) -> Stat {
    let mut stat = Stat::new(DirentKind::File, meta.size);
    stat.mtime = Some(meta.last_modified.into());
    stat.etag = meta.e_tag.clone();
    stat.version = meta.version.clone();
    stat
}

/// A directory is a prefix, with no size or timestamp of its own.
fn dir_stat() -> Stat {
    Stat::new(DirentKind::Dir, 0)
}

/// What a key turned out to be. A file carries its metadata, so verdict and size cost one
/// `head`.
enum Entry {
    File(ObjectMeta),
    Dir,
}

/// Whether a listing found children. **An error is not an answer**: it is propagated, never
/// read as "no children".
///
/// A failed listing cannot tell a missing prefix from an unreachable store, and userspace
/// treats `ENOENT` as durable but retries `EIO`, so swallowing the error would turn a briefly
/// unreachable store into a missing file. Separate from the request so it is testable
/// without a store.
fn children_from(listed: object_store::Result<object_store::ListResult>) -> io::Result<bool> {
    let listed = listed.map_err(to_io_error)?;
    Ok(!listed.common_prefixes.is_empty() || !listed.objects.is_empty())
}

impl S3Fs {
    /// What a path is, in as few requests as the answer allows.
    ///
    /// Three answers from up to two requests:
    ///
    /// * an object with a body: a file, from `head` alone;
    /// * a key that also names a prefix with children: a directory, whether or not an empty
    ///   object of that exact name exists;
    /// * neither: absent.
    ///
    /// A successful `head` is not always the end: an object store console writes a 0-byte
    /// object to stand for a folder, and trusting `head` would make it an empty file the guest
    /// cannot descend into. Only an empty body costs the extra request.
    ///
    /// Shared by `stat` and the first read of a key, so the two cannot disagree about a name.
    ///
    /// Also revalidates the read cache ([`Self::revalidate`]), since the fresh `head` every
    /// `stat` spends is exactly what says whether a cached entry is still current.
    async fn classify(&self, path: &Path) -> io::Result<Entry> {
        let key = self.key(path)?;
        // The root is a directory by construction: no object or prefix need exist, and every
        // mount opens by asking for it.
        if key.is_empty() {
            return Ok(Entry::Dir);
        }
        let found = self.classify_key(&key).await;
        if let Ok(found) = &found {
            self.revalidate(&key, found);
        }
        found
    }

    /// [`Self::classify`] without the revalidation, for a key already known to be non-empty.
    async fn classify_key(&self, key: &str) -> io::Result<Entry> {
        let os = os_path(key)?;
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
            // No object with that exact key; still a directory if keys live under it.
            Err(object_store::Error::NotFound { .. }) => {
                if self.has_children(&os).await? {
                    Ok(Entry::Dir)
                } else {
                    Err(io::ErrorKind::NotFound.into())
                }
            }
            Err(err) => Err(to_io_error(err)),
        }
    }

    /// Whether any key lives under `prefix`.
    ///
    /// An absent prefix lists as empty, not as an error, so emptiness is what says "no".
    ///
    /// Errors propagate ([`children_from`]), so a bad credential or missing bucket surfaces as
    /// `EIO` on any `stat` that gets here: a failed listing carries no status. `head` and `get`
    /// do, so a name that exists still answers `EACCES`.
    async fn has_children(&self, prefix: &OsPath) -> io::Result<bool> {
        children_from(self.store.list_with_delimiter(Some(prefix)).await)
    }

    /// Turn one listing into entries, emitting every name exactly once.
    ///
    /// Where a prefix and an object share a name, **the object's size decides**:
    ///
    /// * empty: the object stands in for the directory, so the object goes;
    /// * non-empty: the object is real content, so the *prefix* goes and its subtree becomes
    ///   unreachable.
    ///
    /// Always preferring the directory would make `stat` disagree with the listing unless it
    /// checked for children on *every* successful `head`, a second round trip per ordinary
    /// file. `stat` reaches the same verdict from the same fact: only an empty body sends it
    /// to the children check.
    ///
    /// Sorted, because the kernel resumes a `readdir` by position.
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

/// Every mutating method keeps the trait's `ReadOnlyFilesystem` default, so a writer hears
/// it on the write; there is no open to hear it on.
impl FileSystem for S3Fs {
    /// Metadata for one key, or for the prefix of that name.
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            Ok(match self.classify(path).await? {
                Entry::File(meta) => file_stat(&meta),
                Entry::Dir => dir_stat(),
            })
        })
    }

    /// The entries directly under `path`.
    ///
    /// One request: `list_with_delimiter` rolls deeper keys into their prefix and returns
    /// this level's objects with metadata, so [`Dirent::with_stat`] answers a `readdirplus`
    /// without a `stat` per name.
    ///
    /// A flat key space posing as a tree needs two fixes:
    ///
    /// * The listed prefix can come back as its own object (a folder marker). That is *this*
    ///   directory, so it is dropped.
    /// * A name can arrive both as a prefix and as an object. A `readdir` may not repeat a
    ///   name, so one side goes.
    ///
    /// Answered from the last listing within `LISTING_TTL`. A listing may lag but a read may
    /// not: a stale name costs a second look, a stale size or body hands over the wrong file.
    /// So `stat` asks every time, and its `head` revalidates what `read_at` keeps.
    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let key = self.key(path)?;
            if let Some(listed) = lock(&self.listings).get(&key) {
                return Ok((*listed).clone());
            }
            let prefix = if key.is_empty() {
                None
            } else {
                Some(os_path(&key)?)
            };
            let listed = self
                .store
                .list_with_delimiter(prefix.as_ref())
                .await
                .map_err(to_io_error)?;
            let entries = Arc::new(Self::resolve_collision(
                listed,
                prefix.as_ref().map(|p| p.as_ref()).unwrap_or(""),
            ));
            lock(&self.listings).admit(key, entries.clone());
            Ok((*entries).clone())
        })
    }

    /// Drop the listings, and what reads learned about keys.
    ///
    /// The read side is revalidated by every `stat` anyway, but a reader asking to look again
    /// wants no kept answer at all.
    fn forget<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.listings).clear();
            *lock(&self.windows) = Windows::default();
        })
    }

    /// Fill `buf` from `offset`, fetching only what the cache cannot answer.
    ///
    /// **The first read of a key costs a `head`.** Clamping needs the size, and there is no
    /// open to have learned it at, so the first read classifies the key and remembers it. A
    /// directory, including a 0-byte marker standing for a prefix, is refused.
    ///
    /// **The range is clamped, never rejected.** A GET at or past the end returns the store's
    /// generic error, indistinguishable from a transport failure, so a read beyond the
    /// remembered size answers `Ok(0)` without a round trip.
    ///
    /// **A short return means EOF.** That is the [`FileSystem`] contract, and callers pass the
    /// length on as-is, so a request a window only partly covers loops to fill the rest.
    ///
    /// **Read-ahead is earned by continuity** (see `ReadCache::last_end`).
    ///
    /// The lock is never held across a fetch or a `head`.
    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            let key = self.key(path)?;
            let (location, size) = self.located(path, &key).await?;
            // `Bounded(0..0)` is an error, so an empty request is settled here, leaving
            // `last_end` undisturbed mid-stream.
            if buf.is_empty() || offset >= size {
                return Ok(0);
            }
            let want = (buf.len() as u64).min(size - offset) as usize;
            let buf = &mut buf[..want];

            let mut filled = 0usize;
            while filled < want {
                let at = offset + filled as u64;

                // One acquisition for both, so they answer from the same state.
                let (from_cache, sequential) = {
                    let windows = lock(&self.windows);
                    match windows.by_key.get(&key) {
                        Some(cache) => (
                            from_window(&cache.window, at, &mut buf[filled..]),
                            cache.last_end == Some(at),
                        ),
                        None => (0, false),
                    }
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
                let data = self.fetch(&location, at, (at + span).min(size)).await?;
                if data.is_empty() {
                    // Defensive: no bytes for a non-empty range is undefined, and looping on it
                    // would not terminate. A shrunken object instead errors out of `fetch`,
                    // rightly, since a short return would claim EOF.
                    break;
                }
                let n = data.len().min(want - filled);
                buf[filled..filled + n].copy_from_slice(&data[..n]);
                filled += n;
                lock(&self.windows).store_window(&key, at, data);
            }

            if filled > 0 {
                // Assigned, not folded with `max` (see `ReadCache::last_end`).
                if let Some(cache) = lock(&self.windows).by_key.get_mut(&key) {
                    cache.last_end = Some(offset + filled as u64);
                }
            }
            Ok(filled)
        })
    }
}

impl S3Fs {
    /// Where `key` lives and how big it is, from the cache or else a `head`.
    ///
    /// On a miss the location comes from that `head`, so a read cannot point at a different
    /// key than the one inspected.
    async fn located(&self, path: &Path, key: &str) -> io::Result<(OsPath, u64)> {
        if let Some(cache) = lock(&self.windows).by_key.get(key) {
            return Ok((os_path(key)?, cache.size));
        }
        let meta = match self.classify(path).await? {
            Entry::Dir => return Err(io::ErrorKind::IsADirectory.into()),
            Entry::File(meta) => meta,
        };
        lock(&self.windows).admit(key.to_string(), ReadCache::fresh(&meta));
        Ok((meta.location, meta.size))
    }

    /// Drop what reads remember about `key` unless `fresh` — what a `head` just said — still
    /// describes the object the entry was filled from.
    ///
    /// Otherwise an object replaced under a mount is served from the old window and clamped
    /// to the old size until eviction, while `stat` reports the new size a guest acts on. And
    /// a guest using `AUTO_INVAL_DATA` drops its pages on the new `mtime` only to get the same
    /// stale bytes back.
    ///
    /// A `Dir` verdict drops the entry too: the key no longer names a file.
    ///
    /// Two deliberate limits:
    ///
    /// * **Freshness is only as frequent as the `stat`s.** A replacement between a `stat` and
    ///   the reads after it is not caught. Catching it within a read needs conditional GETs,
    ///   which turn a replacement into a mid-stream error with no errno this crate can express.
    /// * **A failed `head` changes nothing.** A transient failure must not cost a re-`head` on
    ///   the next read, and POSIX keeps a deleted file readable through an open, so expiring
    ///   by eviction is closer to right than dropping on `ENOENT`.
    fn revalidate(&self, key: &str, fresh: &Entry) {
        let mut windows = lock(&self.windows);
        let stale = match (windows.by_key.get(key), fresh) {
            (None, _) => false,
            (Some(cache), Entry::File(meta)) => !cache.describes(meta),
            (Some(_), Entry::Dir) => true,
        };
        if stale {
            windows.forget(key);
        }
    }

    /// One ranged GET; called with no lock held.
    async fn fetch(&self, location: &OsPath, at: u64, end: u64) -> io::Result<Vec<u8>> {
        let options = GetOptions {
            range: Some(GetRange::Bounded(at..end)),
            ..Default::default()
        };
        let got = self
            .store
            .get_opts(location, options)
            .await
            .map_err(to_io_error)?;
        let bytes = got.bytes().await.map_err(to_io_error)?;
        Ok(bytes.to_vec())
    }
}

/// Copy out of a cached window, returning how much of `out` it could fill.
///
/// Zero means the window does not cover `at`. A partial fill is not EOF: callers loop.
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

/// What the read cache does when the object underneath it changes.
///
/// Over `InMemory`: it still answers `head`, ranged GETs and a per-version etag, which is all
/// the cache reads. `tests/s3_endpoint.rs` covers what only a real S3 can disagree about.
#[cfg(test)]
mod tests {
    use object_store::{PutPayload, memory::InMemory};

    use super::*;

    /// The store and a filesystem over it, so a test can change objects behind the mount.
    fn store() -> (Arc<InMemory>, S3Fs) {
        let store = Arc::new(InMemory::new());
        (store.clone(), S3Fs::with_store(store, String::new()))
    }

    async fn put(store: &InMemory, key: &str, body: &[u8]) {
        store
            .put(&os_path(key).unwrap(), PutPayload::from(body.to_vec()))
            .await
            .unwrap();
    }

    async fn read(fs: &S3Fs, key: &str, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let n = fs.read_at(Path::new(key), &mut buf, 0).await.unwrap();
        buf.truncate(n);
        buf
    }

    /// A listing is kept until `forget`.
    ///
    /// Deleting the object *behind* the store tells a cached answer from a fresh one; counting
    /// requests would pin how the listing is fetched rather than that it is not fetched twice.
    #[tokio::test]
    async fn a_listing_is_answered_from_the_last_one() {
        let (store, fs) = store();
        put(&store, "a.txt", b"a").await;
        put(&store, "b.txt", b"b").await;
        assert_eq!(fs.list(Path::new("/")).await.unwrap().len(), 2);

        store.delete(&os_path("b.txt").unwrap()).await.unwrap();
        assert_eq!(
            fs.list(Path::new("/")).await.unwrap().len(),
            2,
            "the second listing is the first one, which is the whole point"
        );

        fs.forget().await;
        assert_eq!(fs.list(Path::new("/")).await.unwrap().len(), 1);
    }

    /// A read is not a listing: the `stat` before it asks every time, so a file's bytes are
    /// never a guess.
    #[tokio::test]
    async fn a_read_still_asks_after_a_listing_was_kept() {
        let (store, fs) = store();
        put(&store, "a.txt", b"one").await;
        fs.list(Path::new("/")).await.unwrap();

        put(&store, "a.txt", b"two").await;
        // `stat`'s `head` revalidates the read window.
        assert_eq!(fs.stat(Path::new("a.txt")).await.unwrap().size, 3);
        assert_eq!(read(&fs, "a.txt", 3).await, b"two");
    }

    /// An entry for one key sized `mib`, with no body held yet.
    fn held(mib: usize) -> ReadCache {
        ReadCache {
            size: (mib << 20) as u64,
            etag: None,
            mtime: SystemTime::UNIX_EPOCH,
            window: None,
            last_end: None,
        }
    }

    #[test]
    fn what_is_held_stays_inside_the_budget() {
        let mut windows = Windows::default();
        let mib = 1 << 20;
        // Three bodies, half the budget each: the third pushes the first out.
        for key in ["a", "b", "c"] {
            windows.admit(key.to_string(), held(32));
            windows.store_window(key, 0, vec![0u8; 32 * mib]);
        }
        assert!(windows.held_bytes() <= MAX_CACHED_BYTES);
        assert!(
            windows.by_key["a"].window.is_none(),
            "the oldest gave up its body"
        );
        assert!(
            windows.by_key["c"].window.is_some(),
            "the one just read is kept"
        );
        // The entry stays: its size and etag clamp and revalidate the next read.
        assert_eq!(windows.by_key["a"].size, (32 * mib) as u64);
    }

    #[test]
    fn a_body_larger_than_the_budget_is_still_what_the_read_is_served_from() {
        let mut windows = Windows::default();
        windows.admit("small".to_string(), held(1));
        windows.store_window("small", 0, vec![0u8; 1 << 20]);
        windows.admit("huge".to_string(), held(96));
        windows.store_window("huge", 0, vec![0u8; 96 << 20]);

        // Over budget but kept: dropping it would refetch what was just fetched.
        assert!(windows.by_key["huge"].window.is_some());
        assert!(
            windows.by_key["small"].window.is_none(),
            "everything else gave way"
        );
    }

    /// The same file read twice costs a `head` and no transfer.
    #[tokio::test]
    async fn a_file_read_twice_is_fetched_once() {
        let (store, fs) = store();
        put(&store, "doc.pdf", b"the whole document").await;
        assert_eq!(read(&fs, "doc.pdf", 18).await, b"the whole document");

        // A second read that answers after the delete never went back to the store.
        store.delete(&os_path("doc.pdf").unwrap()).await.unwrap();
        assert_eq!(read(&fs, "doc.pdf", 18).await, b"the whole document");
    }

    #[test]
    fn listings_are_given_up_oldest_first() {
        let mut kept = Listings::default();
        for i in 0..MAX_CACHED_LISTINGS + 2 {
            kept.admit(format!("dir-{i}"), Arc::new(vec![]));
        }
        assert!(kept.get("dir-0").is_none(), "the first one listed went");
        assert!(
            kept.get(&format!("dir-{}", MAX_CACHED_LISTINGS + 1))
                .is_some()
        );
        assert_eq!(kept.by_prefix.len(), MAX_CACHED_LISTINGS);

        // Re-listing a prefix it already holds replaces it rather than queueing it twice.
        let before = kept.order.len();
        kept.admit(format!("dir-{}", MAX_CACHED_LISTINGS + 1), Arc::new(vec![]));
        assert_eq!(kept.order.len(), before);

        kept.clear();
        assert!(
            kept.get(&format!("dir-{}", MAX_CACHED_LISTINGS + 1))
                .is_none()
        );
    }

    /// A grown object under a filled entry: without revalidation the remembered size clamps
    /// the read to the old length as EOF, while `stat` reports the new one.
    #[tokio::test]
    async fn a_grown_object_is_read_whole_once_a_stat_has_seen_it() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        assert_eq!(read(&fs, "a.txt", 8).await, b"aaaa");

        put(&store, "a.txt", b"bbbbbbbb").await;
        // No `stat` yet, so the entry still stands: reads alone spend no `head`.
        assert_eq!(read(&fs, "a.txt", 8).await, b"aaaa");

        assert_eq!(fs.stat(Path::new("a.txt")).await.unwrap().size, 8);
        assert_eq!(read(&fs, "a.txt", 8).await, b"bbbbbbbb");
    }

    /// Same length, different bytes: invisible to size and a coarse timestamp, caught by the
    /// etag.
    #[tokio::test]
    async fn a_replacement_of_the_same_length_is_caught_by_the_etag() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        assert_eq!(read(&fs, "a.txt", 4).await, b"aaaa");

        put(&store, "a.txt", b"cccc").await;
        fs.stat(Path::new("a.txt")).await.unwrap();
        assert_eq!(read(&fs, "a.txt", 4).await, b"cccc");
    }

    /// A key a console turns into a folder (0-byte marker, keys under it) must refuse reads
    /// rather than serve the old window.
    #[tokio::test]
    async fn a_key_that_becomes_a_prefix_is_no_longer_readable() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        assert_eq!(read(&fs, "a.txt", 4).await, b"aaaa");

        put(&store, "a.txt/inner", b"x").await;
        put(&store, "a.txt", b"").await;
        assert_eq!(
            fs.stat(Path::new("a.txt")).await.unwrap().kind,
            DirentKind::Dir
        );

        let err = fs
            .read_at(Path::new("a.txt"), &mut [0u8; 4], 0)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::IsADirectory);
    }

    /// A failed `head` leaves the entry, so a deleted object stays readable and a transient
    /// failure costs the next read no round trip.
    #[tokio::test]
    async fn a_failed_head_leaves_the_entry_alone() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        assert_eq!(read(&fs, "a.txt", 4).await, b"aaaa");

        store.delete(&os_path("a.txt").unwrap()).await.unwrap();
        assert_eq!(
            fs.stat(Path::new("a.txt")).await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(lock(&fs.windows).by_key.contains_key("a.txt"));
    }

    /// Re-admitting a key must not queue it twice, or the duplicate is evicted while the entry
    /// is live and every later eviction comes one entry early.
    #[tokio::test]
    async fn a_re_read_key_holds_one_slot() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        read(&fs, "a.txt", 4).await;
        lock(&fs.windows).forget("a.txt");
        read(&fs, "a.txt", 4).await;

        let windows = lock(&fs.windows);
        assert_eq!(windows.order.len(), windows.by_key.len());
    }
}
