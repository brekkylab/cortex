use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use tokio::sync::Mutex;
use unicode_normalization::UnicodeNormalization;

use super::accessor::{DOWNLOAD_URL_KEY, OnedriveAccessor, OnedriveConfig};
use crate::{
    BoxFuture,
    fs::filesystem::{Dirent, DirentKind, FileSystem, Stat},
};

/// Per-directory listing TTL.
///
/// One number for the listing and for the span held under it, deliberately. Two would mean
/// a window in which `ls` answers from one snapshot while a read of the same file answers
/// from another — and everything a `stat` reports (size, mtime, etag) comes off the parent
/// listing, so the two are describing the same snapshot or they are lying about one.
///
/// This is what bounds how stale the tree can be. A network filesystem always has such a
/// number: macOS's own NFS client caches attributes (`acregmin`/`acregmax`) and serves them
/// while the server moves on. This one is longer because a listing here costs a round trip
/// rather than a LAN hop.
const DIR_TTL: Duration = Duration::from_secs(300);

/// How much a read fetches once it is clear the reader is walking the file *and* nothing
/// else is being walked, so a lone walk pays a round trip per span of this size rather
/// than one per window.
///
/// The kernel's window is 64 KiB (32 KiB through FUSE-T) and not ours to choose; one ranged
/// request per window leaves a large read crawling at round-trip speed.
///
/// It is a ceiling rather than the size every walk gets: [`HELD_BUDGET`] is divided among
/// whatever is being walked, and this is what a lone reader's share comes to. Shrinking it
/// instead of dividing it would multiply every large read's round trips, each a sizable
/// fraction of a second to the download host.
const READ_SPAN: u64 = 64 * 1024 * 1024;

/// How much a *first* read fetches, before anything says the reader is walking.
///
/// Two sizes because a span is not free either: the tools that read a file's head and stop
/// would pay a whole `READ_SPAN` for one buffer. Well above the bare window because NFS
/// fires read-ahead the moment a file is touched, and every window of it looks like a walk;
/// a span smaller than that read-ahead is spent before the distinction can be made.
const FIRST_SPAN: u64 = 8 * 1024 * 1024;

/// Cap on how many children one listing will hold.
const MAX_FOLDER_ITEMS: usize = 10_000;

/// Cap on retained listings, so a traversal cannot grow the map without bound. A dropped
/// listing costs a request, not correctness.
const MAX_CACHED_DIRS: usize = 4_096;

type CachedListing = (Instant, Arc<Vec<Child>>);

/// Ceiling on the bytes held across every file at once. See [`OnedriveFs::held`].
///
/// One [`READ_SPAN`], so concurrent readers divide what a lone walk holds.
const HELD_BUDGET: u64 = READ_SPAN;

/// Floor on one file's share of [`HELD_BUDGET`].
///
/// For the case the count cannot tell apart: many files touched once beside one being
/// walked. [`ACTIVE`] keeps a file counted for seconds after its single read, so without a
/// floor a traversal past many small files would slice the budget thin and multiply the
/// requests of the one large file being walked.
///
/// It costs traffic when the files really are all being walked: past
/// `HELD_BUDGET / MIN_SPAN` of them the shares stop fitting and eviction comes back.
const MIN_SPAN: u64 = 4 * 1024 * 1024;

/// How recently a span must have been read from for its file to count as one of the
/// readers dividing [`HELD_BUDGET`].
///
/// Being in the map is the wrong test: an entry lives for [`DIR_TTL`], so files a traversal
/// abandoned would keep dividing the budget while one file is walked. Seconds rather than
/// milliseconds because a miss against Graph costs about a second, so two files alternating
/// touch each other's spans that far apart; what matters is the interval between *misses* on
/// one file, since a hit refreshes [`HeldSpan::used`].
const ACTIVE: Duration = Duration::from_secs(3);

/// One span of one file, and where in it the span begins.
struct HeldSpan {
    at: u64,
    to_eof: bool,
    /// When the bytes were fetched. Decides the TTL, so it is not touched by a read.
    when: Instant,
    /// When a read last came out of these bytes. Decides which span is evicted to make
    /// room, which has to be the one nobody is walking rather than the oldest fetch.
    used: Instant,
    bytes: Arc<Vec<u8>>,
}

/// One resolved entry, as the mount means it.
#[derive(Clone)]
struct Child {
    /// The item's own id, which is how anything asked of the service *after* the listing
    /// addresses it. Opaque ASCII the service minted, so unlike a path it has one
    /// spelling and it survives a rename. See
    /// [`get_item_by_id`](OnedriveAccessor::get_item_by_id).
    id: String,
    /// Listing name: the item's own name, sanitized into a single path segment.
    name: String,
    is_dir: bool,
    /// Exact, for every file: nothing has to be produced to learn it, so nothing has to
    /// stand in for it.
    size: u64,
    mtime: Option<SystemTime>,
    created: Option<SystemTime>,
    /// `cTag` where there is one, else `eTag`. The content tag is the one a revalidating
    /// reader wants; a folder has no `cTag` at all.
    etag: Option<String>,
    /// A preauthenticated URL for the bytes, as the listing returned it.
    ///
    /// Held only as long as the listing that carried it. Microsoft calls it short-lived
    /// and says outright that it "can't be cached"; an expired one answers 4xx, which
    /// [`OnedriveFs::span`] turns into exactly one refetch.
    download_url: Option<String>,
}

/// A OneDrive account as a read-only directory tree.
pub struct OnedriveFs {
    accessor: OnedriveAccessor,
    /// Folder path → its children. Everything `stat` reports comes from here, so a
    /// listing is what makes the kernel's per-entry `getattr` storm free.
    dir_cache: Mutex<HashMap<String, CachedListing>>,
    /// File path → the span last read from it, bounded in bytes by [`HELD_BUDGET`].
    ///
    /// A map rather than one slot, because reads of two files interleave with neither
    /// threads nor a second process: FUSE ops are serialized, so alternating is enough.
    /// With one slot each read finds the other file's span there, no read ever counts as a
    /// walk, and every window pays a whole [`FIRST_SPAN`].
    ///
    /// A single tool rarely interleaves on a network store: read-ahead overlaps files only
    /// when it can outrun the reader, and a miss of about a second never lets it. So a trace
    /// replayed from a fast local store does not describe this one.
    ///
    /// Bounded by *bytes* rather than count, so the ceiling is divided rather than owned by
    /// whoever fetched last, and each span is sized to that division so nothing has to be
    /// evicted for a new span to fit. See [`READ_SPAN`] and [`ACTIVE`].
    held: Mutex<HashMap<String, HeldSpan>>,
}

impl OnedriveFs {
    pub fn new(config: &OnedriveConfig) -> anyhow::Result<Self> {
        Ok(Self {
            accessor: OnedriveAccessor::new(config)?,
            dir_cache: Mutex::new(HashMap::new()),
            held: Mutex::new(HashMap::new()),
        })
    }

    /// One folder's children, from the cache when one is fresh.
    ///
    /// A failed listing is *not* cached. Caching the failure would turn one throttled
    /// request into a whole `DIR_TTL` of an empty directory, which reads as "the folder is
    /// gone" rather than "ask again".
    ///
    /// This is the one call left that addresses the service by a path, and a path has two
    /// Unicode spellings. See the fallback below.
    async fn list_dir(&self, folder: &str) -> io::Result<Arc<Vec<Child>>> {
        {
            let cache = self.dir_cache.lock().await;
            if let Some((at, children)) = cache.get(folder)
                && at.elapsed() < DIR_TTL
            {
                return Ok(children.clone());
            }
        }
        let rows = match self.accessor.list_children(folder, MAX_FOLDER_ITEMS).await {
            Ok(rows) => rows,
            Err(e) if is_not_found(&e) => self.list_unspellable(folder, e).await?,
            Err(e) => return Err(not_found_or_backend(e)),
        };
        let children: Vec<Child> = rows.iter().filter_map(child_from_item).collect();
        let children = Arc::new(children);
        let mut cache = self.dir_cache.lock().await;
        // Drop what has aged out on the way in: nothing else removes an entry, so one
        // listing per folder ever visited would otherwise stay for the life of the mount.
        cache.retain(|_, (at, _)| at.elapsed() < DIR_TTL);
        if cache.len() >= MAX_CACHED_DIRS {
            cache.clear();
        }
        cache.insert(folder.to_string(), (Instant::now(), children.clone()));
        Ok(children)
    }

    /// The folder a path names when the path, as spelled, names nothing.
    ///
    /// The service answers to the spelling it stored and 404s every other one, and macOS
    /// hands a lookup the decomposed spelling of whatever the listing printed.
    /// So after the path as given, two things are tried, cheapest first.
    ///
    /// **Composed.** One request, and it answers the ordinary case: a macOS reader asking
    /// for a folder the service stored composed. The path as given goes first because the
    /// two spellings can name two different sibling folders, and only that order returns
    /// the one the caller's bytes meant.
    ///
    /// **Then segment by segment.** A whole-path spelling is all-composed or
    /// all-decomposed, and a tree touched by two clients can be neither: a folder made on
    /// the web under a child made by the macOS sync client is composed then decomposed.
    /// Such a path would `stat` (since [`resolve`](Self::resolve) normalizes per segment)
    /// while `list` says ENOENT. So the last resort resolves the folder through its
    /// parent's listing, where `same_name` settles each segment on its own, and lists by
    /// the id that comes back. Only a path that was going to fail pays for that, and the
    /// parents are usually already cached because the kernel looked each one up on the way
    /// down.
    ///
    /// The recursion terminates at the root, which is addressed as `/me/drive/root` and
    /// has no spelling to get wrong.
    async fn list_unspellable(
        &self,
        folder: &str,
        as_given: anyhow::Error,
    ) -> io::Result<Vec<Value>> {
        let composed: String = folder.nfc().collect();
        let decomposed: String = folder.nfd().collect();
        // A path that normalizes to itself both ways has one spelling, and the service has
        // just said that one is not there. Neither step below can change the answer, and a
        // macOS mount generates a great many such lookups — every `.DS_Store` and `._*`
        // probe — so they must keep costing the single request they already spent.
        if composed == folder && decomposed == folder {
            return Err(not_found_or_backend(as_given));
        }
        if composed != folder {
            match self
                .accessor
                .list_children(&composed, MAX_FOLDER_ITEMS)
                .await
            {
                Ok(rows) => return Ok(rows),
                Err(e) if !is_not_found(&e) => return Err(not_found_or_backend(e)),
                Err(_) => {}
            }
        }
        let entry = Box::pin(self.resolve(folder)).await?;
        if !entry.is_dir {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        self.accessor
            .list_children_of_id(&entry.id, MAX_FOLDER_ITEMS)
            .await
            .map_err(not_found_or_backend)
    }

    /// Resolve a path to its entry, by looking it up in its parent's listing.
    ///
    /// Through the parent rather than by asking Graph for the item directly, because a
    /// `stat` per entry is what a listing costs the kernel: FUSE-T serves over NFS and an
    /// NFS client fills an attribute for every name it lists. One listing answers all of
    /// them; one item request each would answer them one round trip at a time.
    async fn resolve(&self, path: &str) -> io::Result<Child> {
        let (parent, name) = split_last(path);
        let children = self.list_dir(&parent).await?;
        children
            .iter()
            .find(|c| same_name(&c.name, &name))
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    /// The span covering `want` bytes from `start`, from the map when one reaches and from
    /// the service otherwise. The span's own start comes back beside its bytes, because
    /// the caller cuts by an offset into the file rather than into the buffer — and
    /// because the response, not the request, is what says where the bytes begin.
    ///
    /// Spans begin where a reader asks rather than on fixed boundaries. A sequential walk
    /// then lands inside the held span until it is spent and starts the next one exactly
    /// where it left off, so no window is ever split across two spans and a short return
    /// means end of file rather than a cache boundary.
    async fn span(
        &self,
        child: &Child,
        path: &str,
        start: u64,
        want: u64,
    ) -> io::Result<(u64, Arc<Vec<u8>>)> {
        let (walking, sharers) = {
            let mut held = self.held.lock().await;
            // Every *other* file read within [`ACTIVE`], plus this one, which is about to
            // hold a span whether or not it has an entry. Counting `path`'s own entry would
            // miss this reader once its span went stale, and a lone active neighbour would
            // then take the whole budget and evict the span the division exists to keep.
            let sharers = held
                .iter()
                .filter(|(k, h)| k.as_str() != path && h.used.elapsed() < ACTIVE)
                .count() as u64
                + 1;
            let walking = match held.get_mut(path) {
                Some(h) if h.when.elapsed() < DIR_TTL => {
                    if h.at <= start
                        && (start.saturating_add(want) <= h.at.saturating_add(h.bytes.len() as u64)
                            || h.to_eof)
                    {
                        // Served, so the file keeps counting toward *everyone else's*
                        // share: a neighbour must not decide it is alone and take enough
                        // to evict this span. Hits are microseconds apart, so ACTIVE only
                        // lapses between them for a reader that pauses seconds mid-file.
                        h.used = Instant::now();
                        return Ok((h.at, h.bytes.clone()));
                    }
                    // Not covered, but beginning inside what was — at its end, or
                    // straddling it — so the reader spent the span and wants the next.
                    // Not just `== end`: a window whose size does not divide the span
                    // reaches past it from inside, and that is the same walk.
                    h.at <= start && start <= h.at.saturating_add(h.bytes.len() as u64)
                }
                _ => false,
            };
            (walking, sharers)
        };
        // Sized so everything being walked fits in the budget at once. A constant equal to
        // the budget is the one value that cannot work: two files each wanting all of it
        // means one is always evicted, whatever the eviction order.
        let share = (HELD_BUDGET / sharers).max(MIN_SPAN);
        let len = want.max(if walking {
            share
        } else {
            share.min(FIRST_SPAN)
        });
        let range = start..start.saturating_add(len);

        let url = match child.download_url.as_deref() {
            Some(u) => u.to_string(),
            None => self.fresh_download_url(child).await?,
        };
        let fetched = self.accessor.download(&url, Some(range.clone())).await;
        // One refetch, and only one. The URL the listing carried is short-lived by
        // design, so a failure here is more likely an expiry than a fault — but a fresh
        // URL that fails again is a fault, and retrying it forever would hide it.
        let (at, bytes) = match fetched {
            Ok(v) => v,
            Err(_) => {
                let url = self.fresh_download_url(child).await?;
                self.accessor
                    .download(&url, Some(range))
                    .await
                    .map_err(not_found_or_backend)?
            }
        };

        // Short of what was asked for means the file ended inside it. Recorded, because it
        // is what lets a later window past the end be answered from the map instead of
        // fetching nothing again.
        let to_eof = (bytes.len() as u64) < len;
        let bytes = Arc::new(bytes);
        let now = Instant::now();
        self.hold(
            path,
            HeldSpan {
                at,
                to_eof,
                when: now,
                used: now,
                bytes: bytes.clone(),
            },
        )
        .await;
        Ok((at, bytes))
    }

    /// Keep one span, dropping what has aged out and then the least recently read from
    /// until [`HELD_BUDGET`] has room.
    ///
    /// The new span goes in even when it alone is over budget: it has already been fetched
    /// and the caller is about to read from it, so refusing to hold it would only cost the
    /// next window another fetch.
    async fn hold(&self, path: &str, span: HeldSpan) {
        let len = span.bytes.len() as u64;
        let mut held = self.held.lock().await;
        held.remove(path);
        // Dropped on the way in: nothing else removes an entry, so a span read once would
        // otherwise stay for the life of the mount.
        held.retain(|_, h| h.when.elapsed() < DIR_TTL);
        while !held.is_empty()
            && held.values().map(|h| h.bytes.len() as u64).sum::<u64>() + len > HELD_BUDGET
        {
            let coldest = held
                .iter()
                .min_by_key(|(_, h)| h.used)
                .map(|(k, _)| k.clone());
            match coldest {
                Some(k) => {
                    held.remove(&k);
                }
                None => break,
            }
        }
        held.insert(path.to_string(), span);
    }

    /// A download URL straight from the service, for when the listing's has expired.
    ///
    /// Costs one request and does not disturb the listing: the rest of that snapshot is
    /// still good, and re-listing the folder to refresh one URL would throw away every
    /// other entry's.
    ///
    /// By the item's id and not by its path, because this runs in the middle of a read.
    /// See [`get_item_by_id`](OnedriveAccessor::get_item_by_id).
    async fn fresh_download_url(&self, child: &Child) -> io::Result<String> {
        let item = self
            .accessor
            .get_item_by_id(&child.id)
            .await
            .map_err(not_found_or_backend)?;
        download_url_of(&item).ok_or_else(|| {
            io::Error::other(format!("onedrive: {} has no download url", child.name))
        })
    }

    /// One file's bytes, or one window of them.
    ///
    /// Kept apart from [`FileSystem::read_at`] because the two count in different units: a
    /// window is what the service charges for, and a buffer is what the kernel hands over.
    async fn read_window(
        &self,
        path: &Path,
        range: Option<std::ops::Range<u64>>,
    ) -> io::Result<Vec<u8>> {
        let path = vpath(path)?;
        let path = path.as_str();
        if path == "/" {
            return Err(io::Error::other("is a directory: /"));
        }
        let child = self.resolve(path).await?;
        if child.is_dir {
            return Err(io::Error::other(format!("is a directory: {path}")));
        }
        let Some(r) = range else {
            let url = match child.download_url.as_deref() {
                Some(u) => u.to_string(),
                None => self.fresh_download_url(&child).await?,
            };
            let whole = match self.accessor.download(&url, None).await {
                Ok((_, b)) => b,
                Err(_) => {
                    let url = self.fresh_download_url(&child).await?;
                    self.accessor
                        .download(&url, None)
                        .await
                        .map_err(not_found_or_backend)?
                        .1
                }
            };
            return Ok(whole);
        };
        let want = r.end.saturating_sub(r.start);
        if want == 0 {
            return Ok(Vec::new());
        }
        let (at, bytes) = self.span(&child, path, r.start, want).await?;
        // `at <= r.start` on both of `span`'s return paths: the hit branch carries it as
        // a conjunct, and a fetch returns where the response said the bytes begin, which
        // is `r.start` for a `206` and `0` for a whole file. So neither subtraction here
        // goes backwards.
        let from = r.start.saturating_sub(at);
        Ok(slice(&bytes, Some(from..r.end.saturating_sub(at))))
    }

    /// How many listings are retained. Tests only: growth here is invisible from outside.
    #[cfg(test)]
    pub(crate) async fn listings_retained(&self) -> usize {
        self.dir_cache.lock().await.len()
    }

    /// How many bytes are held for one path, and `None` when nothing is. Tests only.
    #[cfg(test)]
    pub(crate) async fn held_bytes(&self, path: &str) -> Option<u64> {
        self.held
            .lock()
            .await
            .get(path)
            .map(|h| h.bytes.len() as u64)
    }

    /// Push every held span `by` into the past, both when it was fetched and when it was
    /// last read from, so a test can make one look abandoned without waiting. Tests only.
    #[cfg(test)]
    pub(crate) async fn age_spans_for_test(&self, by: Duration) {
        let mut held = self.held.lock().await;
        for h in held.values_mut() {
            if let (Some(w), Some(u)) = (h.when.checked_sub(by), h.used.checked_sub(by)) {
                h.when = w;
                h.used = u;
            }
        }
    }

    /// Age every retained listing past its TTL. Tests only.
    #[cfg(test)]
    pub(crate) async fn age_listings_for_test(&self) {
        let mut cache = self.dir_cache.lock().await;
        let stale = Instant::now() - DIR_TTL - Duration::from_secs(1);
        for (at, _) in cache.values_mut() {
            *at = stale;
        }
    }
}

impl FileSystem for OnedriveFs {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let path = vpath(path)?;
            if path == "/" {
                return Ok(Stat::new(DirentKind::Dir, 0));
            }
            let child = self.resolve(&path).await?;
            Ok(stat_of(&child))
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let path = vpath(path)?;
            let children = self.list_dir(&path).await?;
            // `with_stat` and not `new`: the listing already carries everything an
            // attribute needs, so a reader that takes it here spends no `stat` at all.
            Ok(children
                .iter()
                .map(|c| Dirent::with_stat(c.name.clone(), stat_of(c)))
                .collect())
        })
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            if buf.is_empty() {
                return Ok(0);
            }
            let want = buf.len() as u64;
            let bytes = self
                .read_window(path, Some(offset..offset.saturating_add(want)))
                .await?;
            // Short is EOF and nothing else, which holds because a span never splits a
            // window: `span` fetches from where the read begins and at least as far as it
            // asks, so what comes back either covers the window or ran out of file.
            //
            // Nothing is padded: a driveItem states its length, so a read that comes up
            // short has genuinely reached the end.
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        })
    }
}

/// The attributes a listing already gave, without asking again.
fn stat_of(c: &Child) -> Stat {
    Stat {
        kind: if c.is_dir {
            DirentKind::Dir
        } else {
            DirentKind::File
        },
        size: c.size,
        mtime: c.mtime,
        created: c.created,
        etag: c.etag.clone(),
        ..Stat::new(
            if c.is_dir {
                DirentKind::Dir
            } else {
                DirentKind::File
            },
            c.size,
        )
    }
}

/// One listing row into an entry, or `None` for a row this tree does not show.
///
/// A **package** — a OneNote notebook is the one you meet — is dropped. Microsoft
/// describes it as "a package instead of a folder or file", treated as a folder by some
/// clients and a file by others; it has no bytes to read and no children this API will
/// list. A name that cannot be read is worse than an absence, so it is not shown.
///
/// An item that is neither `file` nor `folder` is dropped for the same reason.
///
/// So is a row with no `id`, which is the same judgement one more time: an item that
/// cannot be addressed after the listing is one whose read would fail the moment its
/// download URL expired. Graph states an id on every driveItem and this code `$select`s
/// it, so the case is a malformed row rather than a shape of the API.
fn child_from_item(v: &Value) -> Option<Child> {
    if v.get("package").is_some() {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    let name = sanitize_name(v.get("name")?.as_str()?);
    let is_dir = v.get("folder").is_some();
    if !is_dir && v.get("file").is_none() {
        return None;
    }
    Some(Child {
        id,
        name,
        is_dir,
        // A folder states a size too — the sum of what it contains — which is not a length
        // anything reads, so it is reported as zero the way a directory always is.
        size: if is_dir {
            0
        } else {
            v.get("size").and_then(|s| s.as_u64()).unwrap_or(0)
        },
        mtime: time_field(v, "lastModifiedDateTime"),
        created: time_field(v, "createdDateTime"),
        etag: v
            .get("cTag")
            .or_else(|| v.get("eTag"))
            .and_then(|t| t.as_str())
            .map(str::to_string),
        download_url: download_url_of(v),
    })
}

fn download_url_of(v: &Value) -> Option<String> {
    v.get(DOWNLOAD_URL_KEY)
        .and_then(|u| u.as_str())
        .map(str::to_string)
}

fn time_field(v: &Value, key: &str) -> Option<SystemTime> {
    let s = v.get(key)?.as_str()?;
    let secs = chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp();
    (secs >= 0).then(|| std::time::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// An `anyhow` error from the accessor into an `io` one, keeping "not found" apart from
/// everything else — a reader distinguishes them and a mount has to as well.
///
/// The distinction is what a caller acts on, and it only goes one way safely. `NotFound`
/// on a path is a name that is gone, which a traversal skips; anything else is the backend
/// failing, which it must not read as an absence. See [`is_not_found`].
fn not_found_or_backend(e: anyhow::Error) -> io::Error {
    if is_not_found(&e) {
        io::Error::from(io::ErrorKind::NotFound)
    } else {
        io::Error::other(format!("{e:#}"))
    }
}

/// Whether the service answered this request `404`.
///
/// Asked of the status the error carries rather than found in its message. A Graph error
/// body states a correlation id, which is hex and so sometimes contains `404` while meaning
/// nothing by it: a substring search would turn a backend failure into an absence, and the
/// mount would answer `ENOENT` for a file that is there.
///
/// Only a Graph error can be one: a failed download says its status without reqwest's
/// error (see [`download`](OnedriveAccessor::download)), and a failed token exchange is a
/// statement about a credential that nothing should be able to turn into a missing file.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        == Some(reqwest::StatusCode::NOT_FOUND)
}

/// Sanitize an item name into a single path segment.
///
/// OneDrive already refuses most of what would need removing — `\ / : * ? " < > |` are not
/// allowed in a name — so this is a guard against a gateway that is less strict rather
/// than a routine transformation. `/` is the one that would silently become a path
/// separator and address something else entirely.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    match cleaned.as_str() {
        "" | "." | ".." => "untitled".to_string(),
        _ => cleaned,
    }
}

/// Whether two names are the same name, in the sense a directory has to mean it.
///
/// One name has two spellings in Unicode — `한` is a single code point composed, or three
/// jamo decomposed, and Japanese voiced marks work the same way. macOS hands a lookup the
/// *decomposed* form of whatever a listing returned, and a service stores whichever form
/// the client that uploaded a file happened to send. So a byte comparison answers `ENOENT`
/// for a name `ls` printed a moment earlier.
///
/// Bytes first, because that is the answer for every ASCII name and most others; the
/// composition runs only when a byte comparison has already failed. Nothing else guards
/// it: a pair spanning the ASCII boundary can still compose to one name, since
/// `NFC("\u{212A}")` — the Kelvin sign — is `"K"`.
fn same_name(a: &str, b: &str) -> bool {
    a == b || a.nfc().eq(b.nfc())
}

/// A `&Path` in the form the resolver works in: `/` for the root, `/a/b` under it.
///
/// `..` is refused rather than walked. A name survives the sanitizer with almost anything
/// in it, so a parent reference resolved here would let a path address a directory nobody
/// named — and the tree has no `..` of its own for it to mean.
fn vpath(path: &Path) -> io::Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => parts.push(
                name.to_str()
                    .ok_or(io::Error::from(io::ErrorKind::InvalidFilename))?,
            ),
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(io::ErrorKind::InvalidFilename.into());
            }
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Split a path into `(parent_dir, last_segment)`. `/a/b` -> (`/a`, `b`); `/a` -> (`/`, `a`).
fn split_last(path: &str) -> (String, String) {
    let p = path.trim_end_matches('/');
    match p.rsplit_once('/') {
        Some((parent, name)) => {
            let parent = if parent.is_empty() {
                "/".to_string()
            } else {
                parent.to_string()
            };
            (parent, name.to_string())
        }
        None => ("/".to_string(), p.to_string()),
    }
}

/// A window of `data`, clamped so a range past the end is empty rather than fatal.
fn slice(data: &[u8], range: Option<std::ops::Range<u64>>) -> Vec<u8> {
    match range {
        Some(r) => {
            debug_assert!(r.end >= r.start, "callers hand this a forward range");
            let start = (r.start as usize).min(data.len());
            // No `.max(start)`: with `r.end >= r.start` above, clamping both to the same
            // length keeps them in order.
            let end = (r.end as usize).min(data.len());
            data[start..end].to_vec()
        }
        None => data.to_vec(),
    }
}

#[cfg(test)]
#[path = "onedrive_tests.rs"]
mod tests;
