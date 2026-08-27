use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::Mutex;

use unicode_normalization::UnicodeNormalization;

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
/// Drive stores, which is an estimate of the export and not a good one — see
/// `FILE_FIELDS`. This is what is left for the rows Drive lists without one at all,
/// which do occur (three spreadsheets in the corpus).
///
/// It cannot be made exact for those. A blob's length is one ranged byte away, but an
/// export honours no range at all (measured: `bytes=0-0` answers `200` with the whole
/// object, `HEAD` answers a `content-length` of `0`), so the only way to learn it is to
/// produce the whole document — which is what `find -size` and `ls -l` must not cost.
const UNKNOWN_LENGTH_SIZE: u64 = 8 * 1024 * 1024;

/// How much a blob read fetches once it is clear the reader is walking the file, so a
/// walk pays a round trip per span of this size rather than one per window.
///
/// The kernel's window is 64 KiB and not ours to choose. Sending each one down as its
/// own ranged request is what made a 641 MB archive take two and a half hours: 10,250
/// requests, and a request is 200 quota units however few bytes it moves.
///
/// Measured against Drive, a ranged request costs a round trip and then bytes, and the
/// two cross over around here:
///
/// ```text
///            time to first byte   total     MB/s
///   64 KiB         0.82 s         0.90 s    0.07
///    8 MiB         0.97 s         1.57 s    5.09
///   32 MiB         0.88 s         4.82 s    6.63
///   64 MiB         0.86 s         5.63 s   11.37
///  128 MiB         0.76 s        10.82 s   11.83
/// ```
///
/// The trip is flat at about 0.9 s whatever is asked for, so bigger spans keep paying
/// off until the transfer itself is the cost — which is at 64 MiB, where the rate tops
/// out around 11 MB/s. Past it only the request count falls, and 128 MiB measured
/// *slower* than 64 for the same archive.
///
/// This is not what the *first* fetch takes. That one is [`FIRST_SPAN`], so a reader that
/// stops after a buffer never pays for a span it will not use.
const READ_SPAN: u64 = 64 * 1024 * 1024;

/// What the first fetch of a file takes, before anything says the reader is walking it.
///
/// It cannot be the window the kernel asked for, however little that reader wants. The
/// mount is served over NFS, whose client fires around a megabyte of read-ahead the
/// moment a file is touched, and every window of it continues the one before — which is
/// a walk by any test this layer can apply. `head -c 8` measured 7.94 s that way: one
/// window, and then [`READ_SPAN`] fetched for read-ahead nobody would read.
///
/// 8 MiB swallows that read-ahead whole, so a head-read is one fetch of 1.57 s and stops
/// there. A reader that really is walking spends this span and gets [`READ_SPAN`] for the
/// next one, which is where the rate tops out.
const FIRST_SPAN: u64 = 8 * 1024 * 1024;

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
    /// For a document it is the size of what Drive *stores* — an estimate of the export
    /// that errs low on text by up to a factor of 23 measured, so it can refuse one that
    /// is already over and can never admit one safely, and is nowhere near the length a
    /// read answers with. `None` for either is [`UNKNOWN_LENGTH_SIZE`], via `entry_size`.
    size: Option<u64>,
}

/// One folder's children as the cache holds them: shared, so a `stat` or a read
/// borrows the listing instead of copying it (a shared folder in the corpus lists
/// 10,000 entries).
type CachedListing = (Instant, Arc<Vec<Child>>);

/// One document's export as the cache holds it: shared, so a read borrows the bytes
/// rather than copying a megabyte per chunk.
type CachedExport = (Instant, Arc<Vec<u8>>);

/// The one span of one blob this holds.
struct HeldSpan {
    id: String,
    /// Where in the file the span begins. A read cuts by absolute offset, so this is
    /// needed to answer one.
    at: u64,
    /// Drive answered shorter than the span asked for, which means the span runs to the
    /// end of the file. Without this a read near the end misses every time — the span
    /// does not reach the offset asked for, and never will.
    to_eof: bool,
    when: Instant,
    bytes: Arc<Vec<u8>>,
}

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
    /// The last span of a blob read, and where in which file it came from.
    ///
    /// One slot rather than a map, because a reader works through one file at a time —
    /// `cat`, `grep` and `cp` all do — so what the next window needs is what the last
    /// one had. A map would hold [`READ_SPAN`] per file ever touched until the TTL ran
    /// out; this holds it once.
    blob: Mutex<Option<HeldSpan>>,
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

    /// The span covering `want` bytes from `start`, from the slot when it reaches and
    /// from Drive otherwise. The span's own start comes back beside its bytes, because
    /// the caller cuts by an offset into the file rather than into the buffer.
    ///
    /// Spans begin where a reader asks rather than on fixed boundaries. A sequential
    /// walk then lands inside the held span until it is spent and starts the next one
    /// exactly where it left off, so no window is ever split across two spans and no
    /// read comes back short of what it asked for — which [`FileSystem::read_at`] would
    /// report to the kernel as the end of the file.
    ///
    /// How much a miss fetches depends on what the last one did. A read that carries on
    /// from where the held span ended is a reader walking the file, and gets a whole
    /// [`READ_SPAN`]; anything else — a different file, a jump to somewhere new — gets
    /// [`FIRST_SPAN`]. So a reader that stops after a buffer pays for 8 MiB it mostly
    /// throws away, and only a walk pays for the 64 MiB that a walk actually spends.
    ///
    /// A span is never smaller than the window asked for, so an oversized read is
    /// answered whole rather than truncated.
    async fn span(&self, id: &str, start: u64, want: u64) -> io::Result<(u64, Arc<Vec<u8>>)> {
        let walking = {
            let held = self.blob.lock().await;
            match held.as_ref() {
                Some(held) if held.id == id && held.when.elapsed() < DIR_TTL => {
                    if held.at <= start
                        && (start.saturating_add(want)
                            <= held.at.saturating_add(held.bytes.len() as u64)
                            || held.to_eof)
                    {
                        return Ok((held.at, held.bytes.clone()));
                    }
                    // Not covered, but beginning inside what was — at its end, or
                    // straddling it — so the reader spent the span and wants the next.
                    // Not just `== end`: a window whose size does not divide the span
                    // reaches past it from inside, and that is the same walk.
                    held.at <= start && start <= held.at.saturating_add(held.bytes.len() as u64)
                }
                _ => false,
            }
        };
        let len = want.max(if walking { READ_SPAN } else { FIRST_SPAN });
        let bytes = self
            .accessor
            .download(id, Some(start..start.saturating_add(len)))
            .await
            .map_err(not_found_or_backend)?;
        let to_eof = (bytes.len() as u64) < len;
        let bytes = Arc::new(bytes);
        *self.blob.lock().await = Some(HeldSpan {
            id: id.to_string(),
            at: start,
            to_eof,
            when: Instant::now(),
            bytes: bytes.clone(),
        });
        Ok((start, bytes))
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
    /// estimate of the export that errs low on text, so all it can do is refuse a document
    /// already over by that number *before a byte moves* — and a document it admits, or
    /// one Drive listed without a size at all, gets no such mercy, so
    /// [`MAX_DOCUMENT_BYTES`] is passed down as well and cuts the
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
            .find(|c| same_name(&c.vfs_name, &name) && c.kind.is_dir())
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
            .find(|c| same_name(&c.vfs_name, &name))
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
            // Answered out of a span: a window reaches Drive only when the span held
            // does not already cover it. Which file it is and how long, neither matters
            // here — a small file is simply one span that came back short, so the two
            // cases the size used to sort out are the same case.
            Serves::Original => {
                let Some(r) = range else {
                    // No window named at all, which is a direct caller asking for the
                    // object rather than a kernel asking for a chunk of it. Spanning it
                    // would answer with the first span and call that the whole file.
                    return self
                        .accessor
                        .download(&child.id, None)
                        .await
                        .map_err(not_found_or_backend);
                };
                // Saturating, because `end - start` on a backwards range underflows and
                // takes the process with it.
                let want = r.end.saturating_sub(r.start);
                // An empty window is not a read. `accessor::download` turns one away by
                // itself, but a span asks for a span's worth around it and would sail
                // straight past that — 8 MiB fetched to answer with an empty vector.
                if want == 0 {
                    return Ok(Vec::new());
                }
                let (at, bytes) = self.span(&child.id, r.start, want).await?;
                // Back to an offset into the buffer. `at <= r.start` holds for anything
                // `span` returns, so neither subtraction goes backwards.
                Ok(slice(&bytes, Some(r.start - at..r.end.saturating_sub(at))))
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
            // A file Drive listed without a size: learn the real one from one ranged
            // response header rather than leaving the placeholder, which would make
            // `ls -l` lie and a reader truncate the body at 8 MiB.
            let size = match (&child.serves, child.size) {
                (Serves::Original, None) => self.probed_len(&child.id).await,
                // A document keeps the estimate the listing gave, and a zip reader
                // cannot open one in place because of it. That is the trade, and it is
                // the way round it is because the number cannot be had cheaply.
                //
                // There is no cheap way. The link the listing carries sends no
                // `Content-Length`, answers `HEAD` with `0`, and ignores a range;
                // `files.export` does declare a length, but only by rendering the export
                // first — measured at 1.5-1.8 s for a document, 1.3-2.6 s for a
                // spreadsheet and 8.8-39.6 s for a deck, 5.9 s averaged over eleven, and
                // one of those spent 39.6 s to answer `403` because the export cleared
                // 10 MB even though the listing said 8.7. Nor is a render kept: three
                // `HEAD`s in a row on one deck cost 8.81, 9.11 and 8.29 s.
                //
                // And `stat` is asked once per entry per listing, because FUSE-T serves
                // the mount over NFS and an NFS client fills an attribute for every name
                // it lists. A shared folder of 129 entries, 40 of them documents, listed
                // in 108 s when this produced the lengths against 1.09 s when it did not,
                // and nothing about that scales — the cost is per document, forever.
                //
                // What the estimate costs in exchange is bounded and known. A zip reader
                // scans backwards from the end it was told, over a window bisected
                // against Info-ZIP at 70,639 bytes, and the listing's size missed by
                // −4,910,139 to +533,322 across six measured documents — both directions,
                // so no margin fits. `unzip` on a document therefore reports an intact
                // file corrupt, and a copy of it carries the kernel's zero padding rather
                // than the real end. Blob files are unaffected: Drive sizes those
                // exactly.
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
            // Short is EOF and nothing else, which holds because a span never splits a
            // window: `span` fetches from where the read begins and at least as far as it
            // asks, so what comes back either covers the window or ran out of file.
            //
            // A short answer would not reach a caller as one anyway. Through a mount the
            // kernel fills whatever this declines to serve, out to the length it was told
            // the file has: measured on a document whose `stat` over-reported by 152,083
            // bytes, `cat` handed back exactly the claimed 60,063,731 with the tail all
            // `0x00`. So `cp` does not rescue a wrong length — it copies the padding — and
            // a reader looking for anything at the end finds zeros. For documents that is
            // now an accepted cost rather than a bug; see the revert of 1c9e67b for what
            // the alternative was measured to cost.
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
        // Drive's `size` is what it *stores*, which is an estimate of the export and
        // wrong by up to a factor of 23 on text. It is treated as the estimate it is:
        // `entry_size` reports it, and `read_window` refuses on it before a byte moves,
        // but the length a read answers with is the length that arrived.
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
/// Whether two names are the same name, in the sense a directory has to mean it.
///
/// One name has two spellings in Unicode — `한` is a single code point composed, or three
/// jamo decomposed, and Japanese voiced marks work the same way — and both spellings are
/// in play at once here, from two directions.
///
/// macOS hands a lookup the *decomposed* form of whatever a listing returned, whichever
/// form the listing used. And Drive stores whichever form the client that uploaded a file
/// happened to send: measured on one real folder, ten names composed and four decomposed,
/// side by side.
///
/// So a byte comparison answers `ENOENT` for a name `ls` printed a moment earlier — and
/// which files it does that to depends on what uploaded them, which is the worst kind of
/// inconsistency to debug. Measured before this: of eight Korean-named files, seven opened
/// only under the composed spelling and one only under the decomposed one, while `readdir`
/// gave every one of them decomposed.
///
/// Bytes first, because that is the answer for every ASCII name and most others; the
/// composition runs only when a byte comparison has already failed.
fn same_name(a: &str, b: &str) -> bool {
    a == b || (a.is_ascii() == b.is_ascii() && !a.is_ascii() && a.nfc().eq(b.nfc()))
}

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
