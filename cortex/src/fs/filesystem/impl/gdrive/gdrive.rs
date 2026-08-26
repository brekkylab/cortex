use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::Mutex;

use super::accessor::{GdriveAccessor, GdriveConfig};
use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, Stat},
};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// The mount root mirrors Drive's own sidebar as virtual sections. `My Drive` is
/// the literal Drive folder id `root`; `Shared with me` is a sentinel resolved
/// to a `sharedWithMe=true` listing (shared items carry no `parents`, so the
/// folder tree alone would never surface them). The sentinel contains `@`, which
/// the Drive id alphabet (`[A-Za-z0-9_-]`) never does, so it can't collide with
/// a real file id. Names are Drive's own English section labels.
const MY_DRIVE_ID: &str = "root";
const MY_DRIVE_NAME: &str = "My Drive";
const SHARED_WITH_ME_ID: &str = "@sharedWithMe";
const SHARED_WITH_ME_NAME: &str = "Shared with me";

/// Whether a listing entry is a document's JSON rather than a file's own bytes.
///
/// By the suffix rather than by a mime: [`Stat`] carries no content type, and it would
/// be a second copy of what the name already says. [`NATIVE_KINDS`] is the one place
/// the pairing lives, so a type added there is answered here without being added twice.
#[cfg(test)]
fn is_native_json(e: &Dirent) -> bool {
    NATIVE_KINDS
        .iter()
        .any(|(_, _, suffix)| e.name.ends_with(suffix))
}

/// How each Docs-editors type is served: the API that answers for it, and the
/// suffix its entry carries.
///
/// These types hold no bytes of their own — Drive can only export a rendering of
/// them — so the document's own API is the only form that carries everything:
/// formulas, slide geometry, and the character indices an edit has to address.
/// The suffix says which document it is, since the Drive name has no extension.
const NATIVE_KINDS: &[(&str, NativeApi, &str)] = &[
    (
        "application/vnd.google-apps.document",
        NativeApi::Doc,
        ".gdoc.json",
    ),
    (
        "application/vnd.google-apps.spreadsheet",
        NativeApi::Sheet,
        ".gsheet.json",
    ),
    (
        "application/vnd.google-apps.presentation",
        NativeApi::Slides,
        ".gslide.json",
    ),
];

/// The API and suffix for a native mime, if it is one.
fn native_kind(mime: &str) -> Option<(NativeApi, &'static str)> {
    NATIVE_KINDS
        .iter()
        .find(|(m, _, _)| *m == mime)
        .map(|(_, api, suffix)| (*api, *suffix))
}

/// Per-directory listing TTL, matching the metadata cache's own listing TTL.
///
/// This cache is not the freshness policy — the wrapper above it is, and it does the
/// expiring, invalidating and negative caching. This one exists because a path has to
/// become a Drive id before anything can be read, and that means walking parent
/// listings. Holding a *different* number here bought nothing and cost two things: a
/// read whose parent listing had expired here but not there re-fetched it (an extra
/// `files.list` per file, on any traversal outrunning the shorter TTL), and for the
/// span between the two numbers `ls` answered from one snapshot while reads resolved
/// against another.
const DIR_TTL: Duration = Duration::from_secs(300);
/// Ceiling on the cell values one spreadsheet's JSON will carry, spent tab by tab
/// in the workbook's own order until it runs out.
///
/// Values are proportional to what is actually filled in — 443 B to 105 KB per tab
/// measured, an 8-tab workbook around 185 KB — so this bounds the outlier rather
/// than the common case. A workbook of 634,410 cells exists in the corpus.
const GRID_BYTES_BUDGET: u64 = 8 * 1024 * 1024;
/// Tabs whose values are requested in one `batchGet`. Each title rides in the
/// query string, so an unbounded count would eventually build an unsendable URL.
const MAX_TABS: usize = 64;

/// Size reported for a document whose length nobody has learned yet.
///
/// It cannot be 0. Reads run under the guest's FUSE mount with `direct_io`, and a
/// 0-length file was measured (in ailoy's Drive mount, which hit this first) to
/// clamp reads to nothing — and a search tool skips a file it is told is empty. An
/// over-estimate is safe in the other direction: the read returns the real bytes
/// and then empty at EOF, so `cat` stops at the true end.
///
/// 8 MiB, matching the content cache's per-object limit: anything at or under it
/// reports its exact length from the moment it is first read, so the placeholder is
/// what an unread document shows, not what a document shows. Measured real lengths
/// were 52 KB to 3.4 MB.
///
/// This is a placeholder, not a measurement — `find -size` and `ls -l` see it until
/// something reads the file. Making it exact up front costs one render per
/// document: measured 2.4s for a six-document folder listing, 6.3s when the
/// kernel's per-entry `getattr` serialises them. See
/// [`GdriveFs::resolve_size_on_stat`].
const UNKNOWN_LENGTH_SIZE: u64 = 8 * 1024 * 1024;

/// Cap on remembered probe results — one per file ever sized in this mount.
const MAX_PROBED_LENGTHS: usize = 50_000;

/// Safety ceiling on one folder's listing (10 pages). Beyond this the listing
/// truncates (the accessor logs it) — a >10k-child folder is pathological to
/// `ls` anyway.
const MAX_FOLDER_FILES: usize = 10_000;


/// Whether Drive holds real bytes for this row. The Docs-editors types (and
/// Forms, Maps, Drawings) do not — `alt=media` answers 403 for them.
fn has_original_bytes(mime: &str) -> bool {
    !mime.starts_with("application/vnd.google-apps.")
}

/// What an entry hands back when read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Serves {
    /// A directory — nothing to read.
    Nothing,
    /// The file's own bytes (`alt=media`), ranged.
    Original,
    /// The document's own structure, from its own API (`documents.get` and
    /// friends) rather than Drive.
    Native(NativeApi),
}

/// Which API answers for a document's structure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NativeApi {
    Doc,
    Sheet,
    Slides,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GKind {
    Folder,
    SharedDrive,
    File,
}

impl GKind {
    fn is_dir(self) -> bool {
        matches!(self, GKind::Folder | GKind::SharedDrive)
    }
}

/// One resolved Drive entry, as the VFS sees it.
#[derive(Clone)]
struct Child {
    /// Listing name: the sanitized (and, on collision, disambiguated) Drive
    /// name — plus a `.json` suffix when the entry serves a document's API JSON.
    vfs_name: String,
    id: String,
    /// Set when the entry lives in a shared drive (listing scope).
    drive_id: Option<String>,
    kind: GKind,
    mtime: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    /// What this entry hands back when read.
    serves: Serves,
    /// Byte length when it is known without fetching: Drive reports it for a file
    /// it holds bytes for. `None` = only producing the bytes can tell, which
    /// `entry_size` reports as [`UNKNOWN_LENGTH_SIZE`].
    size: Option<u64>,
}

/// One folder's children as the cache holds them: shared, so a `stat` or a read
/// borrows the listing instead of copying it (a shared folder in the corpus lists
/// 10,000 entries).
type CachedListing = (Instant, Arc<Vec<Child>>);

/// One document's JSON as the cache holds it: shared, so a read borrows the bytes
/// rather than copying a megabyte per chunk.
type CachedRender = (Instant, Arc<Vec<u8>>);

pub struct GdriveFs {
    accessor: GdriveAccessor,
    /// Lengths learned by probing (file id → bytes), for the rows Drive listed
    /// without a `size`. One probe per file per session: `stat` is asked once per
    /// entry after every listing, and a probe is a request.
    probed: Mutex<HashMap<String, u64>>,
    /// Per-directory listing cache (folder path → children). Path resolution walks
    /// parent listings, so one cached listing answers readdir, stat and the lookup
    /// a read starts with; fetching the bytes themselves still costs a request.
    dir_cache: Mutex<HashMap<String, CachedListing>>,
    /// A document's JSON, once produced (file id → bytes).
    ///
    /// A document has no windows: its API answers with the whole thing or nothing, so a
    /// read of one produces all of it however few bytes were asked for. A kernel asks in
    /// chunks, so without this a 1 MB document read at 128 KiB a time is eight renders of
    /// the same document — and a render is one or two seconds. Held as an `Arc` because a
    /// read borrows it rather than copying a megabyte per chunk.
    rendered: Mutex<HashMap<String, CachedRender>>,
}

impl GdriveFs {
    pub fn new(config: &GdriveConfig) -> anyhow::Result<Self> {
        Ok(Self {
            accessor: GdriveAccessor::new(config)?,
            probed: Mutex::new(HashMap::new()),
            dir_cache: Mutex::new(HashMap::new()),
            rendered: Mutex::new(HashMap::new()),
        })
    }

    /// A document's JSON, from the cache when one is fresh and from its own API
    /// otherwise.
    ///
    /// On [`DIR_TTL`] rather than a number of its own, for the reason that constant
    /// already gives: two TTLs mean a span in which `ls` answers from one snapshot while
    /// a read of the same document answers from another, and the listing is what named
    /// the document in the first place.
    ///
    /// A document over the accessor's whole-read ceiling never arrives, so nothing
    /// unbounded is stored; what bounds the *map* is the same sweep `dir_cache` uses,
    /// which drops what has aged out on the way in.
    async fn rendered_json(&self, id: &str, api: NativeApi) -> io::Result<Arc<Vec<u8>>> {
        if let Some((at, bytes)) = self.rendered.lock().await.get(id)
            && at.elapsed() < DIR_TTL
        {
            return Ok(bytes.clone());
        }
        let bytes = match api {
            NativeApi::Sheet => self.spreadsheet_bytes(id).await,
            NativeApi::Doc => self.accessor.document_json(id).await,
            NativeApi::Slides => self.accessor.presentation_json(id).await,
        }
        .map_err(not_found_or_backend)?;
        let bytes = Arc::new(bytes);
        let mut cache = self.rendered.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < DIR_TTL);
        cache.insert(id.to_string(), (Instant::now(), bytes.clone()));
        Ok(bytes)
    }

    /// The mount root's virtual sections: `My Drive`, `Shared with me`, and
    /// (best-effort — needs scope; accounts without any list none) each shared
    /// drive as its own top-level directory.
    async fn root_sections(&self) -> (Vec<Child>, bool) {
        let section = |name: &str, id: &str, kind: GKind, drive_id: Option<String>| Child {
            vfs_name: name.to_string(),
            id: id.to_string(),
            drive_id,
            kind,
            // A section is a directory, and a directory's type is a directory.
            mtime: None,
            created: None,
            serves: Serves::Nothing,
            size: None,
        };
        let mut children = vec![
            section(MY_DRIVE_NAME, MY_DRIVE_ID, GKind::Folder, None),
            section(SHARED_WITH_ME_NAME, SHARED_WITH_ME_ID, GKind::Folder, None),
        ];
        let listed = self.accessor.list_shared_drives().await;
        // A failure here is indistinguishable from an account with no shared drives,
        // so say so and let the caller keep the reduced root out of the cache: a
        // listing cached for the TTL would hide them for minutes with nothing to
        // explain their absence.
        if let Err(e) = &listed {
            eprintln!("gdrive: shared drives could not be listed, root omits them: {e:#}");
        }
        let complete = listed.is_ok();
        if let Ok(drives) = listed {
            let mut existing: HashSet<String> =
                children.iter().map(|c| c.vfs_name.clone()).collect();
            for d in &drives {
                if let (Some(id), Some(name)) = (
                    d.get("id").and_then(|x| x.as_str()),
                    d.get("name").and_then(|x| x.as_str()),
                ) {
                    let vfs_name = unique_name(&sanitize_name(name), &existing);
                    existing.insert(vfs_name.clone());
                    children.push(section(
                        &vfs_name,
                        id,
                        GKind::SharedDrive,
                        Some(id.to_string()),
                    ));
                }
            }
        }
        (children, complete)
    }

    /// List a directory's immediate children (cached). The root is virtual (see
    /// [`Self::root_sections`]); everything else is a Drive listing.
    async fn list_dir(&self, folder: &str) -> io::Result<Arc<Vec<Child>>> {
        {
            let cache = self.dir_cache.lock().await;
            if let Some((at, children)) = cache.get(folder)
                && at.elapsed() < DIR_TTL
            {
                return Ok(children.clone());
            }
        }
        let mut complete = true;
        let mut children = if folder == "/" {
            let (sections, ok) = self.root_sections().await;
            complete = ok;
            sections
        } else {
            let (folder_id, drive_id) = self.folder_id_of(folder).await?;
            let files = if folder_id == SHARED_WITH_ME_ID {
                self.accessor.list_shared_with_me(MAX_FOLDER_FILES).await
            } else {
                self.accessor
                    .list_files(&folder_id, drive_id.as_deref(), MAX_FOLDER_FILES)
                    .await
            }
            .map_err(not_found_or_backend)?;
            let mut children: Vec<Child> = files.iter().filter_map(child_from_file).collect();
            // Children of a shared drive stay scoped to it (list_files needs the
            // drive id); the drive's own listing rows don't carry `driveId`.
            if let Some(d) = &drive_id {
                for c in children.iter_mut() {
                    c.drive_id.get_or_insert_with(|| d.clone());
                }
            }
            children
        };

        // Two Drive files can share a name; disambiguate so every entry is
        // reachable (readdir shows distinct names, resolve finds each one).
        disambiguate(&mut children);

        let children = Arc::new(children);
        if complete {
            let mut cache = self.dir_cache.lock().await;
            // Drop what has aged out before adding to it. Nothing else ever removed an
            // entry, so a listing stayed for the life of the mount long after its TTL
            // made it unusable — one entry per folder ever visited, and the corpus has
            // a folder that lists 10,000 of them.
            cache.retain(|_, (at, _)| at.elapsed() < DIR_TTL);
            cache.insert(folder.to_string(), (Instant::now(), children.clone()));
        }
        Ok(children)
    }

    /// Resolve a folder path to its Drive id (+ shared-drive id) by walking
    /// parent listings from the root sections (`/My Drive` = the literal Drive
    /// id `root`; `/` itself is virtual and handled by [`Self::list_dir`]).
    async fn folder_id_of(&self, folder: &str) -> io::Result<(String, Option<String>)> {
        let (parent, name) = split_last(folder);
        let children = Box::pin(self.list_dir(&parent)).await?;
        let entry = children
            .iter()
            .find(|c| c.vfs_name == name && c.kind.is_dir())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok((entry.id.clone(), entry.drive_id.clone()))
    }

    /// A spreadsheet's JSON: its structure, with each tab's cell values folded in
    /// under `values`.
    ///
    /// Two calls, because Sheets has no single one that answers both cheaply.
    /// `spreadsheets.get` gives the workbook's shape (3.7-19.5 KB measured) and,
    /// with it, the tab titles that name the ranges; `values:batchGet` then returns
    /// the used range of every tab at once. The flag that looks like the direct
    /// route — `includeGridData=true` — bills per *allocated* cell, and the first
    /// real workbook tried allocated 210,125 for an estimated 189 MB, so it is not
    /// a route at all. See [`GdriveAccessor::sheet_values_batch`].
    ///
    /// A tab whose values exceed the budget is left out with a `valuesOmitted` note
    /// on it, so a reader sees a stated omission rather than an empty sheet.
    async fn spreadsheet_bytes(&self, id: &str) -> anyhow::Result<Vec<u8>> {
        let mut v: Value = serde_json::from_slice(&self.accessor.spreadsheet_json(id).await?)?;
        let titles: Vec<String> = tab_titles(&v);
        if titles.is_empty() {
            return pretty(&v);
        }
        // Whole tabs by name: an A1 range with no cell part means "everything used".
        // A values endpoint that is missing (a Drive-only mock) or forbidden (the
        // Sheets API not enabled on the project) costs the cells, not the read —
        // the workbook's shape is still worth serving, with the reason attached.
        let batch = match self.accessor.sheet_values_batch(id, &titles).await {
            Ok(b) => b,
            Err(e) => {
                // Not logged as well. The reason goes into the workbook itself, which is
                // the copy the reader actually meets — a line on stderr would say the
                // same thing to someone who is not looking at it.
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("valuesUnavailable".into(), Value::String(format!("{e:#}")));
                }
                return pretty(&v);
            }
        };
        fold_values(&mut v, &batch, &titles);
        pretty(&v)
    }

    /// The probed length of `id`, remembered so the kernel's per-entry `getattr`
    /// storm costs one request, not one per stat. `None` when the probe fails —
    /// the placeholder stands rather than a wrong number.
    async fn probed_len(&self, id: &str) -> Option<u64> {
        if let Some(n) = self.probed.lock().await.get(id) {
            return Some(*n);
        }
        match self.accessor.probe_len(id).await {
            Ok(n) => {
                let mut probed = self.probed.lock().await;
                // A learned length is an optimisation, so losing one costs a request
                // rather than correctness — which is what lets this be a flat cap
                // instead of bookkeeping.
                if probed.len() >= MAX_PROBED_LENGTHS {
                    probed.clear();
                }
                probed.insert(id.to_string(), n);
                Some(n)
            }
            // `None`, and nothing said. A probe runs per file, so a folder whose whole
            // listing fails would put one line on stderr per entry — worse than the
            // silence, at ten thousand children. What the caller does with `None` is
            // report the placeholder, which is the same thing it reports for a file
            // nobody has probed yet: the size says "not measured" either way.
            Err(_) => None,
        }
    }

    /// How many listings are retained. Tests only: growth here is invisible from
    /// outside, which is how it went unbounded.
    #[cfg(test)]
    pub(crate) async fn listings_retained(&self) -> usize {
        self.dir_cache.lock().await.len()
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

    /// Resolve any path (file or folder) to its child entry via its parent dir.
    async fn resolve(&self, path: &str) -> io::Result<Child> {
        let (parent, name) = split_last(path);
        let children = self.list_dir(&parent).await?;
        children
            .iter()
            .find(|c| c.vfs_name == name)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

impl GdriveFs {
    /// One file's bytes, or one window of them.
    ///
    /// Kept apart from [`FileSystem::read_at`] because the two count in different
    /// units: a window is what Drive charges for, and a buffer is what the kernel
    /// hands over. The branching that decides *which* API answers belongs with the
    /// window; filling a buffer from the answer is the other side's whole job.
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
        if child.kind.is_dir() {
            return Err(io::Error::other(format!("is a directory: {path}")));
        }
        match child.serves {
            // Ranged: a reader walking a big file, or a search tool sampling its
            // head, must not pull the whole object per chunk.
            Serves::Original => {
                let bytes = self
                    .accessor
                    .download(&child.id, range.clone())
                    .await
                    .map_err(not_found_or_backend)?;
                // Drive honored the range, so the window is already the answer;
                // fall back to slicing if it sent the whole object anyway. Saturating,
                // because `end - start` on a backwards range underflows and takes the
                // process with it.
                Ok(match &range {
                    Some(r) if bytes.len() as u64 > r.end.saturating_sub(r.start) => {
                        slice(&bytes, range)
                    }
                    _ => bytes,
                })
            }
            // A document from its own API, produced once and then held: its API has no
            // notion of a range, so the window is ours to cut out of the whole thing.
            Serves::Native(api) => {
                let bytes = self.rendered_json(&child.id, api).await?;
                Ok(slice(&bytes, range))
            }
            // Only a directory serves nothing, and directories were rejected
            // above — so this is unreachable for a resolved file.
            Serves::Nothing => Err(io::ErrorKind::NotFound.into()),
        }
    }
}

impl FileSystem for GdriveFs {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let path = vpath(path)?;
            if path == "/" {
                return Ok(Stat::new(DirentKind::Dir, 0));
            }
            let child = self.resolve(&path).await?;
            // A file Drive listed without a size: learn the real one from one ranged
            // response header rather than leaving the placeholder, which would make
            // `ls -l` lie and a reader truncate the body at 8 MiB.
            let size = match (&child.serves, child.size) {
                (Serves::Original, None) => self.probed_len(&child.id).await,
                _ => None,
            };
            Ok(Stat {
                kind: kind_of(&child),
                size: size.unwrap_or_else(|| entry_size(&child)),
                mtime: child.mtime,
                created: child.created,
                ..Stat::new(kind_of(&child), 0)
            })
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let path = vpath(path)?;
            let children = self.list_dir(&path).await?;
            Ok(children.iter().map(dirent_for).collect())
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
            // Short is EOF and nothing else, which holds here because the window came
            // back whole: Drive served exactly the range, or `slice` cut it from bytes
            // this already had. Neither can answer part of a window it holds.
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        })
    }
}

fn entry_size(c: &Child) -> u64 {
    match (c.kind.is_dir(), c.size) {
        (true, _) => 0,
        (_, Some(n)) => n,
        // A document nobody has read yet: see UNKNOWN_LENGTH_SIZE.
        (_, None) => UNKNOWN_LENGTH_SIZE,
    }
}

/// Whether this child is a directory, in the trait's own vocabulary.
fn kind_of(c: &Child) -> DirentKind {
    if c.kind.is_dir() {
        DirentKind::Dir
    } else {
        DirentKind::File
    }
}

/// The listing row for one child.
///
/// With a [`Stat`] attached, because a Drive listing answers with names, types and
/// timestamps in one response: filling it costs nothing here and saves the caller a
/// `stat` per entry.
///
/// [`is_estimate`] has nowhere to be said. A `Stat` carries a length and not whether
/// the length was measured, so a document's placeholder reads as a size like any
/// other until something opens the file and [`FileSystem::stat`] probes it.
fn dirent_for(c: &Child) -> Dirent {
    Dirent::with_stat(
        c.vfs_name.clone(),
        Stat {
            size: entry_size(c),
            mtime: c.mtime,
            created: c.created,
            ..Stat::new(kind_of(c), 0)
        },
    )
}

/// Map an accessor error into the one the trait speaks: an upstream HTTP 404 (a file
/// id that no longer exists) becomes [`NotFound`](io::ErrorKind::NotFound), and
/// everything else stays whatever it was, described.
///
/// The distinction is what a caller acts on. `NotFound` on a path is a name that is
/// gone, which a traversal skips; anything else is the backend failing, which it must
/// not read as an absence.
fn not_found_or_backend(e: anyhow::Error) -> io::Error {
    let is_404 = e
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        == Some(reqwest::StatusCode::NOT_FOUND);
    if is_404 {
        io::Error::from(io::ErrorKind::NotFound)
    } else {
        io::Error::other(format!("{e:#}"))
    }
}

/// Map one `files.list` row into the entry it becomes, or `None` when the mount
/// has nothing to serve for it (Forms, Maps and Drawings answer no API and export
/// to nothing — a name that cannot be read is worse than an absence).
fn child_from_file(f: &Value) -> Option<Child> {
    let name = sanitize_name(f.get("name")?.as_str()?);
    let id = f.get("id")?.as_str()?.to_string();
    let mime = f.get("mimeType").and_then(|m| m.as_str()).unwrap_or("");
    let drive_id = f.get("driveId").and_then(|d| d.as_str()).map(String::from);
    let (mtime, created) = (time_field(f, "modifiedTime"), time_field(f, "createdTime"));

    if mime == FOLDER_MIME {
        return Some(Child {
            vfs_name: name,
            id,
            drive_id,
            kind: GKind::Folder,
            mtime,
            created,
            serves: Serves::Nothing,
            size: None,
        });
    }
    let (vfs_name, serves, size) = if has_original_bytes(mime) {
        // Drive reports the length up front, so this entry is honest about its
        // size and a reader can seek inside it.
        let size = f
            .get("size")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<u64>().ok());
        (name, Serves::Original, size)
    } else {
        // A native document: served as its own API's JSON.
        let (api, suffix) = native_kind(mime)?;
        (format!("{name}{suffix}"), Serves::Native(api), None)
    };
    Some(Child {
        vfs_name,
        id,
        drive_id,
        kind: GKind::File,
        mtime,
        created,
        serves,
        size,
    })
}

/// Attach each tab's values to the tab they belong to.
///
/// Paired by sheet title, not by position. `valueRanges` comes back in request order,
/// but the request is built from a *filtered and truncated* view of `sheets` — a sheet
/// with no title cannot be addressed and a workbook past [`MAX_TABS`] is cut off — so
/// walking the two in step hands one tab another's cells and shifts every tab after
/// it. `valueRanges[].range` names its own sheet, which removes the guesswork.
///
/// A tab that ends up with no values says why: it was never requested, nothing came
/// back for it, or its cells did not fit the budget. The budget is spent tab by tab
/// and an oversized one does not consume the remainder — a 20-byte tab after a large
/// one still fits.
fn fold_values(workbook: &mut Value, batch: &Value, requested: &[String]) {
    let mut by_title: HashMap<String, Value> = HashMap::new();
    for vr in batch
        .get("valueRanges")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
    {
        let Some(title) = vr.get("range").and_then(|r| r.as_str()).map(range_title) else {
            continue;
        };
        let values = vr.get("values").cloned().unwrap_or(Value::Array(vec![]));
        by_title.insert(title, values);
    }
    let asked: HashSet<&str> = requested.iter().map(String::as_str).collect();

    let mut budget = GRID_BYTES_BUDGET;
    for tab in workbook
        .get_mut("sheets")
        .and_then(|s| s.as_array_mut())
        .into_iter()
        .flatten()
    {
        let title = tab
            .pointer("/properties/title")
            .and_then(|t| t.as_str())
            .map(str::to_string);
        let Some(obj) = tab.as_object_mut() else {
            continue;
        };
        let note = |reason: &str| serde_json::json!({ "reason": reason });
        let Some(title) = title else {
            obj.insert(
                "valuesOmitted".into(),
                note("this sheet has no title, so its cells cannot be addressed"),
            );
            continue;
        };
        if !asked.contains(title.as_str()) {
            obj.insert(
                "valuesOmitted".into(),
                serde_json::json!({
                    "reason": "past the tab cap, so its values were never requested",
                    "tabCap": MAX_TABS,
                }),
            );
            continue;
        }
        let Some(values) = by_title.get(&title) else {
            obj.insert(
                "valuesOmitted".into(),
                note("the values request returned nothing for this sheet"),
            );
            continue;
        };
        let cost = served_len(values);
        if cost > budget {
            obj.insert(
                "valuesOmitted".into(),
                serde_json::json!({
                    "reason": "over the size budget",
                    "bytes": cost,
                    "budgetLeft": budget,
                }),
            );
            continue;
        }
        budget -= cost;
        obj.insert("values".into(), values.clone());
    }
}

/// The sheet a returned A1 range belongs to: `'메인화면'!A1:Z968` -> `메인화면`.
///
/// The title is everything before the last `!`, unquoted — a quoted title may itself
/// contain `!`, and a literal quote inside one arrives doubled.
fn range_title(range: &str) -> String {
    let sheet = match range.rsplit_once('!') {
        Some((sheet, _)) => sheet,
        None => range,
    };
    match sheet.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        Some(inner) => inner.replace("''", "'"),
        None => sheet.to_string(),
    }
}

/// Pretty-printed JSON with a trailing newline — the form every document is served
/// in, so the bytes read as lines rather than one long string.
fn pretty(v: &Value) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(v)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// A workbook's tab titles, in order, capped at [`MAX_TABS`].
fn tab_titles(workbook: &Value) -> Vec<String> {
    workbook
        .get("sheets")
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| t.pointer("/properties/title")?.as_str())
        .map(str::to_string)
        .take(MAX_TABS)
        .collect()
}

/// How many bytes `v` will add to the served file, counted without building them.
///
/// Pretty-printed, because that is the form the file is served in: a values array is
/// rows of columns of short strings, and indenting one puts every cell on its own
/// line. Measured on a 200x20 grid, that is 1.66x the compact form — so a budget
/// checked against compact bytes admits a file half again as large as it allows.
fn served_len(v: &Value) -> u64 {
    struct Counting(u64);
    impl std::io::Write for Counting {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counting(0);
    serde_json::to_writer_pretty(&mut c, v)
        .map(|()| c.0)
        .unwrap_or(0)
}

fn time_field(f: &Value, key: &str) -> Option<std::time::SystemTime> {
    rfc3339_to_systemtime(f.get(key)?.as_str()?)
}

/// Parse an RFC 3339 timestamp into a `SystemTime` (pre-epoch → `None`).
fn rfc3339_to_systemtime(s: &str) -> Option<std::time::SystemTime> {
    let secs = chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp();
    (secs >= 0).then(|| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// Sanitize a Drive name into a single path segment. Drive allows any character
/// in a name — including `/` — so path separators and control chars collapse to
/// `_`, and a name that is empty, `.`, or `..` after that falls back to a
/// placeholder (it would otherwise escape its directory).
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

/// Give every child a unique `vfs_name`: on a collision, number it ` (2)`, ` (3)`,
/// … so duplicate Drive names don't shadow each other in readdir/resolve.
///
/// The number goes *before* the extension. Appending it (`sheet.gsheet.json (2)`)
/// kept the name unique but took the entry out of every glob a reader would use to
/// find it — measured against a real account, two of 33 spreadsheets were invisible
/// to `**/*.gsheet.json`.
fn disambiguate(children: &mut [Child]) {
    let mut seen: HashSet<String> = HashSet::new();
    for c in children.iter_mut() {
        if seen.insert(c.vfs_name.clone()) {
            continue;
        }
        let (stem, ext) = split_extension(&c.vfs_name, c.serves);
        let mut n = 2;
        loop {
            let cand = format!("{stem} ({n}){ext}");
            if seen.insert(cand.clone()) {
                c.vfs_name = cand;
                break;
            }
            n += 1;
        }
    }
}

/// Split a listing name into the part a number can follow and the extension it must
/// stay in front of.
///
/// A document's suffix is known exactly (`.gsheet.json`, not `.json`). A file keeps
/// whatever follows its last dot when that looks like an extension. A directory has
/// no extension to protect, so `v1.2` numbers as `v1.2 (2)`.
fn split_extension(name: &str, serves: Serves) -> (&str, &str) {
    if serves == Serves::Nothing {
        return (name, "");
    }
    if let Serves::Native(api) = serves {
        let suffix = NATIVE_KINDS
            .iter()
            .find(|(_, a, _)| *a == api)
            .map(|(_, _, s)| *s)
            .unwrap_or("");
        if let Some(stem) = name.strip_suffix(suffix) {
            return (stem, suffix);
        }
    }
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && ext.len() <= 8
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            (stem, &name[stem.len()..])
        }
        _ => (name, ""),
    }
}

/// Disambiguate a shared-drive name that collides with a root section.
fn unique_name(name: &str, existing: &HashSet<String>) -> String {
    if !existing.contains(name) {
        return name.to_string();
    }
    let mut candidate = format!("{name} [Shared Drive]");
    let mut suffix = 2;
    while existing.contains(&candidate) {
        candidate = format!("{name} [Shared Drive {suffix}]");
        suffix += 1;
    }
    candidate
}

/// Split a path into `(parent_dir, last_segment)`. `/a/b` -> (`/a`, `b`);
/// `/a` -> (`/`, `a`).
/// A `&Path` in the form the resolver works in: `/` for the root, `/a/b` under it.
///
/// `..` is refused rather than walked. A Drive name survives the sanitizer with almost
/// anything in it, so a parent reference resolved here would let a path address a
/// directory nobody named — and the tree has no `..` of its own for it to mean.
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

/// The requested window of `data`, clamped to what exists.
///
/// Both ends are clamped and the end is never allowed below the start: a range that
/// asks backwards is empty, not a panic. The caller is a filesystem read, so the range
/// arrives from whatever a client asked for, and `data[4..2]` aborts the process.
fn slice(data: &[u8], range: Option<std::ops::Range<u64>>) -> Vec<u8> {
    match range {
        Some(r) => {
            let start = (r.start as usize).min(data.len());
            let end = (r.end as usize).min(data.len()).max(start);
            data[start..end].to_vec()
        }
        None => data.to_vec(),
    }
}

#[cfg(test)]
#[path = "gdrive_tests.rs"]
mod tests;
