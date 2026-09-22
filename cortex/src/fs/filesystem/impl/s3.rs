//! A read-only [`FileSystem`] store over an object store (S3 and compatibles).
//!
//! Object keys map to paths and a directory is a key prefix — that is the whole of the
//! mapping. There is no directory object to create or remove; two keys sharing a prefix *are*
//! a directory. Metadata comes from `head`, listings from `list_with_delimiter`, and file data
//! from ranged GETs.
//!
//! # Read-only, and why that is a design rather than an omission
//!
//! [`FileSystem::write_at`] addresses a byte offset. Answering that over S3 means reading the
//! whole object, patching it, and putting it back — `object_store` does not expose a byte-range
//! patch, and its multipart API requires parts of at least 5 MiB where a guest's writes are at
//! most 1 MiB. So a write needs staging, with a ceiling on it (a guest picks the offset, so an
//! allocation sized from one is a guest-chosen allocation).
//!
//! And staging needs somewhere to end: a multipart upload has to be *completed*, and this trait
//! has no signal for when a writer is done (see *Durability* on [`FileSystem`]). That is a
//! separate piece of work, waiting on a design that has one. Every write here keeps the trait's
//! `ReadOnlyFilesystem` default.
//!
//! `ReadOnlyFilesystem` and not [`Unsupported`](io::ErrorKind::Unsupported): the store *could*
//! write, it is this implementation that will not, and userspace acts on the difference.
//!
//! # Async
//!
//! `object_store` is async and so is [`FileSystem`], so each operation `.await`s the client
//! directly — no runtime lives here. An async-native consumer drives it with its own runtime; a
//! sync interface binding (fuse/fuse-t) `block_on`s at its callback boundary.

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

// `S3Config` lives in `volume/spec.rs`, not here. It is a wire type: a build without
// this feature still has to parse a spec that names an S3 volume, so the settings
// cannot be behind the feature the *provider* is behind. Its hand-written `Debug`,
// which redacts the secret, went with it.

/// Connection settings for an [`S3Fs`].
///
/// `Serialize` because a caller may well keep its configuration in a file. The hand-written
/// `Debug` below redacts the secret, because a log is not that file.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Custom endpoint (MinIO / R2 / localstack); `None` for real AWS.
    pub endpoint: Option<String>,
    /// Key prefix every path is rooted under. Composes with whatever mount path a
    /// [`ContextFs`](crate::fs::ContextFs) puts this store at: the mount table strips its own path
    /// first, then this prefix is prepended.
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

/// An object store's keys, served as a tree.
///
/// Read-only, and the module doc says why that is a design rather than an omission.
pub struct S3Fs {
    store: Arc<dyn ObjectStore>,
    /// Normalised to carry no leading or trailing `/`, so [`Self::key`] can join
    /// with `/` unconditionally.
    prefix: String,
    /// What the store answered when a directory was last listed.
    listings: Mutex<Listings>,
    /// What reads have learned about keys, and what they read ahead into.
    ///
    /// Taken through [`lock`], which ignores poisoning: a panic anywhere else must not turn
    /// every later read into a panic of its own — a binding that serves its device from a
    /// single worker thread would lose the whole mount to one poisoned lock.
    windows: Mutex<Windows>,
}

impl S3Fs {
    /// Build an S3 client from `cfg`.
    pub fn new(cfg: &S3Config) -> io::Result<Self> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region)
            .with_access_key_id(&cfg.access_key_id)
            .with_secret_access_key(&cfg.secret_access_key);
        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.with_endpoint(endpoint).with_allow_http(true);
        }
        // Synchronous — the builder just assembles config; requests run later on
        // whatever runtime drives the async ops.
        let store = builder.build().map_err(to_io_error)?;
        Ok(Self::with_store(
            Arc::new(store),
            cfg.key_prefix.clone().unwrap_or_default(),
        ))
    }

    /// Wrap an already-built store.
    ///
    /// Deliberately not `pub`. Widening it would let this type be assembled over
    /// any `ObjectStore` — GCS, Azure, even `LocalFileSystem` — which may well be
    /// worth doing, but not under the name `S3Fs` and not without its own
    /// design pass. Tests reach it as a sibling module.
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
    pub async fn check_reachable(&self) -> io::Result<()> {
        self.list(Path::new("")).await?;
        Ok(())
    }

    /// Map a request path to an object key, rooted under [`S3Config::key_prefix`].
    ///
    /// `..` and OS prefixes are rejected rather than resolved, so a request can
    /// never address a key outside the configured prefix — the same rule
    /// [`PassthroughFs`] applies to its root.
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
/// `Path::parse` preserves bytes that are special in a URL (`%`, `#`, …), so a key
/// surfaced by a listing round-trips to the same object on read.
fn os_path(key: &str) -> io::Result<OsPath> {
    OsPath::parse(key).map_err(|_| io::ErrorKind::InvalidFilename.into())
}

/// Translate an object-store error into the kind this crate answers with.
///
/// Every variant that exists today is named, the way the errno tables in the bindings are.
/// Unlike those, the wildcard is **required**: `object_store::Error` is `#[non_exhaustive]`,
/// so an external crate cannot match it exhaustively — a variant added upstream falls to the
/// catch-all and reaches userspace as `EIO`.
///
/// Anything not classified is wrapped with `io::Error::other`, never with a raw OS error:
/// `host_errno` forwards `raw_os_error()` when there is one, so a preserved transport errno
/// would reach userspace as itself rather than as `EIO`. Wrapping also keeps the upstream
/// error as the `source`, which a classified kind on its own would drop.
fn to_io_error(err: object_store::Error) -> io::Error {
    use object_store::Error;
    let kind = match &err {
        Error::NotFound { .. } => io::ErrorKind::NotFound,
        Error::AlreadyExists { .. } => io::ErrorKind::AlreadyExists,
        Error::InvalidPath { .. } => io::ErrorKind::InvalidFilename,
        Error::PermissionDenied { .. } => io::ErrorKind::PermissionDenied,
        // Credentials missing or expired. A claim about the caller, not about the filesystem,
        // so not `Unsupported`; `EACCES` is what userspace can act on.
        Error::Unauthenticated { .. } => io::ErrorKind::PermissionDenied,
        Error::UnknownConfigurationKey { .. } => io::ErrorKind::InvalidInput,
        // These two *are* claims about the filesystem — the store cannot do the operation at
        // all — which is what `Unsupported` means.
        Error::NotSupported { .. } => io::ErrorKind::Unsupported,
        Error::NotImplemented { .. } => io::ErrorKind::Unsupported,
        // `Precondition`/`NotModified` are only reachable once this store sends conditional
        // requests, which it does not. Left unclassified with the rest: the right answer
        // would be `ESTALE`, which `io::ErrorKind` has no name for.
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err)
}

/// Read-ahead window size. One miss fetches this much and serves subsequent reads
/// inside it from memory, which is the difference between one round trip and one
/// per request — a guest asks in 128 KiB pieces at most (32 KiB through FUSE-T).
const READAHEAD_CHUNK: u64 = 8 << 20;

/// How many keys may hold a window at once.
///
/// A cap is needed *because* the windows are keyed by path rather than by an open. A
/// per-open window is bounded by however many files a consumer has open at once — which is
/// to say, not bounded at all, only invisible. This one is a number.
///
/// It is not the memory bound, though it was written as one. A window is as large as the
/// read that filled it: [`READAHEAD_CHUNK`] for a sequential reader asking in guest-sized
/// pieces, but a caller that reads a whole file in one call — which is what a window
/// holding a document for a viewer does — gets a window the size of the file. Eight of
/// those is however large eight files are. [`MAX_CACHED_BYTES`] is the bound.
const MAX_CACHED_KEYS: usize = 8;

/// How many bytes of object bodies are held across every entry.
///
/// The number the count above was assumed to give. Keeping whole bodies is what makes a
/// file opened twice cost one `head` and no transfer, and it is worth doing — but a reader
/// that opens six large documents should not be six large documents of memory. Past this,
/// the oldest entries give up what they hold.
///
/// Never the entry just filled, however large it is: that one is what the read in progress
/// is being served from, and dropping it would send the same bytes over the wire again to
/// answer the call that just fetched them.
const MAX_CACHED_BYTES: u64 = 64 << 20;

/// What the store remembers about one key between reads.
///
/// Keyed by path, so an entry outlives every open of it — there is no open to end its life
/// (see [`FileSystem`]), and nothing but eviction would ever drop it. Two things bound how
/// long what it says can stay wrong: [`MAX_CACHED_KEYS`] on how many are kept at all, and
/// [`S3Fs::revalidate`] on whether a kept one still describes the object.
struct ReadCache {
    /// Size as of the `head` that filled this entry.
    ///
    /// Held so a read past the end answers without a round trip, and so ranges can be
    /// clamped rather than sent and rejected — an out-of-range GET comes back as the
    /// generic error variant, indistinguishable from a transport failure.
    ///
    /// Being a clamp and not just a number is why staleness here is not a small matter: a
    /// grown object read against a remembered smaller size answers `Ok(0)` at the old end,
    /// which the contract says is EOF. Hence [`Self::describes`].
    size: u64,

    /// What the object was when this entry was filled, as the store names it — the pair
    /// [`Self::describes`] compares a later `head` against.
    ///
    /// `etag` is a content fingerprint and settles it alone. `mtime` is kept for the stores
    /// that report no tag, where size and timestamp together are all there is to compare.
    etag: Option<String>,

    mtime: SystemTime,

    /// The last fetched window as `(start, bytes)`.
    window: Option<(u64, Vec<u8>)>,

    /// Where the previous read of this key ended, or `None` before the first one.
    ///
    /// This is how sequential access is recognised — a read that starts where the last one
    /// ended earns read-ahead, anything else is served at exactly the size asked for.
    /// Request size cannot serve as the signal: one consumer path sends a uniform size
    /// whether access is sequential or random.
    ///
    /// Assigned, not folded with `max`. A monotonic value would pin itself to the highest
    /// offset ever read, and then nothing below that can ever equal it — so a reader that
    /// peeks at the tail once and then streams from the front (a zip's central directory, an
    /// ELF's section headers) loses read-ahead for the whole file.
    ///
    /// Being per *key* rather than per open, two consumers reading one object interleave
    /// here, and each costs the other a read-ahead. The store cannot tell them apart — a
    /// path is not an open (see [`FileSystem`]) — and the answer stays correct either way:
    /// what a wrong guess costs is one round trip, never a wrong byte.
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
    /// A tag on both sides settles it by itself: that is what an etag is for, and it catches
    /// the replacement that keeps the length. With a tag missing from either side there is
    /// only size and timestamp, which agree for an object rewritten to the same length within
    /// the store's timestamp resolution — a weaker answer, and the strongest one available.
    fn describes(&self, meta: &ObjectMeta) -> bool {
        match (&self.etag, &meta.e_tag) {
            (Some(held), Some(fresh)) => held == fresh,
            _ => self.size == meta.size && self.mtime == SystemTime::from(meta.last_modified),
        }
    }
}

/// How long a listing is handed out again without asking the store.
///
/// A listing here is one request, sometimes a few — not the walk a page render is — so this
/// is not about a cold tree. It is about the same directory being asked for twice in a row,
/// which is what a reader walking a tree does, and what an agent does every time it runs
/// `ls` in a loop. Nothing in this store writes, so what changes a listing is someone else's
/// doing; a reader who can see that has [`FileSystem::forget`].
const LISTING_TTL: Duration = Duration::from_secs(30);

/// How many directories' listings are kept.
const MAX_CACHED_LISTINGS: usize = 256;

/// The listings kept, and the order to give them up in.
#[derive(Default)]
struct Listings {
    by_prefix: HashMap<String, (Instant, Arc<Vec<Dirent>>)>,
    /// Prefixes in the order they were first listed. Insertion order for the reason
    /// [`Windows::order`] gives.
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

    /// Keys in the order they were first cached, so the oldest goes first. Insertion order
    /// rather than true LRU: the eviction it gets wrong is one round trip, and a
    /// use-ordered queue would have to be touched on every read — under the mutex that
    /// every read already contends for.
    ///
    /// Holds exactly the keys `by_key` holds, which is what makes its length the cap. A key
    /// left here after its entry went would spend a slot on nothing and then evict a live
    /// entry when it came up.
    order: VecDeque<String>,
}

impl Windows {
    /// Make room for `key` and record it, evicting the oldest entry once the cap is passed.
    ///
    /// Only a key the map did not already hold joins the queue. Two reads can race to admit
    /// the same key — each one saw a miss — and a second copy in the queue would come up for
    /// eviction while the entry is still in use.
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
    /// The eviction is here rather than at the call site because this is the only place a
    /// window grows, and a budget checked anywhere else is a budget that holds until someone
    /// adds a second writer.
    fn store_window(&mut self, key: &str, at: u64, data: Vec<u8>) {
        let Some(cache) = self.by_key.get_mut(key) else {
            return;
        };
        cache.window = Some((at, data));
        while self.held_bytes() > MAX_CACHED_BYTES {
            // The oldest entry that is not the one just filled. Nothing is dropped when that
            // is the only one holding anything — see `MAX_CACHED_BYTES`.
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
                // The window goes, the entry stays: what it knows about the object — its
                // size, its etag — is what lets the next read clamp a range and revalidate,
                // and that costs nothing to keep.
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
fn children_from(listed: object_store::Result<object_store::ListResult>) -> io::Result<bool> {
    let listed = listed.map_err(to_io_error)?;
    Ok(!listed.common_prefixes.is_empty() || !listed.objects.is_empty())
}

impl S3Fs {
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
    /// Shared by `stat` and the first read of a key, so the two cannot disagree about
    /// a name — the same reason `list` decides collisions from the object's size rather
    /// than by consulting `stat`.
    ///
    /// It is also where the read cache is revalidated, because this is where a fresh `head`
    /// already is: every `stat` spends one, and what it reports is exactly what says whether
    /// a cached entry still describes the object. See [`Self::revalidate`].
    async fn classify(&self, path: &Path) -> io::Result<Entry> {
        let key = self.key(path)?;
        // The volume's own root is a directory by construction — there need not be
        // any object or prefix of that name, and every mount opens by asking for it.
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
            // No object with that exact key. Still a directory if keys live
            // under it — a prefix is not an object.
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
    /// A listing is how this is asked, and an absent prefix answers with an empty
    /// result rather than an error — so emptiness, not failure, is what says "no".
    /// Without that distinction every path would look like a directory.
    ///
    /// An error is not read as "no children" — [`children_from`] carries that rule.
    /// What follows from it reaches callers: a bad credential or a missing bucket
    /// surfaces as `EIO` on any `stat` that gets here, because an object store reports
    /// a failed listing generically whatever status came back. `head` and `get` do
    /// carry the status, so a name that exists still answers `EACCES`.
    async fn has_children(&self, prefix: &OsPath) -> io::Result<bool> {
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

/// Three methods, which is all a read-only store implements: everything that would change
/// something keeps the trait's `ReadOnlyFilesystem` default. A caller that meant to write
/// hears it on the write rather than on an open, there being no open to hear it on.
impl FileSystem for S3Fs {
    /// Metadata for one key, or for the prefix of that name — see `classify`.
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
    /// One request. `list_with_delimiter` rolls deeper keys into the prefix they sit in and
    /// hands back the objects at this level with their metadata already attached, which is
    /// what lets [`Dirent::with_stat`] answer a `readdirplus` without a `stat` per name.
    ///
    /// Two things have to be untangled first, both consequences of a flat key space
    /// pretending to be a tree:
    ///
    /// * The listed prefix can come back as an object of its own — a folder marker. That is
    ///   *this* directory, not something in it, so it is dropped.
    /// * A name can arrive from both sides at once: as a prefix (because keys live under it)
    ///   and as an object (because a key of exactly that name exists). A `readdir` may not
    ///   repeat a name, so one side has to go — see `resolve_collision`.
    ///
    /// Answered from the last listing while that is still recent. See [`LISTING_TTL`]: what a
    /// listing says may be a little behind, and what a *read* says may not be — a stale name
    /// costs a reader a second look, a stale size or a stale body is a reader handed the wrong
    /// file. `stat` and `read_at` ask every time, and the sizes a kept listing carries are the
    /// ones it was given.
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
    /// The read side cannot be stale — a `stat` revalidates it against a fresh `head` — so
    /// this is mostly about the listings. It goes too because a reader who asks to look
    /// again has said, as plainly as the protocol allows, that they do not want an answer
    /// from anything kept here.
    fn forget<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            lock(&self.listings).clear();
            *lock(&self.windows) = Windows::default();
        })
    }

    /// Fill `buf` from `offset`, fetching only what the cache cannot answer.
    ///
    /// Four things are load-bearing:
    ///
    /// **The first read of a key costs a `head`.** The size has to be known before a range
    /// can be clamped, and a path plane has no open to have learned it at — so the first
    /// read classifies the key and remembers what it found. A directory is refused here, and
    /// so is the marker case (a 0-byte object standing for a prefix), or a guest would read a
    /// directory as an empty file.
    ///
    /// **The range is clamped, never rejected.** A GET that starts at or past the end comes
    /// back as the store's generic error, which is indistinguishable from a transport failure
    /// — so the end is decided here, from the remembered size, and a read beyond it answers
    /// `Ok(0)` without a round trip.
    ///
    /// **A short return means EOF and nothing else.** That is the contract [`FileSystem`]
    /// states, and `Posix` believes it — it calls this once and passes the length on. So a
    /// request the cached window only partly covers must not stop there: the loop fills the
    /// remainder, or a file would appear to end at a window boundary.
    ///
    /// **Read-ahead is earned by continuity, not by request size.** A read that starts where
    /// the last one ended fetches a whole window; anything else fetches exactly what was
    /// asked.
    ///
    /// The lock is never held across a fetch, and never across the `head` either.
    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            let key = self.key(path)?;
            let (location, size) = self.located(path, &key).await?;
            // `Bounded(0..0)` is an error rather than an empty answer, so an empty request is
            // settled here — and without disturbing `last_end`, which a zero-length read in
            // the middle of a stream would otherwise push off the sequence.
            if buf.is_empty() || offset >= size {
                return Ok(0);
            }
            let want = (buf.len() as u64).min(size - offset) as usize;
            let buf = &mut buf[..want];

            let mut filled = 0usize;
            while filled < want {
                let at = offset + filled as u64;

                // One acquisition for both questions: nothing waits on the network between
                // them, so splitting it would only widen the chance of an answer from one
                // state and a decision from another.
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
                    // Defensive. A store answering a non-empty range with no bytes has no
                    // defined meaning here, and looping on it would not terminate.
                    //
                    // Not the stale-size path: an object that shrank makes the range invalid,
                    // so the store errors and `?` carries that out of `fetch` — which is
                    // right, because shortening the return instead would say this was EOF.
                    break;
                }
                let n = data.len().min(want - filled);
                buf[filled..filled + n].copy_from_slice(&data[..n]);
                filled += n;
                lock(&self.windows).store_window(&key, at, data);
            }

            if filled > 0 {
                // Assigned, not folded — see `ReadCache::last_end`.
                if let Some(cache) = lock(&self.windows).by_key.get_mut(&key) {
                    cache.last_end = Some(offset + filled as u64);
                }
            }
            Ok(filled)
        })
    }
}

impl S3Fs {
    /// Where `key` lives in the store and how big it is, from the cache if it is there and
    /// from a `head` if it is not.
    ///
    /// The location comes from the `head` that classified it rather than being mapped again,
    /// so a read cannot end up pointed at a different key than the one that was inspected.
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
    /// Without this, an entry survives every open of its key and is bounded only by eviction,
    /// so an object replaced under a mount is served from the old window and clamped to the
    /// old size until seven other keys push it out. Meanwhile `stat` answers from a `head`
    /// every time: the two would disagree, and the one a guest acts on — the size it was told
    /// — is the one the read path would refuse to honour. Worse, this store reports `mtime`
    /// precisely so a guest negotiating `AUTO_INVAL_DATA` drops its own pages when the object
    /// moves; a cache that ignored the same signal would hand the same stale bytes straight
    /// back.
    ///
    /// A `Dir` verdict drops the entry too: the key does not name a file any more, so nothing
    /// remembered about reading it is true.
    ///
    /// Two limits, both deliberate:
    ///
    /// * **Freshness is only as frequent as the `stat`s.** A replacement between one `stat`
    ///   and the reads that follow is not caught here, and how often a kernel revalidates is
    ///   the kernel's business. Catching it *within* a read means conditional GETs — see the
    ///   note on `Precondition` in [`to_io_error`] — which is a separate change: it turns a
    ///   replacement into an error mid-stream, and that needs an errno this crate can express.
    /// * **A failed `head` changes nothing.** Only a positive answer is acted on. A transient
    ///   failure must not cost a re-`head` on the next read, and a key deleted from under an
    ///   open is a case POSIX says keeps reading — the entry expiring by eviction is closer to
    ///   right than dropping it the moment a `stat` says `ENOENT`.
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

    /// One ranged GET. No lock is held here — see the loop in
    /// [`read_at`](FileSystem::read_at).
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
/// Zero means the window does not cover `at` — it may still cover *part* of what was asked
/// for, which is why the caller loops rather than treating a partial answer as the end.
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
/// Over `InMemory` rather than a real bucket: what is under test is this file's bookkeeping,
/// and a store that answers locally still answers with `head`, ranged GETs and an etag per
/// version — which is the whole of what the cache reads. The live-endpoint tests
/// (`tests/s3_endpoint.rs`) cover the parts only a real S3 can disagree about.
#[cfg(test)]
mod tests {
    use object_store::{PutPayload, memory::InMemory};

    use super::*;

    /// The store and a filesystem over it, so a test can change an object behind the mount the
    /// way something outside it would.
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

    /// A listing is kept, and what is kept is what a reader sees until it is not.
    ///
    /// Deleting the object *behind* the store is how a test asks whether the answer came from
    /// the store or from the cache: nothing else can tell the two apart, and counting requests
    /// would pin how the listing is fetched rather than that it is not fetched twice.
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

        // What a reader says when they can see that the bucket has moved on.
        fs.forget().await;
        assert_eq!(fs.list(Path::new("/")).await.unwrap().len(), 1);
    }

    /// A read is not a listing: it asks every time, so a file's bytes are never a guess.
    #[tokio::test]
    async fn a_read_still_asks_after_a_listing_was_kept() {
        let (store, fs) = store();
        put(&store, "a.txt", b"one").await;
        fs.list(Path::new("/")).await.unwrap();

        put(&store, "a.txt", b"two").await;
        // `stat` heads the object and revalidates the read window with what it finds.
        assert_eq!(fs.stat(Path::new("a.txt")).await.unwrap().size, 3);
        assert_eq!(read(&fs, "a.txt", 3).await, b"two");
    }

    /// A whole body kept for one key, sized `mib`.
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
        // The entry itself stays: its size and etag are what clamp and revalidate the next
        // read, and they cost nothing.
        assert_eq!(windows.by_key["a"].size, (32 * mib) as u64);
    }

    #[test]
    fn a_body_larger_than_the_budget_is_still_what_the_read_is_served_from() {
        let mut windows = Windows::default();
        windows.admit("small".to_string(), held(1));
        windows.store_window("small", 0, vec![0u8; 1 << 20]);
        windows.admit("huge".to_string(), held(96));
        windows.store_window("huge", 0, vec![0u8; 96 << 20]);

        // Over budget, and kept anyway: dropping it would send the bytes that were just
        // fetched over the wire again, to answer the call that fetched them.
        assert!(windows.by_key["huge"].window.is_some());
        assert!(
            windows.by_key["small"].window.is_none(),
            "everything else gave way"
        );
    }

    /// The case this is all for: the same file opened twice costs a `head` and no transfer.
    #[tokio::test]
    async fn a_file_read_twice_is_fetched_once() {
        let (store, fs) = store();
        put(&store, "doc.pdf", b"the whole document").await;
        assert_eq!(read(&fs, "doc.pdf", 18).await, b"the whole document");

        // Take the object away behind the store: a second read that still answers is one
        // that never went back for it.
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

    /// The case the revalidation exists for: a longer object under a filled entry. The
    /// remembered size is a clamp, so without the drop the read stops at the old length and
    /// reports it as EOF — while `stat` is already answering with the new one.
    #[tokio::test]
    async fn a_grown_object_is_read_whole_once_a_stat_has_seen_it() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        assert_eq!(read(&fs, "a.txt", 8).await, b"aaaa");

        put(&store, "a.txt", b"bbbbbbbb").await;
        // Nothing has asked about the name yet, so the entry still stands — this is the
        // documented limit, not an accident: reads alone spend no `head`.
        assert_eq!(read(&fs, "a.txt", 8).await, b"aaaa");

        assert_eq!(fs.stat(Path::new("a.txt")).await.unwrap().size, 8);
        assert_eq!(read(&fs, "a.txt", 8).await, b"bbbbbbbb");
    }

    /// Same length, different bytes — invisible to size and to a timestamp of coarse enough
    /// resolution. The etag is what catches it.
    #[tokio::test]
    async fn a_replacement_of_the_same_length_is_caught_by_the_etag() {
        let (store, fs) = store();
        put(&store, "a.txt", b"aaaa").await;
        assert_eq!(read(&fs, "a.txt", 4).await, b"aaaa");

        put(&store, "a.txt", b"cccc").await;
        fs.stat(Path::new("a.txt")).await.unwrap();
        assert_eq!(read(&fs, "a.txt", 4).await, b"cccc");
    }

    /// A key that stops naming a file at all: an object store console turning it into a folder
    /// leaves a 0-byte marker with keys under it. Nothing remembered about reading it survives
    /// that, and the read has to refuse rather than serve the old window.
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

    /// A `head` that fails says nothing about what a reader holds. The entry stays, which is
    /// what keeps an object deleted from under a reader readable — and keeps a transient
    /// failure from charging the next read a round trip.
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

    /// Re-admitting a key must not queue it twice, or the duplicate comes up for eviction
    /// while the entry is live — and every eviction after it is one entry early.
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
