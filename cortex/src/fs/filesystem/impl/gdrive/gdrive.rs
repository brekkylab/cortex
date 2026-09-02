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
/// clamp reads to nothing — and a search tool skips a file it is told is empty.
///
/// An over-estimate does *not* mean `cat` stops at the true end: the client bounds a
/// read by the length it was told, so it asks for the whole span and takes back the whole
/// span. [`FileSystem::read_at`] fills the part past the JSON for that reason. Before it
/// did, the kernel filled it with `0x00` and every JSON parser threw at the seam —
/// measured on a live mount, a 1,574,113-byte deck read as 8,388,608 bytes of which
/// 6,814,495 were zeros, and `json.load` raised rather than skipped.
///
/// [`MAX_DOCUMENT_BYTES`], and equal to it on purpose rather than by coincidence. An
/// *under*-estimate is the failure with no recovery: the reader stops where it was told
/// to, every window full, so nothing reports a short read and the JSON simply ends
/// mid-token. Reproduced against the mock at the old 8 MiB — a 12,582,929-byte document
/// read back as exactly 8,388,608 bytes that do not parse, while a second read inside the
/// TTL succeeded because the cache then knew the real length.
///
/// Tying it to the accessor's ceiling is what makes that unreachable rather than
/// unlikely. `get_pretty` bounds the *raw* body at that number and Google already returns
/// 2-space pretty JSON — measured, raw 2,491,712 against 2,491,642 re-serialized — so the
/// served bytes cannot exceed it. A document that would has no length to under-state,
/// because `body_within` refuses it and the read fails loudly instead.
///
/// A document reports its exact length from the moment something first reads it, so this
/// is what an *unread* document shows and not what a document shows. Measured JSON
/// lengths across the corpus were 7 KB to 3.4 MB.
///
/// This is a placeholder, not a measurement — `find -size` and `ls -l` see it until
/// something reads the file. Making it exact up front costs one render per
/// document: measured 2.4s for a six-document folder listing, 6.3s when the
/// kernel's per-entry `getattr` serialises them — and `stat` runs once per name, because
/// FUSE-T serves over NFS and an NFS client fills an attribute for every entry it lists.
const UNKNOWN_LENGTH_SIZE: u64 = MAX_DOCUMENT_BYTES;

/// How long a line the padding past a document's JSON is broken into.
///
/// The padding is whitespace either way — JSON ignores what follows a value — so this only
/// decides what shape the tail has for the tools that read it. See [`FileSystem::read_at`]
/// for the measurement that picked spaces over newlines, and this over all spaces.
const PAD_LINE: u64 = 4096;

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
const MAX_REMEMBERED_LENGTHS: usize = 50_000;

/// Safety ceiling on one folder's listing (10 pages). Beyond this the listing
/// truncates (the accessor logs it) — a >10k-child folder is pathological to
/// `ls` anyway.
const MAX_FOLDER_FILES: usize = 10_000;

/// How many characters of a Drive id a colliding name carries. See [`id_tag`], which
/// takes them from the end for a measured reason.
const ID_TAG_LEN: usize = 8;

/// Whether Drive holds real bytes for this row. The Docs-editors types (and Forms,
/// Maps, Drawings) do not — `alt=media` answers *"Only files with binary content can be
/// downloaded. Use Export with Docs Editors files."* The first three have APIs of their
/// own and are served from those; the rest have nothing to serve and are not listed.
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
    /// Byte length as the listing reported it, which means two different things.
    ///
    /// For a file Drive holds bytes for it is exact, and a reader can seek inside it. For
    /// a document it is the size of what Drive *stores*, which describes neither the JSON
    /// served nor anything else a reader sees — 4,775 stored against 133,625 of JSON on
    /// one measured document. It is kept for the ceiling check and nothing else; the
    /// length an entry reports comes from `entry_size`.
    size: Option<u64>,
}

/// One folder's children as the cache holds them: shared, so a `stat` or a read
/// borrows the listing instead of copying it (a shared folder in the corpus lists
/// 10,000 entries).
type CachedListing = (Instant, Arc<Vec<Child>>);

/// One document's JSON as the cache holds it: shared, so a read borrows the bytes
/// rather than copying a megabyte per chunk.
/// A document's served length beside the `modifiedTime` it was measured at. Stored with
/// the length rather than folded into the key: the map holds one row per document, and an
/// edit replaces that row instead of leaving the old one behind unreadable.
type RememberedLength = (Option<std::time::SystemTime>, u64);

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
    lengths: Mutex<HashMap<String, RememberedLength>>,
    /// Per-directory listing cache (folder path → children). Path resolution walks
    /// parent listings, so one cached listing answers readdir, stat and the lookup
    /// a read starts with; fetching the bytes themselves still costs a request.
    dir_cache: Mutex<HashMap<String, CachedListing>>,
    /// The last content read, and where in which file it came from.
    ///
    /// One slot rather than a map, because a reader works through one file at a time —
    /// `cat`, `grep` and `cp` all do — so what the next window needs is what the last one
    /// had. A map would hold [`READ_SPAN`] per file ever touched until the TTL ran out;
    /// this holds it once.
    ///
    /// A document lands here too, as the span it is: a whole file at offset 0. Its API has
    /// no ranges, so a read of any window produces all of it, and the next window has to
    /// find it here or produce it again — 3.4 MB read at 64 KiB a time is 52 windows and
    /// would be 52 renders at about 1.8 s each. Held apart from a blob's span at first,
    /// under a byte budget of its own; the two turned out to be the same slot with the
    /// same reason under it, and one of them can hold what the reader is reading.
    held: Mutex<Option<HeldSpan>>,
}

impl GdriveFs {
    pub fn new(config: &GdriveConfig) -> anyhow::Result<Self> {
        Ok(Self {
            accessor: GdriveAccessor::new(config)?,
            lengths: Mutex::new(HashMap::new()),
            dir_cache: Mutex::new(HashMap::new()),
            held: Mutex::new(None),
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
            let slot = self.held.lock().await;
            match slot.as_ref() {
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
        *self.held.lock().await = Some(HeldSpan {
            id: id.to_string(),
            at: start,
            to_eof,
            when: Instant::now(),
            bytes: bytes.clone(),
        });
        Ok((start, bytes))
    }

    /// A document's JSON, from the slot when it is the document being read and from its
    /// own API otherwise.
    ///
    /// Stored as what it is: a span of the whole file at offset 0, in the same slot a
    /// blob's span goes in. The two were separate caches with the same sentence under
    /// each — whatever is being read is held and nothing more is promised — and a map
    /// under a byte budget was buying what one slot already gives, at the cost of an
    /// eviction loop and a second [`READ_SPAN`]-sized ceiling to hold at once.
    ///
    /// `to_eof`, because there is no more of it: the API answers with the whole document
    /// or nothing, which is also why this has to exist at all. Without it a 3.4 MB
    /// document read at 64 KiB a time is 52 renders of the same document.
    async fn rendered_json(&self, child: &Child, api: NativeApi) -> io::Result<Arc<Vec<u8>>> {
        let id = child.id.as_str();
        {
            let slot = self.held.lock().await;
            if let Some(held) = slot.as_ref()
                && held.id == id
                && held.at == 0
                && held.to_eof
                && held.when.elapsed() < DIR_TTL
            {
                return Ok(held.bytes.clone());
            }
        }
        let bytes = match api {
            NativeApi::Sheet => self.spreadsheet_bytes(id).await,
            NativeApi::Doc => self.accessor.document_json(id).await,
            NativeApi::Slides => self.accessor.presentation_json(id).await,
        }
        .map_err(not_found_or_backend)?;
        let bytes = Arc::new(bytes);
        *self.held.lock().await = Some(HeldSpan {
            id: id.to_string(),
            at: 0,
            to_eof: true,
            when: Instant::now(),
            bytes: bytes.clone(),
        });
        // Outlives the bytes above, so a listing after they age out still knows the length.
        self.remember_len(child, bytes.len() as u64).await;
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
            // Composed, because that is what `resolve` compares by. Two shared drives
            // spelled the same name two ways would otherwise both keep it, and one of
            // them would be unreachable from the root.
            let mut existing: HashSet<String> = children
                .iter()
                .map(|c| c.vfs_name.nfc().collect())
                .collect();
            for d in &drives {
                if let (Some(id), Some(name)) = (
                    d.get("id").and_then(|x| x.as_str()),
                    d.get("name").and_then(|x| x.as_str()),
                ) {
                    let vfs_name = unique_name(&sanitize_name(name), &existing);
                    existing.insert(vfs_name.nfc().collect());
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

    /// The length `stat` reports for a document, and the memory that keeps it from going
    /// back to the placeholder.
    ///
    /// Held apart from the bytes on purpose. The two are worth very different amounts —
    /// the JSON is megabytes and expires on [`DIR_TTL`] under a byte budget, the length is
    /// eight bytes and has no reason to expire at all — so keeping them together meant a
    /// document read six minutes ago listed at the placeholder again. Against a live
    /// account that is `ls -l` saying 3.4 MB and then saying 64 MiB for the same unchanged
    /// file.
    ///
    /// Stamped with `modifiedTime` rather than aged by a clock, which is both more
    /// accurate and cheaper. A document that has not changed keeps its length
    /// indefinitely, and one that has loses it the moment a listing says so. A TTL would
    /// do the opposite of each: discard a length that is still right, and serve one that
    /// is already wrong until it lapses.
    ///
    /// The stamp rides in the value and the *id* is the key, so an edit replaces the entry
    /// rather than adding one. Keying the pair would leave a row per version behind — a
    /// document edited often would fill the map by itself — and none of the old rows could
    /// ever be read again.
    ///
    /// Losing an entry costs a listing's accuracy and never correctness, which is what
    /// lets the bound be a flat cap and a clear rather than bookkeeping — the next read
    /// puts it back, and a read produces the JSON either way.
    async fn remembered_len(&self, child: &Child) -> Option<u64> {
        // The bytes first, while they are still held: `stat` and a read of the same
        // document answer from the same place or they disagree about where it ends.
        let in_hand = {
            let slot = self.held.lock().await;
            slot.as_ref()
                .filter(|h| h.id == child.id && h.at == 0 && h.to_eof)
                .filter(|h| h.when.elapsed() < DIR_TTL)
                .map(|h| h.bytes.len() as u64)
        };
        if in_hand.is_some() {
            return in_hand;
        }
        self.lengths
            .lock()
            .await
            .get(&child.id)
            .filter(|(mtime, _)| *mtime == child.mtime)
            .map(|(_, len)| *len)
    }

    /// Remember what a document was served as, against the `modifiedTime` it had then.
    ///
    /// A row that states no `modifiedTime` is not remembered at all. Storing one would
    /// stamp it `None`, and `None` matches `None` — the entry would then be valid forever
    /// with nothing able to retire it, and there is no TTL underneath to catch that. A
    /// length that outlives the document it measured is short as soon as the document
    /// grows, and a short length is the one kind `read_at` cannot pad around: the reader
    /// stops where it was told, which is the failure [`UNKNOWN_LENGTH_SIZE`] is tied to
    /// the accessor's ceiling to prevent. Better to answer the placeholder, which is the
    /// honest answer for a length nothing can date.
    async fn remember_len(&self, child: &Child, len: u64) {
        if child.mtime.is_none() {
            return;
        }
        let mut lengths = self.lengths.lock().await;
        if lengths.len() >= MAX_REMEMBERED_LENGTHS {
            lengths.clear();
        }
        lengths.insert(child.id.clone(), (child.mtime, len));
    }

    /// How many listings are retained. Tests only: growth here is invisible from
    /// outside, which is how it went unbounded.
    #[cfg(test)]
    pub(crate) async fn listings_retained(&self) -> usize {
        self.dir_cache.lock().await.len()
    }

    /// What the one slot is holding: the Drive id and how many bytes. Tests only — a
    /// slot nothing can see is a slot nothing keeps. Which *kind* it is cannot be read
    /// off the span (a small blob read from 0 is also `at: 0, to_eof`), so the id is what
    /// a caller compares, exactly as `rendered_json` and `remembered_len` do.
    #[cfg(test)]
    pub(crate) async fn held_slot(&self) -> Option<(String, u64)> {
        let slot = self.held.lock().await;
        slot.as_ref().map(|h| (h.id.clone(), h.bytes.len() as u64))
    }

    /// Drop the produced bytes while keeping what was learned from them. Tests only —
    /// it is the state a document reaches on its own, by the byte budget or the TTL.
    #[cfg(test)]
    pub(crate) async fn forget_rendered_for_test(&self) {
        *self.held.lock().await = None;
    }

    /// How many document lengths are remembered. Tests only.
    #[cfg(test)]
    pub(crate) async fn lengths_remembered(&self) -> usize {
        self.lengths.lock().await.len()
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
    /// Whether this path is served as a document's own JSON.
    ///
    /// What [`FileSystem::read_at`] pads with is only sound because the answer is yes: a
    /// blob is bytes and padding one corrupts it, where a document is JSON and JSON is
    /// defined to ignore the whitespace after it.
    async fn serves_json(&self, path: &Path) -> bool {
        let Ok(path) = vpath(path) else { return false };
        self.resolve(&path)
            .await
            .is_ok_and(|c| matches!(c.serves, Serves::Native(..)))
    }

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
                // Saturating, and not as a guard against a client: `read_at` is the only
                // caller and builds `offset..offset + buf.len()`, so this cannot go
                // backwards. It is that the zero check below has to be here anyway — that
                // `offset + buf.len()` saturates, so an offset at the top of the range
                // gives an empty window — and saturating here is the free way to land a
                // degenerate range in a branch that already exists. Plain `-` would wrap
                // in release, and `want` near `u64::MAX` asks Drive for everything from
                // `start` to the end of the file to answer with an empty vector.
                let want = r.end.saturating_sub(r.start);
                // An empty window is not a read. `accessor::download` turns one away by
                // itself, but a span asks for a span's worth around it and would sail
                // straight past that — 8 MiB fetched to answer with nothing.
                if want == 0 {
                    return Ok(Vec::new());
                }
                let (at, bytes) = self.span(&child.id, r.start, want).await?;
                // Back to an offset into the buffer. `at <= r.start` holds for anything
                // `span` returns, so neither subtraction goes backwards.
                Ok(slice(&bytes, Some(r.start - at..r.end.saturating_sub(at))))
            }
            // A document from its own API, produced once and then held: its API has no
            // notion of a range, so the window is ours to cut out of the whole thing.
            Serves::Native(api) => {
                let bytes = self.rendered_json(&child, api).await?;
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
            let size = match (&child.serves, child.size) {
                // A document reports the length of the JSON it is served as, once
                // something has produced that JSON — and goes on reporting it after the
                // bytes are gone, because the length is remembered separately against the
                // `modifiedTime` it was measured at. This costs no request either way:
                // producing it is what a read does anyway.
                //
                // Before anything has read it, the placeholder, and there is no cheap way
                // to do better. The API answers `HEAD` with `400`, so the length cannot be
                // had without the body, and the body is the whole document. Asking for one
                // per entry is what a listing would then cost — `stat` runs once per name,
                // because FUSE-T serves over NFS and an NFS client fills an attribute for
                // every entry it lists — a second per document, forever.
                (Serves::Native(..), _) => self.remembered_len(&child).await,
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
            // client fills whatever this declines to serve, out to the length it was told
            // the file has: measured on a document whose `stat` over-reported by 152,083
            // bytes, `cat` handed back exactly the claimed 60,063,731 with the tail all
            // `0x00`. So `cp` does not rescue a wrong length — it copies the padding.
            //
            // Which is why a document fills its own tail below rather than leaving the
            // byte to the kernel. The length stays an estimate — see the revert of 1c9e67b
            // for what producing a real one was measured to cost — but the filler is now a
            // byte the format it is filling can absorb.
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            // A full window is the common case and needs nothing more.
            if n == buf.len() {
                return Ok(n);
            }
            // Short. For a blob that is the true end and the whole story, because Drive
            // sizes those exactly. For a document it is instead the read running past the
            // end of the JSON and into the span [`UNKNOWN_LENGTH_SIZE`] claimed — and
            // something is going to fill that span either way. Filling it here rather than
            // leaving the kernel to fill it with `0x00` is what makes the over-estimate
            // cost what its doc comment says it costs: JSON is defined to absorb trailing
            // whitespace, so the document still parses, where the zero padding made every
            // parser throw at the seam.
            //
            // Spaces, broken by a newline every [`PAD_LINE`] bytes. This was newlines
            // throughout, on the reasoning that empty lines keep `grep` cheap where one
            // enormous line would not. Measured, that is backwards — a line-oriented tool
            // pays per line, and a 64 MiB tail of newlines is 67 million of them:
            //
            //     tail                  jq      grep      sed      awk
            //      8 MiB  newlines      1.42s    0.49s        -        -
            //      8 MiB  spaces        0.06s    0.02s        -        -
            //     64 MiB  newlines     10.98s    3.95s    6.18s    4.59s
            //     64 MiB  spaces        0.53s    0.15s    0.03s    1.21s
            //
            // The periodic newline is what the all-spaces form gives up: it keeps the tail
            // from being one 64 MB line, which a `readline` hands over as one 64 MB string.
            // At 4 KiB it costs nothing measurable (jq 0.52s, grep 0.14s) and leaves
            // `wc -l` a number a reader can look at.
            //
            // Asked only when the window came back short, so a walk does not pay a second
            // `resolve` per window: a blob is short once, at its end, and a document only
            // past the JSON.
            let end = offset.saturating_add(n as u64);
            if end >= UNKNOWN_LENGTH_SIZE || !self.serves_json(path).await {
                return Ok(n);
            }
            let pad = ((UNKNOWN_LENGTH_SIZE - end) as usize).min(buf.len() - n);
            buf[n..n + pad].fill(b' ');
            // Placed by absolute offset, not by offset into this window, so the seam
            // between two windows neither doubles a newline nor drops one.
            let mut at = end + (PAD_LINE - 1 - end % PAD_LINE);
            while at < end + pad as u64 {
                buf[n + (at - end) as usize] = b'\n';
                at += PAD_LINE;
            }
            Ok(n + pad)
        })
    }
}

fn entry_size(c: &Child) -> u64 {
    match (c.kind.is_dir(), c.size) {
        (true, _) => 0,
        (_, Some(n)) => n,
        // A document nobody has read yet: see UNKNOWN_LENGTH_SIZE. A *blob* reaches this
        // arm only if Drive listed it without a `size`, which it does not do — measured
        // across one account's 182 non-native files, every one carried it. If that ever
        // changes the placeholder is the wrong answer for a blob, since `read_at` pads
        // only JSON and the kernel fills the rest of a binary with `0x00`; refusing the
        // file would be better than a length that corrupts it.
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
        // A native document: served as its own API's JSON. Drive's `size` is dropped
        // here rather than carried: it describes what Drive stores, which is neither the
        // JSON's length nor within an order of magnitude of it, so keeping it would only
        // give `entry_size` a wrong number to prefer over the placeholder.
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

/// The sheet a returned A1 range belongs to: `'연간 요약'!A1:Z968` -> `연간 요약`.
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

/// Give every child a unique `vfs_name`, so duplicate Drive names don't shadow each
/// other in readdir/resolve.
///
/// A name only one child holds is left alone. A name two or more hold is a collision,
/// and **every** member of that collision takes a tag from its own Drive id —
/// `report_1BxiMVs0.pdf` — rather than one keeping the bare name and the rest being
/// numbered.
///
/// The tag goes *before* the extension. Appending it (`sheet.gsheet.json_1Bxi`) would
/// keep the name unique but take the entry out of every glob a reader would use to find
/// it — measured against a real account, two of 33 spreadsheets were invisible to
/// `**/*.gsheet.json`.
///
/// Three rules, each of which costs a silently wrong file when broken.
///
/// **Collisions are counted by composition, not by bytes**, because [`same_name`]
/// resolves by composition. Drive stores whichever spelling the uploading client sent, so
/// one folder holds both — measured, ten composed names beside four decomposed. Counting
/// bytes leaves a canonically equal pair *both* untagged, and `resolve` then answers every
/// lookup with whichever came first: one file becomes unopenable and `cat` on it serves
/// the other one's contents, with no error anywhere.
///
/// **The tag comes from the file, never from its rank.** Numbering — ` (2)`, ` (3)` —
/// makes a name a function of the whole sibling set rather than of the file, so the set
/// changing renames files that did not. Ordering that numbering by id rather than by
/// arrival fixed only half of it: an id never changes, but a file's *position* among the
/// ids does. Adding a third `report.pdf` whose id sorts between two existing ones used to
/// hand `report (2).pdf` to the newcomer and push the previous holder to `report (3).pdf`.
/// Nothing errors — the path still resolves, it just opens a different document, which is
/// the exact failure this function exists to prevent. Moving a file out, or deleting one,
/// shifted everything after it the same way. A tag read off the id has none of that:
/// adding or removing a sibling leaves every other name untouched.
///
/// What no scheme can avoid is the boundary itself. A bare `report.pdf` can belong to one
/// file, so the moment a second file takes that Drive name, one of them has to give it
/// up. Tagging *both* is what keeps that to a single event — the 1-to-2 transition — with
/// nothing moving on any later change.
///
/// **Uniqueness is checked after tagging, not assumed.** [`ID_TAG_LEN`] characters
/// identify a file within a folder unless two ids in one collision end the same way,
/// which needs a shared Drive name *and* a shared tail — measured across one account's
/// 205 ids, no two shared a tail against eight pairs sharing a head. Such a group takes
/// whole ids instead, which keeps a tagged name and a number from appearing together:
/// numbering is the thing this scheme exists to remove, so it is better as a last resort
/// nothing normally reaches than as a form a reader meets beside a tag.
///
/// Underneath both, a folder that also holds a file literally named like a tagged one
/// still falls through to numbering. That is set-dependent, as the whole-id step is —
/// but leaving two entries under one name is worse than renaming one of them.
fn disambiguate(children: &mut [Child]) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in children.iter().enumerate() {
        groups
            .entry(c.vfs_name.nfc().collect())
            .or_default()
            .push(i);
    }
    for idxs in groups.into_values().filter(|g| g.len() > 1) {
        // Whole ids for the group if a shortened one would repeat inside it.
        let short: HashSet<&str> = idxs.iter().map(|&i| id_tag(&children[i].id)).collect();
        let whole = short.len() != idxs.len();
        for i in idxs {
            let tag = if whole {
                children[i].id.clone()
            } else {
                id_tag(&children[i].id).to_string()
            };
            let (stem, ext) = split_extension(&children[i].vfs_name, children[i].serves);
            let tagged = format!("{stem}_{tag}{ext}");
            children[i].vfs_name = tagged;
        }
    }

    // The net under all of it. Nothing above can leave two entries under one name unless
    // a Drive name already looked like a tagged one, and this is cheap enough to run
    // rather than reason about.
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<usize> = (0..children.len()).collect();
    order.sort_by(|&a, &b| children[a].id.cmp(&children[b].id));
    for i in order {
        if seen.insert(children[i].vfs_name.nfc().collect()) {
            continue;
        }
        let (stem, ext) = split_extension(&children[i].vfs_name, children[i].serves);
        let mut n = 2;
        let renamed = loop {
            let cand = format!("{stem} ({n}){ext}");
            if seen.insert(cand.nfc().collect::<String>()) {
                break cand;
            }
            n += 1;
        };
        children[i].vfs_name = renamed;
    }
}

/// The characters of a Drive id that a tagged name carries: the **last** ones.
///
/// Not the first, because a Drive id is not uniform along its length. The older 28-char
/// scheme front-loads a shared prefix — measured against one account, six files carried
/// `0BxO-Nrmd-kR7`, thirteen identical characters, and of seventeen such ids only ten had
/// a distinct first eight. The tail is where that scheme puts what distinguishes a file:
/// across the same account's 205 ids the last eight characters collided *no* times against
/// eight collisions for the first eight, and 28.0 bits of entropy against 17.1 within the
/// legacy family.
///
/// Byte-slicing is safe here: a Drive id is `[A-Za-z0-9_-]`, so every character is one
/// byte and none of them needs sanitizing.
fn id_tag(id: &str) -> &str {
    &id[id.len().saturating_sub(ID_TAG_LEN)..]
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
///
/// `existing` holds composed names and is asked in the composed form, for the reason
/// [`disambiguate`] gives: a set that compares bytes leaves a canonically equal pair both
/// unnumbered, and [`same_name`] then answers every lookup with whichever came first.
fn unique_name(name: &str, existing: &HashSet<String>) -> String {
    let taken = |n: &str| existing.contains(&n.nfc().collect::<String>());
    if !taken(name) {
        return name.to_string();
    }
    let mut candidate = format!("{name} [Shared Drive]");
    let mut suffix = 2;
    while taken(&candidate) {
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
///
/// Nothing else guards it. An earlier version skipped the composition when exactly one
/// side was ASCII, on the reasoning that a pair spanning that boundary cannot compose to
/// the same thing. It can: `NFC("\u{212A}")` — the Kelvin sign — is `"K"`, and
/// `NFC("\u{037E}")` — the Greek question mark — is `";"`. A file listed as
/// `2\u{212A} readings.txt` answered `ENOENT` to the spelling a reader would type, which
/// is the failure this function exists to prevent.
fn same_name(a: &str, b: &str) -> bool {
    a == b || a.nfc().eq(b.nfc())
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
/// Both ends are clamped, because a reader may seek past the end and a window may
/// straddle it. Neither end is reordered: `read_window` is the only caller and its own
/// only caller is [`FileSystem::read_at`], which builds `offset..offset + buf.len()`, so
/// `r.end >= r.start` holds by construction. Guarding it here as well was defence against
/// this crate rather than against a client — `read_window` is private and there is no
/// other way in.
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
#[path = "gdrive_tests.rs"]
mod tests;
