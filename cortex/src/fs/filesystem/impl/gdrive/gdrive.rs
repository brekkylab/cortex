use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::Mutex;

use super::accessor::{GdriveAccessor, GdriveConfig, MAX_DOCUMENT_BYTES};
use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, Stat},
};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// The mount root mirrors Drive's own sidebar as virtual sections. `My Drive` is the
/// literal Drive folder id `root`, and a shared drive has a drive id of its own —
/// but `Shared with me` answers to no id at all, because Drive holds no folder for
/// it. What it gathers carries no `parents`, so the folder tree alone would never
/// surface any of it. [`GKind`] is what tells the three apart; see [`Listing`] for
/// where that decides the query. Names are Drive's own English section labels.
const MY_DRIVE_ID: &str = "root";
const MY_DRIVE_NAME: &str = "My Drive";
const SHARED_WITH_ME_NAME: &str = "Shared with me";

/// Whether a listing entry's name is one an export would produce.
///
/// It cannot answer whether the entry *is* an export: an uploaded `.pptx` reads the same
/// as an exported deck, which is what serving the export buys. Only [`Serves`] separates
/// them, and only `resolve` has it. This is the weaker question a name can answer, and
/// it is asked of [`NATIVE_KINDS`] so a type added there is covered without being added
/// twice.
#[cfg(test)]
fn looks_like_an_export(e: &Dirent) -> bool {
    NATIVE_KINDS
        .iter()
        .any(|(_, _, _, ext)| e.name.ends_with(ext))
}

/// How each Docs-editors type is served: what Drive exports it as, and the extension
/// the entry then carries.
///
/// These types hold no bytes of their own. `alt=media` refuses them outright
/// (measured: `403`, *"Only files with binary content can be downloaded. Use Export
/// with Docs Editors files."*), so an export is the only form there is, and the
/// Office one is the form a reader can open.
///
/// The extension is the export's own, because that is now what the bytes are. A Drive
/// name carries none of its own, so the suffix is also what says which of the three
/// the entry came from.
///
/// **A spreadsheet loses more than layout here.** Google exports an in-cell image as a
/// picture floating over the sheet rather than as the cell's value, so a `VLOOKUP` that
/// returned the image in Sheets returns `#N/A` in Excel; named ranges can arrive as
/// `#REF!` in the same file. Measured on a real workbook, and reproduced by downloading
/// it from Drive's own UI — it is Google's export, not this path. Nothing here can
/// repair it: the cell has no cached value to fall back on.
const NATIVE_KINDS: &[(&str, NativeApi, &str, &str)] = &[
    (
        "application/vnd.google-apps.document",
        NativeApi::Doc,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ".docx",
    ),
    (
        "application/vnd.google-apps.spreadsheet",
        NativeApi::Sheet,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ".xlsx",
    ),
    (
        "application/vnd.google-apps.presentation",
        NativeApi::Slides,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ".pptx",
    ),
];

/// The export MIME and extension for a native mime, if it is one.
fn native_kind(mime: &str) -> Option<(NativeApi, &'static str, &'static str)> {
    NATIVE_KINDS
        .iter()
        .find(|(m, _, _, _)| *m == mime)
        .map(|(_, api, export, ext)| (*api, *export, *ext))
}

/// The export MIME this document is served as.
fn export_mime(api: NativeApi) -> &'static str {
    NATIVE_KINDS
        .iter()
        .find(|(_, a, _, _)| *a == api)
        .map(|(_, _, m, _)| *m)
        .unwrap_or("")
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
/// Size reported for a document whose length nobody has learned yet.
///
/// It cannot be 0. Reads run under the guest's FUSE mount with `direct_io`, and a
/// 0-length file was measured (in ailoy's Drive mount, which hit this first) to
/// clamp reads to nothing — and a search tool skips a file it is told is empty. An
/// over-estimate is safe in the other direction: the read returns the real bytes
/// and then empty at EOF, so `cat` stops at the true end.
///
/// Rarely reached, now that a document reports Drive's own `size`: that number is what
/// Drive stores, which is what an export hands back — within 0.3% on the documents
/// measured. This is what is left for the rows Drive lists without one, which do
/// occur (three spreadsheets in the corpus).
///
/// It cannot be made exact for those. A blob's length is one ranged byte away, but an
/// export honours no range at all (measured: `bytes=0-0` answers `200` with the whole
/// object, `HEAD` answers a `content-length` of `0`), so the only way to learn it is to
/// produce the whole document — which is what `find -size` and `ls -l` must not cost.
const UNKNOWN_LENGTH_SIZE: u64 = 8 * 1024 * 1024;

/// A blob at or under this is fetched whole, once, and every chunk after that is cut
/// out of what is held.
///
/// Drive charges by the request and not by the byte: a download is 200 quota units
/// however few bytes it moves, so a 20 MB file read at 256 KiB a time costs 16,000
/// units where reading it whole costs 200. The ranged read is still the right shape
/// past this line — a reader that wants a file's head, or a `grep` that abandons it
/// after one buffer, must not pay for the rest — and the 1.79 GB archive in the corpus
/// answers its first 4 KiB in a second because of it.
///
/// 8 MiB is where the two stop trading evenly. Under it a whole fetch is a handful of
/// chunks' worth of bytes and one request instead of dozens; over it the bytes a
/// head-read would waste outgrow what the requests cost.
const WHOLE_BLOB_LIMIT: u64 = 8 * 1024 * 1024;

/// Cap on remembered probe results — one per file ever sized in this mount.
const MAX_PROBED_LENGTHS: usize = 50_000;

/// Safety ceiling on one folder's listing (10 pages). Beyond this the listing
/// truncates (the accessor logs it) — a >10k-child folder is pathological to
/// `ls` anyway.
const MAX_FOLDER_FILES: usize = 10_000;

/// Whether Drive holds real bytes for this row. The Docs-editors types (and Forms,
/// Maps, Drawings) do not — `alt=media` answers *"Only files with binary content can be
/// downloaded. Use Export with Docs Editors files."* The first three are exported
/// instead; the rest export to nothing and are not listed.
fn has_original_bytes(mime: &str) -> bool {
    !mime.starts_with("application/vnd.google-apps.")
}

/// What an entry hands back when read.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Serves {
    /// A directory — nothing to read.
    Nothing,
    /// The file's own bytes (`alt=media`), ranged. Drive holds these, so a reader can
    /// seek inside one and pay for the window it asks for.
    Original,
    /// A Docs-editors document, exported to its Office form. The link comes from the
    /// listing rather than from a path this builds — see `FILE_FIELDS`.
    Native(NativeApi, String),
}

/// Which Docs-editors type a document is.
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
    /// The `Shared with me` section, which is not a folder anywhere: Drive has no id
    /// for it, and only `sharedWithMe=true` gathers what it holds.
    SharedWithMe,
    File,
}

impl GKind {
    fn is_dir(self) -> bool {
        matches!(
            self,
            GKind::Folder | GKind::SharedDrive | GKind::SharedWithMe
        )
    }
}

/// What listing a directory takes, which is not the same question as what it is called.
///
/// Two of the root's three sections resolve to an id Drive knows, and the third
/// resolves to none: `Shared with me` is a view rather than a folder. An `Option<String>`
/// would say the same thing and let a caller reach past it into a query that needs an id
/// — this cannot be read without deciding which of the two it is.
enum Listing {
    /// A real Drive folder. `drive_id` scopes it to a shared drive when it lives in one.
    Folder {
        id: String,
        drive_id: Option<String>,
    },
    /// Everything shared with this account. It has no id to ask about.
    SharedWithMe,
}

/// One resolved Drive entry, as the VFS sees it.
#[derive(Clone)]
struct Child {
    /// Listing name: the sanitized (and, on collision, disambiguated) Drive name —
    /// plus the export's extension when the entry is a Docs-editors document, since a
    /// Drive name carries none of its own.
    vfs_name: String,
    id: String,
    /// Set when the entry lives in a shared drive (listing scope).
    drive_id: Option<String>,
    kind: GKind,
    mtime: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    /// What this entry hands back when read.
    serves: Serves,
    /// Byte length as the listing reported it, which means two different things.
    ///
    /// For a file Drive holds bytes for it is exact, and a reader can seek inside it.
    /// For a document it is the size of what Drive *stores* — an estimate of the
    /// export, within 0.3% on the two measured — good enough to refuse an oversized
    /// one before a byte moves, and not good enough to be the length a read answers
    /// with. `None` for either is [`UNKNOWN_LENGTH_SIZE`], via `entry_size`.
    size: Option<u64>,
}

/// One folder's children as the cache holds them: shared, so a `stat` or a read
/// borrows the listing instead of copying it (a shared folder in the corpus lists
/// 10,000 entries).
type CachedListing = (Instant, Arc<Vec<Child>>);

/// One document's export as the cache holds it: shared, so a read borrows the bytes
/// rather than copying a megabyte per chunk.
type CachedExport = (Instant, Arc<Vec<u8>>);

/// The one small blob held whole, and which file it is: `(id, fetched-at, bytes)`.
type HeldBlob = (String, Instant, Arc<Vec<u8>>);

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
    /// A document's exported bytes, once fetched (file id → bytes).
    ///
    /// A document has no windows: an export honours no range, so a read of one produces
    /// all of it however few bytes were asked for. A kernel asks in chunks, so without
    /// this a 1 MB document read at 128 KiB a time is eight exports of the same
    /// document — and an export is seconds, 44 of them measured on a 13.6 MB deck. Held
    /// as an `Arc` because a read borrows it rather than copying a megabyte per chunk.
    exports: Mutex<HashMap<String, CachedExport>>,
    /// The last small blob read whole: `(file id, when, bytes)`.
    ///
    /// One slot rather than a map, because a reader works through one file at a time —
    /// `cat`, `grep` and `cp` all do — so what the next chunk needs is what the last
    /// chunk had. A map would hold [`WHOLE_BLOB_LIMIT`] per file ever touched until the
    /// TTL ran out; this holds it once.
    blob: Mutex<Option<HeldBlob>>,
}

impl GdriveFs {
    pub fn new(config: &GdriveConfig) -> anyhow::Result<Self> {
        Ok(Self {
            accessor: GdriveAccessor::new(config)?,
            probed: Mutex::new(HashMap::new()),
            dir_cache: Mutex::new(HashMap::new()),
            exports: Mutex::new(HashMap::new()),
            blob: Mutex::new(None),
        })
    }

    /// A small blob's bytes, whole, from the slot when they are fresh and from Drive
    /// otherwise.
    ///
    /// Unranged on purpose: a range is what makes Drive answer twice for one file, and
    /// this exists to make it answer once. See [`WHOLE_BLOB_LIMIT`].
    async fn whole_blob(&self, id: &str) -> io::Result<Arc<Vec<u8>>> {
        if let Some((held, at, bytes)) = self.blob.lock().await.as_ref()
            && held == id
            && at.elapsed() < DIR_TTL
        {
            return Ok(bytes.clone());
        }
        let bytes = self
            .accessor
            .download(id, None)
            .await
            .map_err(not_found_or_backend)?;
        let bytes = Arc::new(bytes);
        *self.blob.lock().await = Some((id.to_string(), Instant::now(), bytes.clone()));
        Ok(bytes)
    }

    /// A document's exported bytes, from the cache when they are fresh and from Drive
    /// otherwise.
    ///
    /// On [`DIR_TTL`] rather than a number of its own, for the reason that constant
    /// already gives: two TTLs mean a span in which `ls` answers from one snapshot while
    /// a read of the same document answers from another, and the listing is what named
    /// the document in the first place.
    ///
    /// Two ceilings, because only one of them can be trusted. The listing's `size` is an
    /// estimate of the export (within 0.3% on the documents measured), so it refuses an
    /// oversized document *before a byte moves* — and a document Drive listed without one
    /// gets no such mercy, so [`MAX_DOCUMENT_BYTES`] is passed down as well and cuts the
    /// stream off frame by frame. Neither is redundant: the first is fast and
    /// approximate, the second is exact and expensive.
    async fn exported(
        &self,
        id: &str,
        api: NativeApi,
        link: &str,
        listed: Option<u64>,
    ) -> io::Result<Arc<Vec<u8>>> {
        if let Some((at, bytes)) = self.exports.lock().await.get(id)
            && at.elapsed() < DIR_TTL
        {
            return Ok(bytes.clone());
        }
        // Refused on the estimate, before anything is fetched. Without this the only
        // ceiling is the one that counts frames as they arrive, which means paying
        // `MAX_DOCUMENT_BYTES` of transfer to learn a document is too big — every time
        // the cache goes cold. A 401 MB workbook exists in the corpus.
        if let Some(n) = listed
            && n > MAX_DOCUMENT_BYTES
        {
            return Err(io::Error::other(format!(
                "the document is {n} bytes, over the {MAX_DOCUMENT_BYTES} byte limit \
                 for a whole-document read"
            )));
        }
        let bytes = self
            .accessor
            .export_document(link, export_mime(api), MAX_DOCUMENT_BYTES)
            .await
            .map_err(not_found_or_backend)?;
        let bytes = Arc::new(bytes);
        let mut cache = self.exports.lock().await;
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
            // No id: nothing reads one for this kind, and `Listing` is what makes that
            // hold rather than a convention.
            section(SHARED_WITH_ME_NAME, "", GKind::SharedWithMe, None),
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
            let listing = self.how_to_list(folder).await?;
            let (files, drive_id) = match &listing {
                Listing::SharedWithMe => (
                    self.accessor.list_shared_with_me(MAX_FOLDER_FILES).await,
                    None,
                ),
                Listing::Folder { id, drive_id } => (
                    self.accessor
                        .list_files(id, drive_id.as_deref(), MAX_FOLDER_FILES)
                        .await,
                    drive_id.clone(),
                ),
            };
            let files = files.map_err(not_found_or_backend)?;
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
    /// What listing `folder` takes, found by asking its parent about it.
    ///
    /// The parent knew what each of its children was; a path does not carry that, so the
    /// listing of the parent is where it is recovered. A shared drive's `driveId` travels
    /// this way too — the rows inside one do not carry it, so each level hands it down.
    async fn how_to_list(&self, folder: &str) -> io::Result<Listing> {
        let (parent, name) = split_last(folder);
        let children = Box::pin(self.list_dir(&parent)).await?;
        let entry = children
            .iter()
            .find(|c| c.vfs_name == name && c.kind.is_dir())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(match entry.kind {
            GKind::SharedWithMe => Listing::SharedWithMe,
            _ => Listing::Folder {
                id: entry.id.clone(),
                drive_id: entry.drive_id.clone(),
            },
        })
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
                // Small enough to hold: one unranged fetch answers every chunk that
                // follows. The length has to be known without asking for it — a probe
                // is a request, which is the cost this is avoiding — so a blob Drive
                // listed without a size reads by the window unless `stat` already
                // learned it.
                let known = match child.size {
                    Some(n) => Some(n),
                    None => self.probed.lock().await.get(&child.id).copied(),
                };
                if let Some(n) = known
                    && n <= WHOLE_BLOB_LIMIT
                {
                    let bytes = self.whole_blob(&child.id).await?;
                    return Ok(slice(&bytes, range));
                }
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
            // A document, exported once and then held. An export honours no range
            // (measured: `bytes=0-0` answers `200` with the whole object, and `HEAD`
            // answers `content-length: 0`), so the window is ours to cut out of what
            // the export handed back.
            Serves::Native(api, link) => {
                let bytes = self.exported(&child.id, api, &link, child.size).await?;
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
            // The length a caller is about to seek by, so it is worth a request to get
            // right. Every tool that reads a file's end reads this number: `lseek(0,
            // SEEK_END)` is answered from the kernel's cached attributes and never asks
            // the file, so an estimate here is an estimate for `unzip`, `tar`, `mmap`
            // and everything built on them. A zip is the sharp case — its directory sits
            // at the end, and a reader that seeks to the wrong end reports a corrupt
            // archive for a file that is intact.
            let size = match (&child.serves, child.size) {
                // Drive listed no length: one ranged response header has it, which is
                // cheaper than the object and cached for the session.
                (Serves::Original, None) => self.probed_len(&child.id).await,
                // A document's length is knowable no other way than by producing it —
                // there is no `Content-Length`, `HEAD` answers `0`, and a range is
                // ignored. So this exports it, and the export is held: whatever reads
                // next spends nothing, because a document has no windows and would have
                // produced the same bytes anyway.
                //
                // Only `ls -l`, `du` and `find -size` pay for this without reading, and
                // only for documents small enough to serve. Past the ceiling nothing can
                // read the document at all, so its length is left an estimate — there is
                // no one to mislead.
                //
                // An estimate could not have been made close enough. A zip reader scans
                // backwards from the end it was told, over a bounded window: bisected
                // against Info-ZIP on a real export, 70,639 bytes of over-report still
                // opened and 70,640 did not. Drive's listed size missed by −4,910,139 to
                // +533,322 across five measured documents, both directions, so no margin
                // fits inside that window. The number has to be produced, not guessed.
                (Serves::Native(api, link), listed)
                    if listed.is_none_or(|n| n <= MAX_DOCUMENT_BYTES) =>
                {
                    self.exported(&child.id, *api, link, listed)
                        .await
                        .ok()
                        .map(|b| b.len() as u64)
                }
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
            //
            // A short answer does not reach a caller as one, though, and that is why the
            // size `stat` reports has to be right rather than close. Through a mount the
            // kernel fills what this declines to serve, out to the length it was told the
            // file has: measured on a document whose `stat` over-reported by 152,083
            // bytes, `cat` handed back exactly the claimed 60,063,731 with the tail all
            // `0x00`. So `cp` does not rescue a wrong length — it copies the padding — and
            // a reader looking for anything at the end finds zeros.
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
    // A document's length is not free from a listing, so this does not claim one. Drive's
    // `size` estimates the export within 0.3% on the files measured, which is close
    // enough to refuse an oversized one and not close enough to be a length: a reader
    // seeking to that end lands in the wrong place. [`FileSystem::stat`] produces the
    // export and answers exactly, and a consumer that wants the number asks for it there.
    //
    // The FUSE bindings are unaffected either way — neither implements `readdirplus`, so
    // a listing's attributes never reach a kernel. This is for the consumers that do read
    // them, and it keeps them from meeting an estimate dressed as a measurement.
    if matches!(c.serves, Serves::Native(..)) {
        return Dirent::new(c.vfs_name.clone(), kind_of(c));
    }
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
    let listed_size = f
        .get("size")
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<u64>().ok());
    let (vfs_name, serves, size) = if has_original_bytes(mime) {
        // Drive reports the length up front, so this entry is honest about its
        // size and a reader can seek inside it.
        (name, Serves::Original, listed_size)
    } else {
        // A native document, exported. Without a link there is nothing to serve, so
        // the row becomes no entry at all — the same answer this gives a Form or a
        // Drawing, and for the same reason: a name that cannot be read is worse than
        // an absence.
        let (api, export, ext) = native_kind(mime)?;
        let link = f.get("exportLinks")?.get(export)?.as_str()?.to_string();
        // Drive's `size` is what it *stores*, which is what an export hands back —
        // within 0.3% on the two documents measured. It is an estimate and treated as
        // one: `entry_size` reports it, and `read_window` refuses on it before a byte
        // moves, but the length a read answers with is the length that arrived.
        (
            format!("{name}{ext}"),
            Serves::Native(api, link),
            listed_size,
        )
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
/// The number goes *before* the extension. Appending it (`sheet.xlsx (2)`) kept the
/// name unique but took the entry out of every glob a reader would use to find it —
/// measured against a real account, two of 33 spreadsheets were invisible to
/// `**/*.xlsx`.
fn disambiguate(children: &mut [Child]) {
    let mut seen: HashSet<String> = HashSet::new();
    for c in children.iter_mut() {
        if seen.insert(c.vfs_name.clone()) {
            continue;
        }
        let (stem, ext) = split_extension(&c.vfs_name, &c.serves);
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
/// A document's extension is known exactly, from [`NATIVE_KINDS`] rather than from the
/// name. A file keeps whatever follows its last dot when that looks like an extension.
/// A directory has none to protect, so `v1.2` numbers as `v1.2 (2)`.
fn split_extension<'a>(name: &'a str, serves: &Serves) -> (&'a str, &'a str) {
    if *serves == Serves::Nothing {
        return (name, "");
    }
    if let Serves::Native(api, _) = serves {
        let suffix = NATIVE_KINDS
            .iter()
            .find(|(_, a, _, _)| a == api)
            .map(|(_, _, _, ext)| *ext)
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
