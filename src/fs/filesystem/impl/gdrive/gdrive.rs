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
    fs::{Dirent, DirentKind, FileSystem, Stat, filesystem::posix::NAME_MAX},
};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// The mount root mirrors Drive's sidebar as virtual sections, under Drive's own English
/// labels. `My Drive` is the literal folder id `root` and a shared drive has a drive id of
/// its own, but `Shared with me` has no id: Drive holds no folder for it, and what it
/// gathers carries no `parents`, so the folder tree alone never surfaces any of it.
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

/// Per-directory listing TTL, and the life of a span held beside it.
///
/// Nothing above this store caches for it, so this bounds how stale the tree can be.
/// One number for listings and spans, so `ls` and a read of the same file answer from
/// the same snapshot.
const DIR_TTL: Duration = Duration::from_secs(300);
/// Ceiling on the cell values one spreadsheet's JSON will carry, spent tab by tab
/// in the workbook's own order until it runs out.
///
/// Values are proportional to what is actually filled in, so this bounds the outlier
/// rather than the common case.
const GRID_BYTES_BUDGET: u64 = 8 * 1024 * 1024;
/// Tabs whose values are requested in one `batchGet`. Each title rides in the
/// query string, so an unbounded count would eventually build an unsendable URL.
const MAX_TABS: usize = 64;

/// Size reported for a document whose length nobody has learned yet.
///
/// A placeholder, not a measurement: `ls -l` and `find -size` see it until something reads
/// the file, which from then on reports its exact length.
///
/// It cannot be 0: a client bounds a read by the length it was told, so a file reported
/// empty hands back nothing however many bytes are behind it, and a search tool skips it.
///
/// An over-estimate means the client asks for and takes back the whole claimed span, so
/// [`FileSystem::read_at`] fills the part past the JSON; zero fill there would make every
/// JSON parser throw at the seam.
///
/// An *under*-estimate has no recovery: the reader stops where it was told, every window
/// full, so nothing reports a short read and the JSON ends mid-token. Equal to
/// [`MAX_DOCUMENT_BYTES`] so that cannot happen: `get_pretty` bounds the *raw* body at that
/// number and Google already returns pretty JSON of about the re-serialized length, so
/// served bytes cannot exceed it, and a larger document fails loudly in `body_within`
/// instead.
///
/// An exact length up front costs one render per document, and `stat` runs once per name,
/// because FUSE-T serves over NFS and an NFS client fills an attribute for every entry it
/// lists.
const UNKNOWN_LENGTH_SIZE: u64 = MAX_DOCUMENT_BYTES;

/// How long a line the padding past a document's JSON is broken into.
///
/// The padding is whitespace JSON ignores, so this only shapes the tail for line-oriented
/// tools (see the padding step of this store's `read_at`).
const PAD_LINE: u64 = 4096;

/// How much a blob read fetches once it is clear the reader is walking the file, so a
/// walk pays a round trip per span of this size rather than one per window.
///
/// The kernel's window is 64 KiB and not ours to choose, and FUSE-T's NFS backend halves
/// it to 32 KiB; one ranged request per window makes a large file cost a request, and its
/// quota, per window.
///
/// A ranged request costs a round trip of about the same length whatever its size, then
/// bytes, so bigger spans pay off until the transfer itself dominates — around this size,
/// past which only the request count falls.
///
/// The *first* fetch takes [`FIRST_SPAN`] instead, so a reader that stops after a buffer
/// never pays for a span it will not use. And a walk gets this only as a lone reader: its
/// span is its share of [`HELD_BUDGET`], divided among the files being read at once.
const READ_SPAN: u64 = 64 * 1024 * 1024;

/// What the first fetch of a file takes, before anything says the reader is walking it.
///
/// It cannot be the window the kernel asked for, however little that reader wants. The
/// mount is served over NFS, whose client fires around a megabyte of read-ahead the
/// moment a file is touched, and every window of it continues the one before — which is
/// a walk by any test this layer can apply, so a head-read would fetch [`READ_SPAN`] for
/// read-ahead nobody reads.
///
/// This swallows that read-ahead whole, so a head-read is one fetch and stops there. A
/// reader that really is walking spends this span and gets [`READ_SPAN`] for the next one.
const FIRST_SPAN: u64 = 8 * 1024 * 1024;

/// Ceiling on the bytes held across every file at once: one [`READ_SPAN`], so concurrent
/// readers divide what a lone walk holds. See [`GdriveFs::held`].
const HELD_BUDGET: u64 = READ_SPAN;

/// Floor on one file's share of [`HELD_BUDGET`].
///
/// For the case the count cannot tell apart: many files touched once beside one being
/// walked. [`ACTIVE`] keeps a file counted for seconds after its single read, so a
/// traversal past many small files would otherwise shrink the walked file's span to
/// almost nothing and multiply its requests.
///
/// It costs traffic in the opposite case, where the files really are all being walked.
/// Past `HELD_BUDGET / MIN_SPAN` files the shares stop fitting and eviction comes back.
const MIN_SPAN: u64 = 4 * 1024 * 1024;

/// How recently a span must have been read from for its file to count as one of the readers
/// dividing [`HELD_BUDGET`].
///
/// Being in the map is the wrong test: an entry lives for [`DIR_TTL`], so a traversal past
/// many files leaves them all behind and would divide the budget among them while one file
/// is walked. Seconds rather than milliseconds because a miss against Drive costs about that
/// (see [`READ_SPAN`]), so two files alternating touch each other's spans a second or more
/// apart; what matters is the interval between *misses* on one file, since a hit refreshes
/// [`HeldSpan::used`].
const ACTIVE: Duration = Duration::from_secs(3);

/// Cap on remembered document lengths — one per Docs-editors file this mount has read.
///
/// Cleared wholesale rather than evicted one at a time: losing an entry costs a listing's
/// accuracy and never correctness, since the next read puts it back and a read produces
/// the JSON either way.
const MAX_REMEMBERED_LENGTHS: usize = 50_000;

/// Safety ceiling on one folder's listing. Beyond this the listing truncates (the
/// accessor logs it) — a folder that large is pathological to `ls` anyway.
const MAX_FOLDER_FILES: usize = 10_000;

/// How many characters of a Drive id every served name carries (see [`id_tag`] and
/// [`disambiguate`]).
const ID_TAG_LEN: usize = 8;

/// The longest name this store will hand out, measured the way the mount measures it.
///
/// [`NAME_MAX`] is what `statfs` reports, and nothing below enforces it. The reason to
/// keep to it anyway is what happens on the way *out*: `cp`, `tar` and `rsync` write to
/// filesystems that do enforce it, and a name over it fails there rather than here.
///
/// Counted decomposed, because that is the form the mount emits and it is not the form
/// Drive stores. Decomposed Korean takes two to three times the bytes, so a name that fits
/// comfortably as Drive holds it can be too long once served.
const NAME_BUDGET: usize = NAME_MAX as usize;

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
    /// Listing name: the sanitized Drive name with a tag off this entry's own id (see
    /// [`disambiguate`]) — plus a `.json` suffix when the entry serves a document's API
    /// JSON. Untagged at the root, whose sections this store names itself.
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
    /// served nor anything else a reader sees. It is kept for the ceiling check and nothing
    /// else; the length an entry reports comes from `entry_size`.
    size: Option<u64>,
}

/// One folder's children as the cache holds them: shared, so a `stat` or a read
/// borrows the listing instead of copying it, which matters for a large folder.
type CachedListing = (Instant, Arc<Vec<Child>>);

/// A document's served length beside the `modifiedTime` it was measured at.
type RememberedLength = (Option<std::time::SystemTime>, u64);

/// One span of one file, and where in it the span begins. Keyed by Drive id in
/// [`GdriveFs::held`], which is why the id is not a field here.
struct HeldSpan {
    /// Where in the file the span begins. A read cuts by absolute offset, so this is
    /// needed to answer one.
    at: u64,
    /// Drive answered shorter than the span asked for, which means the span runs to the
    /// end of the file. Without this a read near the end misses every time — the span
    /// does not reach the offset asked for, and never will. A document is always this: its
    /// API has no ranges, so what comes back is the whole of it.
    to_eof: bool,
    /// When the bytes were fetched. Decides the TTL, so a read does not touch it.
    when: Instant,
    /// When a read last came out of these bytes. The share count needs this rather than
    /// `when`, which a hit does not touch, so by `when` a file still being walked looks
    /// abandoned after [`ACTIVE`] and its span shrinks. Eviction orders by it too, where the
    /// two measure the same.
    used: Instant,
    bytes: Arc<Vec<u8>>,
}

pub struct GdriveFs {
    accessor: GdriveAccessor,
    /// A Docs-editors file's served length (file id → stamped length), learned when its
    /// JSON was produced and kept past the bytes themselves. A document has no `size` to
    /// list and `HEAD` answers 400, so its length exists only once something renders it.
    /// See [`Self::remembered_len`].
    lengths: Mutex<HashMap<String, RememberedLength>>,
    /// Per-directory listing cache (folder path → children). Path resolution walks
    /// parent listings, so one cached listing answers readdir, stat and the lookup
    /// a read starts with; fetching the bytes themselves still costs a request.
    dir_cache: Mutex<HashMap<String, CachedListing>>,
    /// Drive id → the span last read from that file, bounded in bytes by [`HELD_BUDGET`].
    ///
    /// A document lands here too, as a whole-file span at offset 0. Its API has no ranges,
    /// so a read of any window produces all of it, and the next window finds it here or
    /// renders the whole document again.
    ///
    /// A map rather than one slot, because reads of two files interleave with neither
    /// threads nor a second process: FUSE ops are serialized, so alternating is enough.
    /// With one slot each read finds the other file there, no read ever counts as a walk,
    /// and every window pays a whole [`FIRST_SPAN`] and a request's quota. A lost document
    /// is worse than a lost span, costing a re-render rather than a re-fetch.
    ///
    /// A single tool rarely interleaves on this store: read-ahead overlaps files only when
    /// it can outrun the reader, and a miss of about a second never lets it. So traces
    /// replayed from a fast local store do not describe this one.
    ///
    /// Bounded by *bytes* rather than count, so the ceiling is divided rather than owned by
    /// whoever fetched last, and each span is sized to that division so nothing has to be
    /// evicted to fit. See [`READ_SPAN`] and [`ACTIVE`].
    held: Mutex<HashMap<String, HeldSpan>>,
}

impl GdriveFs {
    pub fn new(config: &GdriveConfig) -> anyhow::Result<Self> {
        Ok(Self {
            accessor: GdriveAccessor::new(config)?,
            lengths: Mutex::new(HashMap::new()),
            dir_cache: Mutex::new(HashMap::new()),
            held: Mutex::new(HashMap::new()),
        })
    }

    /// The span covering `want` bytes from `start`, from the map when one reaches and
    /// from Drive otherwise. The span's own start comes back beside its bytes, because
    /// the caller cuts by an offset into the file rather than into the buffer.
    ///
    /// Spans begin where a reader asks rather than on fixed boundaries. A sequential
    /// walk then lands inside the held span until it is spent and starts the next one
    /// exactly where it left off, so no window is ever split across two spans and no
    /// read comes back short of what it asked for — which [`FileSystem::read_at`] would
    /// report to the kernel as the end of the file.
    ///
    /// How much a miss fetches depends on what the last one did and on how many other files
    /// are being read. A read that carries on from where the held span ended is a reader
    /// walking the file, and gets its share of [`HELD_BUDGET`] — a whole [`READ_SPAN`] when
    /// it is the only reader, half that at two. Anything else — a different file, a jump to
    /// somewhere new — gets that share capped at [`FIRST_SPAN`], so a reader that stops
    /// after a buffer pays for a first span it mostly throws away rather than a walk's worth.
    ///
    /// A span is never smaller than the window asked for, so an oversized read is
    /// answered whole rather than truncated.
    async fn span(&self, id: &str, start: u64, want: u64) -> io::Result<(u64, Arc<Vec<u8>>)> {
        let (walking, sharers) = {
            let mut held = self.held.lock().await;
            // Every *other* file read within [`ACTIVE`], plus this one, which is about to
            // hold a span whether or not it has an entry. Counting `id`'s own entry would
            // miss this reader once its span went stale, and a lone active neighbour would
            // then take the whole budget and evict the span the division exists to keep.
            let sharers = held
                .iter()
                .filter(|(k, h)| k.as_str() != id && h.used.elapsed() < ACTIVE)
                .count() as u64
                + 1;
            let walking = match held.get_mut(id) {
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
        // Sized so everything being read fits at once, rather than a constant. A constant
        // equal to the budget is the one value that cannot work: two files each wanting
        // all of it means one is always evicted, whatever the order.
        let share = (HELD_BUDGET / sharers).max(MIN_SPAN);
        let len = want.max(if walking {
            share
        } else {
            share.min(FIRST_SPAN)
        });
        let bytes = self
            .accessor
            .download(id, Some(start..start.saturating_add(len)))
            .await
            .map_err(not_found_or_backend)?;
        let to_eof = (bytes.len() as u64) < len;
        let bytes = Arc::new(bytes);
        let now = Instant::now();
        self.hold(
            id,
            HeldSpan {
                at: start,
                to_eof,
                when: now,
                used: now,
                bytes: bytes.clone(),
            },
        )
        .await;
        Ok((start, bytes))
    }

    /// Keep one span, dropping what has aged out and then the least recently read from
    /// until [`HELD_BUDGET`] has room.
    ///
    /// Dividing the budget keeps this loop from running at all in the ordinary case, so
    /// the order matters only past the point where a share hits [`MIN_SPAN`], where
    /// evicting the least recently read costs less traffic than evicting arbitrarily.
    ///
    /// The new span goes in even when it alone is over budget: it has already been paid
    /// for and the caller is about to read from it, so refusing to hold it would only cost
    /// the next window another fetch — or, for a document, another render.
    async fn hold(&self, id: &str, span: HeldSpan) {
        let len = span.bytes.len() as u64;
        let mut held = self.held.lock().await;
        held.remove(id);
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
        held.insert(id.to_string(), span);
    }

    /// A document's JSON, from the map when it is held there and from its
    /// own API otherwise.
    ///
    /// Held as a whole-file span at offset 0 with `to_eof`, in the same map as blob spans:
    /// the API answers with the whole document or nothing, so without holding it a document
    /// read a window at a time is one render per window.
    async fn rendered_json(&self, child: &Child, api: NativeApi) -> io::Result<Arc<Vec<u8>>> {
        let id = child.id.as_str();
        {
            let mut held = self.held.lock().await;
            if let Some(h) = held.get_mut(id)
                && h.at == 0
                && h.to_eof
                && h.when.elapsed() < DIR_TTL
            {
                // Read from, so this is the one to keep when room runs short — and losing
                // a document costs a render rather than a range request.
                h.used = Instant::now();
                return Ok(h.bytes.clone());
            }
        }
        let bytes = match api {
            NativeApi::Sheet => self.spreadsheet_bytes(id).await,
            NativeApi::Doc => self.accessor.document_json(id).await,
            NativeApi::Slides => self.accessor.presentation_json(id).await,
        }
        .map_err(not_found_or_backend)?;
        let bytes = Arc::new(bytes);
        let now = Instant::now();
        self.hold(
            id,
            HeldSpan {
                at: 0,
                to_eof: true,
                when: now,
                used: now,
                bytes: bytes.clone(),
            },
        )
        .await;
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
        if let Ok(mut drives) = listed {
            // Sorted here: `drives.list` accepts `orderBy` and discards it, and
            // documents no order. Unsorted, two `ls /` could disagree, and `unique_name`
            // gives the plain name to whichever same-named drive it reaches first, so the
            // suffix would move between listings — a shared drive has no id tag to fall
            // back on. Not by `modifiedTime`: a shared drive has none, and asking for one
            // is a `400 Invalid field selection`. By name for a reader, then by id to make
            // it total: same-named drives, even created in the same second, differ only
            // by id.
            drives.sort_by(|a, b| {
                let key = |d: &Value| {
                    let name: String = d
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .nfc()
                        .collect();
                    let id = d
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    (name, id)
                };
                key(a).cmp(&key(b))
            });
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

        // Drive lets one folder hold two files of a name and a directory cannot, so every
        // entry takes a tag off its own id: readdir shows distinct names, resolve finds each
        // one, and no entry's name depends on what else the folder holds.
        //
        // Not at the root. Those three sections are names this store invents rather than
        // names Drive gave, `unique_name` already keeps them apart, and `Shared with me`
        // answers to no id at all — tagging it would leave a trailing `_` standing in for
        // an id that does not exist.
        if folder != "/" {
            disambiguate(&mut children);
        }

        let children = Arc::new(children);
        if complete {
            let mut cache = self.dir_cache.lock().await;
            // Drop what has aged out before adding: nothing else removes an entry, so one
            // listing per folder ever visited would stay for the life of the mount.
            cache.retain(|_, (at, _)| at.elapsed() < DIR_TTL);
            cache.insert(folder.to_string(), (Instant::now(), children.clone()));
        }
        Ok(children)
    }

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
    /// `spreadsheets.get` gives the workbook's shape and, with it, the tab titles that
    /// name the ranges; `values:batchGet` then returns the used range of every tab at once.
    /// `includeGridData=true` is no route: it bills per *allocated* cell. See
    /// [`GdriveAccessor::sheet_values_batch`].
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
                // Not logged: the reason goes into the workbook, the copy the reader
                // actually meets.
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("valuesUnavailable".into(), Value::String(format!("{e:#}")));
                }
                return pretty(&v);
            }
        };
        fold_values(&mut v, &batch, &titles);
        pretty(&v)
    }

    /// The length `stat` reports for a document, kept so it does not fall back to the
    /// placeholder.
    ///
    /// Held apart from the bytes: the JSON is large and expires on [`DIR_TTL`] under a
    /// byte budget, while the length is tiny with no reason to expire, so an unchanged
    /// document read minutes ago does not flip back to the placeholder in `ls -l`.
    ///
    /// Stamped with `modifiedTime` rather than aged by a clock: an unchanged document keeps
    /// its length indefinitely, and a changed one loses it the moment a listing says so,
    /// where a TTL would discard lengths still right and serve ones already wrong.
    ///
    /// Keyed by id with the stamp in the value, so an edit replaces the row instead of
    /// leaving an unreadable row per version. Losing an entry costs a listing's accuracy,
    /// never correctness, so the bound is a flat cap and a clear.
    async fn remembered_len(&self, child: &Child) -> Option<u64> {
        // The bytes first, while they are still held: `stat` and a read of the same
        // document answer from the same place or they disagree about where it ends.
        let in_hand = {
            let held = self.held.lock().await;
            held.get(child.id.as_str())
                .filter(|h| h.at == 0 && h.to_eof)
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
    /// A row with no `modifiedTime` is not remembered: stamped `None`, it would match
    /// `None` forever with no TTL to retire it, and once the document grew the length
    /// would be short — the one error `read_at` cannot pad around, since the reader stops
    /// where it was told. The placeholder is the honest answer for a length nothing can
    /// date.
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
    /// outside.
    #[cfg(test)]
    pub(crate) async fn listings_retained(&self) -> usize {
        self.dir_cache.lock().await.len()
    }

    /// How many bytes are held for one Drive id, `None` when nothing is. Tests only. Keyed
    /// by id because a span's kind cannot be read off it (a small blob read from 0 is also
    /// `at: 0, to_eof`).
    #[cfg(test)]
    pub(crate) async fn held_bytes(&self, id: &str) -> Option<u64> {
        self.held.lock().await.get(id).map(|h| h.bytes.len() as u64)
    }

    /// Drop the produced bytes while keeping what was learned from them. Tests only —
    /// it is the state a document reaches on its own, by the byte budget or the TTL.
    #[cfg(test)]
    pub(crate) async fn forget_rendered_for_test(&self) {
        self.held.lock().await.clear();
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
            // does not already cover it. A small file is simply one span that came back
            // short, so size needs no case of its own.
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
                // `read_at` never builds a backwards range, but its `offset + buf.len()`
                // saturates, so an offset at the top of the range gives an empty window.
                // Saturating here lands any degenerate range in the zero check below,
                // where plain `-` would wrap in release and ask Drive for the rest of the
                // file to answer with nothing.
                let want = r.end.saturating_sub(r.start);
                // An empty window is not a read: `span` would widen it and fetch a whole
                // span to answer with nothing.
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
                // A document reports its served JSON length once something has produced
                // it, remembered past the bytes against `modifiedTime`; no request either
                // way. Before that, the placeholder: `HEAD` answers `400`, so the length
                // needs the whole body, and `stat` runs once per listed name (FUSE-T
                // serves over NFS), so an exact length would cost a render per document
                // per listing.
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
            // Short is EOF and nothing else, because a span never splits a window: `span`
            // fetches from where the read begins and at least as far as it asks, so what
            // comes back either covers the window or ran out of file.
            //
            // A short answer does not reach a caller as one: through a mount the client
            // fills the rest out to the length `stat` claimed with `0x00`, and `cp` copies
            // that padding. So a document fills its own tail below, with a byte JSON can
            // absorb.
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            // A full window is the common case and needs nothing more.
            if n == buf.len() {
                return Ok(n);
            }
            // Short. For a blob that is the true end, because Drive sizes blobs exactly.
            // For a document it is the read running past the JSON into the span
            // [`UNKNOWN_LENGTH_SIZE`] claimed, which something fills either way: whitespace
            // here keeps the document parseable, where the kernel's `0x00` makes every
            // parser throw at the seam.
            //
            // Spaces, broken by a newline every [`PAD_LINE`] bytes. Not all newlines: a
            // line-oriented tool pays per line, so a tail of newlines makes it slow where
            // spaces do not. The periodic newline keeps the tail from being one huge line,
            // which a `readline` would hand over as one huge string; it costs nothing
            // noticeable and leaves `wc -l` a number a reader can look at.
            //
            // Asked only when the window came back short, so a walk does not pay a second
            // `resolve` per window.
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
        // A document nobody has read yet (see UNKNOWN_LENGTH_SIZE). A *blob* lands here
        // only if Drive listed it without a `size`, which it does not do. For a blob the
        // placeholder would be wrong, since `read_at` pads only JSON and the kernel
        // zero-fills the rest of a binary.
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
/// A `Stat` carries no provenance for its length, so an unread document's placeholder
/// reads as a size like any other.
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
/// everything else becomes `Other` carrying the full error chain as its message.
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
/// and an oversized one does not consume the remainder, so a small tab after it still
/// fits.
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
/// line, well past the compact form — so a budget checked against compact bytes would
/// admit a file much larger than it allows.
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

/// What this name costs the mount, which serves decomposed.
fn served_name_len(name: &str) -> usize {
    name.nfd().map(char::len_utf8).sum()
}

/// `name` with `tag` in front of its extension, cut to [`NAME_BUDGET`] if it has to be.
///
/// The stem is what gives. The tag is what makes the name unique and the extension is what
/// keeps it inside a glob, so neither can be the part that goes — and a cut from the right
/// would take the tag first, which is exactly the part that tells two entries apart.
/// Characters come off one at a time rather than by a byte count, because a byte slice can
/// land inside one.
fn shorten_for_tag(name: &str, serves: Serves, tag: &str) -> String {
    let (stem, ext) = split_extension(name, serves);
    let fixed = served_name_len(ext) + 1 + tag.len();
    let mut stem: String = stem.to_string();
    while !stem.is_empty() && served_name_len(&stem) + fixed > NAME_BUDGET {
        stem.pop();
    }
    format!("{stem}_{tag}{ext}")
}

/// Give every child a `vfs_name` that names the file and nothing else about the folder.
///
/// **Every entry carries a tag off its own Drive id** ([`id_tag`]) — `report_a1b2c3d4.pdf`
/// — whether or not anything else in the folder shares its name, so no arrival, departure,
/// rename or move renames anybody else, and a path an agent carries from listing to
/// reading keeps resolving. For the same reason the tag comes from the file, never from its
/// rank: numbering (` (2)`, ` (3)`) makes a name a function of the sibling set, and even
/// ordered by id a file's *position* among the ids changes.
///
/// The tag goes in front of the extension, because appending it (`sheet.gsheet.json_1Bxi`)
/// takes the entry out of every glob a reader would use, such as `**/*.gsheet.json`.
///
/// **Grouped by composition, not by bytes**, because [`same_name`] resolves by composition
/// and Drive stores whichever spelling the uploader sent, both within one folder. Two
/// spellings of one name are one collision.
///
/// A group whose ids also end alike takes whole ids, which keeps a tag and a number off
/// the same name. Numbering remains only as a net for a Drive name already shaped like a
/// tagged one.
fn disambiguate(children: &mut [Child]) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in children.iter().enumerate() {
        groups
            .entry(c.vfs_name.nfc().collect())
            .or_default()
            .push(i);
    }
    for idxs in groups.into_values() {
        // Whole ids for the group if a shortened one would repeat inside it.
        let short: HashSet<&str> = idxs.iter().map(|&i| id_tag(&children[i].id)).collect();
        let whole = short.len() != idxs.len();
        for i in idxs {
            let tag = if whole {
                children[i].id.clone()
            } else {
                id_tag(&children[i].id).to_string()
            };
            // Cut with the tag in hand: a group promoted to whole ids needs more room than
            // the short form.
            let tagged = shorten_for_tag(&children[i].vfs_name, children[i].serves, &tag);
            children[i].vfs_name = tagged;
        }
    }

    // The net: only a Drive name already shaped like a tagged one can collide here, and
    // this is cheap enough to run rather than reason about.
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
/// Not the first, because a Drive id is not uniform along its length: the older id scheme
/// front-loads a prefix shared across many files, so the tail varies far more than the
/// head.
///
/// Byte-slicing is safe here: a Drive id is `[A-Za-z0-9_-]`, so every character is one
/// byte and none of them needs sanitizing.
fn id_tag(id: &str) -> &str {
    &id[id.len().saturating_sub(ID_TAG_LEN)..]
}

/// Split a listing name into the part a tag can follow and the extension that tag must stay
/// in front of.
///
/// A document's suffix is known exactly (`.gsheet.json`, not `.json`). A file keeps
/// whatever follows its last dot when that looks like an extension. A directory has
/// no extension to protect, so `v1.2` is served as `v1.2_1a2b3c4d`.
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
/// `existing` holds composed names and is asked in composed form: a byte-compared set
/// would leave a canonically equal pair both unnumbered, and [`same_name`] would then
/// answer every lookup with whichever came first.
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

/// Whether two names are the same name, in the sense a directory has to mean it.
///
/// One name has two Unicode spellings — `한` is one code point composed or three jamo
/// decomposed, and Japanese voiced marks likewise — and both arrive here: macOS hands a
/// lookup the *decomposed* form of whatever a listing returned, and Drive stores whichever
/// form the uploading client sent, both within one folder. A byte comparison would answer
/// `ENOENT` for a name `ls` just printed, for whichever files happened to be uploaded
/// composed.
///
/// Bytes first, which settles every ASCII name and most others; composition runs only when
/// that fails. There is no ASCII shortcut: `NFC("\u{212A}")` (Kelvin sign) is `"K"` and
/// `NFC("\u{037E}")` (Greek question mark) is `";"`, so `2\u{212A} readings.txt` must
/// match the spelling a reader would type.
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

/// Split a path into `(parent_dir, last_segment)`. `/a/b` -> (`/a`, `b`);
/// `/a` -> (`/`, `a`).
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
/// straddle it. Neither is reordered: the only way in is the private `read_window` from
/// [`FileSystem::read_at`], which builds `offset..offset + buf.len()`, so
/// `r.end >= r.start` holds by construction.
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
