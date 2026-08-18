//! [`MessengerFs`] — a messenger's conversations as a filesystem, synthesized along the
//! time axis.
//!
//! Written once and generic over [`MessengerSource`], which is the lane's whole design. A
//! messenger has no hierarchy to mirror, so every platform's tree has to be *invented*; four
//! adapters each inventing one is four layouts a document claims are the same.
//!
//! ```text
//! channels/<name>__<id>/
//!   2026-08-10/               a day the conversation HAS — never a calendar
//!     chat.jsonl              one line per message
//!     threads/<root-id>/      a thread, same shape as a day
//!       chat.jsonl
//!       files/
//!     files/<name>__<id>      attachments posted that day
//! dms/…                       only when the source can enumerate them
//! users/<name>__<id>.json
//! ```
//!
//! # Why the date directories are what the walk saw
//!
//! No messenger has an endpoint for "which days does this conversation have". The tempting
//! answer — every day between its creation and now — invents a directory for every silent
//! day, which on a long quiet channel is nearly all of them. The reader here is an agent,
//! which cannot tell a quiet day from a failed request and pays a round trip to find out.
//!
//! So [`dates`](MessengerFs::dates) walks the conversation's newest messages backwards
//! for a bounded number of pages and lists only the days it saw. Listing and reachability are
//! then different questions, which is what keeps old history readable without presenting an
//! empty directory:
//!
//! * a listed day is served;
//! * a day *inside* the walked range that went unlisted is refused, at no request — the walk
//!   already proved it empty;
//! * a day below a truncated walk's floor is fetched, and served only if it has messages.
//!
//! # One request fills three things
//!
//! Entering a day is one [`history`](MessengerSource::history) call, and its answer is
//! `chat.jsonl`'s bytes, the `threads/` listing (roots with replies) and the `files/` listing
//! together — so listing a day usually leaves reading it free. That is [`Day`], and it is
//! cached whole.
//!
//! # A known limit
//!
//! A thread lives under its *root's* day, so replies written later do not appear under the
//! day they were written. Not fixable from here: a history window selects on a message's own
//! timestamp and replies are not in it, and scanning every day for late replies is the cost
//! the bounded walk exists to avoid.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use super::source::{ConvId, ConvKind, Conversation, FileRef, MessengerSource, MsgId, Window};
use super::{User, render_line};
use crate::BoxFuture;
use crate::fs::{Dirent, DirentKind, FileSystem, Stat};

/// The three sections at the mount root.
const CHANNELS: &str = "channels";
const DMS: &str = "dms";
const USERS: &str = "users";

/// The file a day's (or a thread's) messages are served as.
const CHAT: &str = "chat.jsonl";
/// The directories inside a day.
const THREADS: &str = "threads";
const FILES: &str = "files";

/// Pages of history one [`dates`](MessengerFs::dates) walk reads before giving up.
///
/// A budget rather than a backstop: this is the one call whose cost the tree chooses, and it
/// buys depth of history with requests. What it cannot buy is completeness — a conversation
/// older than the walk keeps history the listing does not name, which is why an unlisted day
/// below the floor is fetched rather than refused.
const SCAN_PAGES: usize = 10;

/// The most bytes one open attachment may hold, and so both of the things that number
/// decides: below it an attachment is fetched whole at `open`, above it a window at a time.
///
/// One constant for both because it is really one rule — a ceiling on what a handle costs in
/// memory. A mount serving several readers holds one of these per open file, so an attachment
/// nobody bounded is a mount a single 2 GB upload can stop.
///
/// Measurement sets a floor and not the value: a reader asks in 128 KiB pieces at most (32 KiB
/// through FUSE-T, `examples/measure_read_sizes.rs`), so a window orders smaller than this
/// spends the round trip it saved on the very next read. Anything from a few MiB up satisfies
/// that.
///
/// What picks the value is a trade this lane has rather than an object store: an attachment is
/// a document somebody posted in chat, and the *only* shape whose length can be checked
/// against the listing is the whole one — the check that catches a refused token answering 200
/// with a login page. So raising this verifies more of the real population, and costs that
/// much memory per open file; lowering it drops screenshots and reports into the unverified
/// path to save memory a mount is unlikely to be short of.
///
/// 8 MiB is a judgement, not a finding. It sits above where documents, images and spreadsheets
/// land and below recordings, so the files an agent actually greps are verified and a video
/// cannot wedge a mount. The thing that would refine it is the size distribution of attachments
/// in real workspaces, which this crate has one sample of.
const WINDOW: u64 = 8 << 20;

/// How long anything fetched stays reusable.
///
/// One number for every kind, because they are all the same trade: a `stat` followed by an
/// `open` is two calls into this volume for one thing a reader is doing, and without a window
/// in which the second is free every `ls -l` pays for the whole listing twice.
const TTL: Duration = Duration::from_secs(15);

/// A value and when it stops being reusable.
struct Cached<T> {
    value: T,
    at: Instant,
}

impl<T> Cached<T> {
    fn get(entry: Option<&Cached<T>>) -> Option<&T> {
        entry.filter(|c| c.at.elapsed() < TTL).map(|c| &c.value)
    }

    fn new(value: T) -> Self {
        Cached {
            value,
            at: Instant::now(),
        }
    }
}

/// What one [`history`](MessengerSource::history) call produced, kept whole.
///
/// The three fields arrive together and are read separately, which is the reason this is one
/// cache entry and not three: `ls` of a day, `ls` of its `threads/`, `ls` of its `files/` and
/// `cat chat.jsonl` are four operations over one request.
struct Day {
    /// `chat.jsonl`'s bytes, already rendered.
    chat: Arc<Vec<u8>>,
    /// Roots that have replies — the `threads/` listing.
    threads: Vec<MsgId>,
    /// Attachments posted that day.
    files: Vec<FileRef>,
}

#[derive(Default)]
struct Cache {
    convs: Option<Cached<Vec<Conversation>>>,
    users: Option<Cached<Vec<User>>>,
    /// The days each conversation was seen to have, and whether the walk was truncated —
    /// which is what tells an unlisted day apart from one below the floor.
    dates: HashMap<ConvId, Cached<(Vec<NaiveDate>, bool)>>,
    days: HashMap<(ConvId, NaiveDate), Cached<Arc<Day>>>,
    threads: HashMap<(ConvId, MsgId), Cached<Arc<Day>>>,
    /// The one attachment window held, as `(path, start, bytes)`.
    ///
    /// One and not a map, which is what makes [`WINDOW`] a ceiling on the whole store rather
    /// than on each open file: a reader walking a document forwards keeps replacing it, and two
    /// readers alternating between large attachments replace each other's — slower, bounded, and
    /// rare, where a map would be unbounded because nothing here is told when a file is closed.
    window: Option<(PathBuf, u64, Vec<u8>)>,
}

/// A messenger as a read-only filesystem. See the module docs.
pub struct MessengerFs<S> {
    source: Arc<S>,
    /// The timestamp every synthesized directory reports.
    ///
    /// Fixed at construction for the reason [`Workspace`](crate::fs::Workspace)'s is: a
    /// directory advertising `now()` on each `stat` looks perpetually modified, and a guest
    /// that negotiated `AUTO_INVAL_DATA` watches that field to decide when to drop cached
    /// pages.
    born: SystemTime,
    cache: Mutex<Cache>,
}

impl<S: MessengerSource> MessengerFs<S> {
    pub fn new(source: S) -> Self {
        MessengerFs {
            source: Arc::new(source),
            born: SystemTime::now(),
            cache: Mutex::new(Cache::default()),
        }
    }

    /// A directory this store made up rather than read.
    fn dir(&self) -> Stat {
        let mut s = Stat::new(DirentKind::Dir, 0);
        s.mtime = Some(self.born);
        s
    }

    /// A file of `size`, timestamped like every synthesized entry.
    fn file(&self, size: u64) -> Stat {
        let mut s = Stat::new(DirentKind::File, size);
        s.mtime = Some(self.born);
        s
    }

    // ---- fetches, each answering from the cache first ---------------------------------

    async fn conversations(&self) -> io::Result<Vec<Conversation>> {
        if let Some(v) = Cached::get(self.cache.lock().unwrap().convs.as_ref()) {
            return Ok(v.clone());
        }
        let (convs, _truncated) = self.source.conversations().await?;
        self.cache.lock().unwrap().convs = Some(Cached::new(convs.clone()));
        Ok(convs)
    }

    async fn users(&self) -> io::Result<Vec<User>> {
        if let Some(v) = Cached::get(self.cache.lock().unwrap().users.as_ref()) {
            return Ok(v.clone());
        }
        let (users, _truncated) = self.source.users().await?;
        self.cache.lock().unwrap().users = Some(Cached::new(users.clone()));
        Ok(users)
    }

    /// The days `conv` was seen to have, and whether the walk stopped short of its history.
    async fn dates(&self, conv: &ConvId) -> io::Result<(Vec<NaiveDate>, bool)> {
        if let Some(v) = Cached::get(self.cache.lock().unwrap().dates.get(conv)) {
            return Ok(v.clone());
        }
        // One unreadable conversation is not a broken tree: it is listed, and it is empty.
        let (msgs, truncated) = match self.source.scan(conv, SCAN_PAGES).await {
            Ok(v) => v,
            Err(e) if e.is_conversation_denied() => (Vec::new(), false),
            Err(e) => return Err(e.into()),
        };
        let mut days: Vec<NaiveDate> = msgs.iter().map(|m| day_of(m.ts)).collect();
        days.sort_unstable();
        days.dedup();
        let out = (days, truncated);
        self.cache
            .lock()
            .unwrap()
            .dates
            .insert(conv.clone(), Cached::new(out.clone()));
        Ok(out)
    }

    /// One day of a conversation, assembled from a single history call.
    async fn day(&self, conv: &ConvId, date: NaiveDate) -> io::Result<Arc<Day>> {
        let key = (conv.clone(), date);
        if let Some(v) = Cached::get(self.cache.lock().unwrap().days.get(&key)) {
            return Ok(v.clone());
        }
        let msgs = match self.source.history(conv, window_of(date)).await {
            Ok((msgs, _truncated)) => msgs,
            Err(e) if e.is_conversation_denied() => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let day = Arc::new(self.assemble(&msgs).await?);
        self.cache
            .lock()
            .unwrap()
            .days
            .insert(key, Cached::new(day.clone()));
        Ok(day)
    }

    /// One thread, which is [`Scope`]-identical to a day: same files, same `chat.jsonl`.
    ///
    /// [`Scope`]: Day
    async fn thread(&self, conv: &ConvId, root: &MsgId) -> io::Result<Arc<Day>> {
        let key = (conv.clone(), root.clone());
        if let Some(v) = Cached::get(self.cache.lock().unwrap().threads.get(&key)) {
            return Ok(v.clone());
        }
        let msgs = match self.source.thread(conv, root).await {
            Ok((msgs, _truncated)) => msgs,
            Err(e) if e.is_conversation_denied() => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        if msgs.is_empty() {
            return Err(io_err(io::ErrorKind::NotFound));
        }
        let day = Arc::new(self.assemble(&msgs).await?);
        self.cache
            .lock()
            .unwrap()
            .threads
            .insert(key, Cached::new(day.clone()));
        Ok(day)
    }

    /// Render a run of messages into the three things a directory serves.
    async fn assemble(&self, msgs: &[super::Message]) -> io::Result<Day> {
        // Names resolved once for the whole run rather than per line: the directory is one
        // call and a line without a name is a line nobody can `grep` for by person.
        let by_id: HashMap<String, String> = self
            .users()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|u| (u.id, u.name))
            .collect();

        let mut chat = Vec::new();
        let mut threads = Vec::new();
        let mut files = Vec::new();
        for m in msgs {
            let mut m = m.clone();
            if m.from.name.is_none()
                && let Some(n) = by_id.get(&m.from.id)
            {
                m.from.name = Some(n.clone());
            }
            chat.extend_from_slice(&render_line(&m));
            // A root with replies, which is what `threads/` lists. A reply names its root and
            // is not itself one.
            if let Some(t) = &m.thread
                && t.root.is_none()
                && t.replies > 0
            {
                threads.push(m.id.clone());
            }
            files.extend(m.files.iter().cloned());
        }
        Ok(Day {
            chat: Arc::new(chat),
            threads,
            files,
        })
    }

    // ---- resolving a path ------------------------------------------------------------

    /// The conversation a `<name>__<id>` directory names, in the section `kind`.
    ///
    /// Matched on the id half rather than the name: a name is what a workspace renames, and a
    /// path that stops resolving because somebody retitled a channel is a path that was never
    /// an address.
    async fn conv_at(&self, kind: ConvKind, dir: &str) -> io::Result<Conversation> {
        let id = ConvId(id_of(dir).to_string());
        self.conversations()
            .await?
            .into_iter()
            .find(|c| c.kind == kind && c.id == id)
            .ok_or(io_err(io::ErrorKind::NotFound))
    }

    /// Whether `date` is a day of `conv` that may be served, and its content if so.
    ///
    /// The three-case rule from the module docs lives here, and nowhere else.
    async fn day_if_present(&self, conv: &ConvId, date: NaiveDate) -> io::Result<Arc<Day>> {
        let (days, truncated) = self.dates(conv).await?;
        if days.binary_search(&date).is_ok() {
            return self.day(conv, date).await;
        }
        // Inside a range the walk covered, an unlisted day is empty and known to be: the walk
        // saw every day from its floor to the newest message, so this costs no request.
        let floor = days.first().copied();
        let inside = floor.is_some_and(|f| date >= f);
        if inside || !truncated {
            return Err(io_err(io::ErrorKind::NotFound));
        }
        // Below a truncated walk's floor. The walk never reached here, so nothing has been
        // proved — ask, and serve it only if it has messages.
        let day = self.day(conv, date).await?;
        if day.chat.is_empty() {
            return Err(io_err(io::ErrorKind::NotFound));
        }
        Ok(day)
    }

    /// The sections this source actually has. `dms/` is absent rather than empty when the
    /// source cannot enumerate DMs at all — an empty directory would say there are none.
    fn sections(&self) -> Vec<&'static str> {
        let caps = self.source.capabilities();
        let mut out = Vec::new();
        if caps.channels {
            out.push(CHANNELS);
        }
        if caps.dms {
            out.push(DMS);
        }
        // Always: every messenger has members, and resolving an author id is what makes a
        // line searchable by person rather than by opaque id.
        out.push(USERS);
        out
    }

    fn section_kind(&self, seg: &str) -> Option<ConvKind> {
        let caps = self.source.capabilities();
        match seg {
            CHANNELS if caps.channels => Some(ConvKind::Channel),
            DMS if caps.dms => Some(ConvKind::Dm),
            _ => None,
        }
    }

    /// Resolve everything below a day or thread directory: the shared tail of both, since a
    /// thread is the same shape as a day.
    async fn within(&self, day: Arc<Day>, rest: &[String]) -> io::Result<Node> {
        match rest {
            [] => Ok(Node::Dir),
            [f] if f == CHAT => Ok(Node::Bytes(day.chat.clone())),
            [f] if f == FILES => Ok(Node::Dir),
            [f, name] if f == FILES => {
                let want = id_of(name);
                let file = day
                    .files
                    .iter()
                    .find(|f| f.id == want)
                    .ok_or(io_err(io::ErrorKind::NotFound))?;
                Ok(Node::File(file.clone()))
            }
            _ => Err(io_err(io::ErrorKind::NotFound)),
        }
    }
}

/// What a path names, once resolved.
enum Node {
    /// A directory this volume synthesized.
    Dir,
    /// A file whose bytes are already in hand.
    Bytes(Arc<Vec<u8>>),
    /// An attachment, which is bytes the source still has to fetch.
    File(FileRef),
}

impl<S: MessengerSource> MessengerFs<S> {
    /// The one path walker. `stat`, `list` and `open` all ask it what a path names, so the
    /// three cannot disagree about what exists.
    async fn resolve(&self, path: &Path) -> io::Result<Node> {
        let segs = segments(path);
        match segs.as_slice() {
            [] => Ok(Node::Dir),

            [s] if self.sections().contains(&s.as_str()) => Ok(Node::Dir),

            // users/<name>__<id>.json
            [s, name] if s == USERS => {
                let want = id_of(name.trim_end_matches(".json"));
                let user = self
                    .users()
                    .await?
                    .into_iter()
                    .find(|u| u.id == want)
                    .ok_or(io_err(io::ErrorKind::NotFound))?;
                let bytes = serde_json::to_vec_pretty(&user.record)
                    .map_err(io::Error::other)?;
                Ok(Node::Bytes(Arc::new(bytes)))
            }

            // <section>/<conv>/…
            [s, conv_dir, rest @ ..] if self.section_kind(s).is_some() => {
                let kind = self.section_kind(s).expect("checked");
                let conv = self.conv_at(kind, conv_dir).await?;
                match rest {
                    [] => Ok(Node::Dir),
                    [date, tail @ ..] => {
                        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
                            .map_err(|_| io_err(io::ErrorKind::NotFound))?;
                        let day = self.day_if_present(&conv.id, date).await?;
                        match tail {
                            [t] if t == THREADS => Ok(Node::Dir),
                            [t, root, within @ ..] if t == THREADS => {
                                let root = MsgId(root.clone());
                                if !day.threads.contains(&root) {
                                    return Err(io_err(io::ErrorKind::NotFound));
                                }
                                let thread = self.thread(&conv.id, &root).await?;
                                self.within(thread, within).await
                            }
                            _ => self.within(day, tail).await,
                        }
                    }
                }
            }

            _ => Err(io_err(io::ErrorKind::NotFound)),
        }
    }
}

impl<S: MessengerSource> MessengerFs<S> {
    /// The listing for a directory. Split out of [`FileSystem::list`] so the boxed future has
    /// one `await` to hold rather than the whole match.
    async fn listing(&self, path: &Path) -> io::Result<Vec<Dirent>> {
        let segs = segments(path);
        match segs.as_slice() {
            [] => Ok(self
                .sections()
                .into_iter()
                .map(|s| Dirent::new(s, DirentKind::Dir))
                .collect()),

            [s] if s == USERS => Ok(self
                .users()
                .await?
                .into_iter()
                .map(|u| {
                    Dirent::new(
                        format!("{}__{}.json", sanitize(&u.name), u.id),
                        DirentKind::File,
                    )
                })
                .collect()),

            [s] if self.section_kind(s).is_some() => {
                let kind = self.section_kind(s).expect("checked");
                Ok(self
                    .conversations()
                    .await?
                    .into_iter()
                    .filter(|c| c.kind == kind)
                    .map(|c| {
                        Dirent::new(
                            format!("{}__{}", sanitize(&c.name), c.id.0),
                            DirentKind::Dir,
                        )
                    })
                    .collect())
            }

            // A conversation lists the days it has.
            [s, conv_dir] if self.section_kind(s).is_some() => {
                let kind = self.section_kind(s).expect("checked");
                let conv = self.conv_at(kind, conv_dir).await?;
                let (days, _) = self.dates(&conv.id).await?;
                Ok(days
                    .into_iter()
                    .map(|d| Dirent::new(d.format("%Y-%m-%d").to_string(), DirentKind::Dir))
                    .collect())
            }

            _ => match self.resolve(path).await? {
                Node::Dir => self.list_dir(&segs).await,
                Node::Bytes(_) | Node::File(_) => Err(io_err(io::ErrorKind::NotADirectory)),
            },
        }
    }
}

impl<S: MessengerSource> FileSystem for MessengerFs<S> {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            match self.resolve(path).await? {
                Node::Dir => Ok(self.dir()),
                Node::Bytes(b) => Ok(self.file(b.len() as u64)),
                Node::File(f) => Ok(self.file(f.size)),
            }
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move { self.listing(path).await })
    }

    /// Serve `buf` from `offset`.
    ///
    /// Two shapes behind one method, decided by what the path names rather than by how it is
    /// read — [`FileRef::size`] is in the listing, so there is nothing to learn by watching.
    /// Assembled bytes are already in hand; an attachment is fetched whole when it fits within
    /// the store's memory ceiling for one file, and a window at a time when it does not.
    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            match self.resolve(path).await? {
                Node::Dir => Err(io_err(io::ErrorKind::IsADirectory)),
                Node::Bytes(data) => Ok(copy_out(&data, buf, offset)),

                // Unranged, which is what lets the source check the body against the length the
                // listing promised — the check that catches a refused token answering 200 with a
                // login page. It is also no more requests: a window is larger than the file.
                Node::File(f) if f.size <= WINDOW => {
                    let (bytes, _) = self.source.fetch_file(&f, None).await?;
                    Ok(copy_out(&bytes, buf, offset))
                }

                Node::File(f) => self.windowed(path, &f, buf, offset).await,
            }
        })
    }
}

impl<S: MessengerSource> MessengerFs<S> {
    /// Listings for the directories inside a day or a thread.
    async fn list_dir(&self, segs: &[String]) -> io::Result<Vec<Dirent>> {
        let (Some(s), Some(conv_dir)) = (segs.first(), segs.get(1)) else {
            return Err(io_err(io::ErrorKind::NotFound));
        };
        let kind = self.section_kind(s).ok_or(io_err(io::ErrorKind::NotFound))?;
        let conv = self.conv_at(kind, conv_dir).await?;
        let date = segs
            .get(2)
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .ok_or(io_err(io::ErrorKind::NotFound))?;
        let day = self.day_if_present(&conv.id, date).await?;

        // A thread's own subdirectories reuse the day's shape, so resolve which scope this
        // listing is inside before naming its entries.
        let (scope, tail) = match &segs[3..] {
            [t, root, rest @ ..] if t == THREADS => {
                (self.thread(&conv.id, &MsgId(root.clone())).await?, rest)
            }
            rest => (day.clone(), rest),
        };

        match tail {
            [] if segs.len() == 3 => {
                // The day itself: chat.jsonl, plus whichever of the two directories has
                // anything in it. An empty one would be a claim the tree cannot support.
                let mut out = vec![Dirent::new(CHAT, DirentKind::File)];
                if !scope.threads.is_empty() {
                    out.push(Dirent::new(THREADS, DirentKind::Dir));
                }
                if !scope.files.is_empty() {
                    out.push(Dirent::new(FILES, DirentKind::Dir));
                }
                Ok(out)
            }
            [t] if t == THREADS => Ok(day
                .threads
                .iter()
                .map(|r| Dirent::new(r.0.clone(), DirentKind::Dir))
                .collect()),
            [] => {
                // A thread directory.
                let mut out = vec![Dirent::new(CHAT, DirentKind::File)];
                if !scope.files.is_empty() {
                    out.push(Dirent::new(FILES, DirentKind::Dir));
                }
                Ok(out)
            }
            [f] if f == FILES => Ok(scope
                .files
                .iter()
                .map(|f| {
                    let mut st = Stat::new(DirentKind::File, f.size);
                    st.mtime = Some(self.born);
                    Dirent::with_stat(format!("{}__{}", sanitize(&f.name), f.id), st)
                })
                .collect()),
            _ => Err(io_err(io::ErrorKind::NotFound)),
        }
    }
}


impl<S: MessengerSource> MessengerFs<S> {
    /// An attachment past [`WINDOW`], served from the one window this store holds.
    ///
    /// A request the held window only partly covers must not stop there: the trait states that a
    /// short read is EOF, so a document would appear to end at a window boundary. Windows begin
    /// where a miss was rather than on a grid, so that seam lands mid-request whenever the first
    /// access was not at zero — a PDF reader opening at the trailer is the ordinary case.
    async fn windowed(
        &self,
        path: &Path,
        f: &FileRef,
        buf: &mut [u8],
        offset: u64,
    ) -> io::Result<usize> {
        if buf.is_empty() || offset >= f.size {
            return Ok(0);
        }
        // Never past the end the listing named: a range beyond it is a request the source would
        // refuse, and refusing here costs no round trip.
        let want = (buf.len() as u64).min(f.size - offset) as usize;
        let buf = &mut buf[..want];

        let mut filled = 0usize;
        while filled < want {
            let at = offset + filled as u64;
            let from_held = self.serve_held(path, at, &mut buf[filled..]);
            if from_held > 0 {
                filled += from_held;
                continue;
            }
            // A miss always pulls a whole window rather than only what was asked. No continuity
            // test, unlike an object store's: a document is read forwards, and the cost of being
            // wrong is bounded by `WINDOW` on a file already past it.
            let end = (at + WINDOW).min(f.size);
            let (data, served_range) = self.source.fetch_file(f, Some(at..end)).await?;
            if data.is_empty() {
                // A non-empty range answered with no bytes has no meaning here, and looping on it
                // would not terminate. Short of `want`, so a caller reads it as the end — which
                // it is, as far as this store can tell.
                break;
            }
            // A range is a request, not a promise. Answered with `false`, the range was ignored
            // and this is the file from byte zero — recording it as if it began at `at` would
            // hand out the head of the file for every offset, quietly and with the right length.
            let start = if served_range { at } else { 0 };
            let Some(from) = at.checked_sub(start).filter(|f| *f < data.len() as u64) else {
                // Neither shape can land here. Breaking rather than looping, because a source
                // that answered outside both would not terminate.
                break;
            };
            let from = from as usize;
            let n = (data.len() - from).min(want - filled);
            buf[filled..filled + n].copy_from_slice(&data[from..from + n]);
            filled += n;
            self.cache.lock().unwrap().window = Some((path.to_path_buf(), start, data));
        }
        Ok(filled)
    }

    /// Serve as much of `out` as the held window covers *for this path*, returning how much that
    /// was. Zero means it does not — either a different file is held, or this offset is outside
    /// the bytes that are.
    fn serve_held(&self, path: &Path, at: u64, out: &mut [u8]) -> usize {
        let held = self.cache.lock().unwrap();
        let Some((held_path, start, data)) = held.window.as_ref() else {
            return 0;
        };
        if held_path != path || at < *start || at >= *start + data.len() as u64 {
            return 0;
        }
        let from = (at - *start) as usize;
        let n = (data.len() - from).min(out.len());
        out[..n].copy_from_slice(&data[from..from + n]);
        n
    }
}

/// Copy out of bytes already in hand. `Ok(0)` past the end, which is EOF.
fn copy_out(data: &[u8], buf: &mut [u8], offset: u64) -> usize {
    if offset >= data.len() as u64 {
        return 0;
    }
    let from = offset as usize;
    let n = (data.len() - from).min(buf.len());
    buf[..n].copy_from_slice(&data[from..from + n]);
    n
}

/// An errno-shaped failure with no message of its own.
fn io_err(kind: io::ErrorKind) -> io::Error {
    kind.into()
}

// ---- helpers -------------------------------------------------------------------------

/// A path's components, `/`-separated and empties dropped.
fn segments(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The id half of a `<name>__<id>` entry — everything after the last `__`.
///
/// The id is last so that a name containing `__` cannot swallow it, which a workspace where
/// somebody titled a channel `a__b` would otherwise do.
fn id_of(entry: &str) -> &str {
    entry.rsplit_once("__").map(|(_, id)| id).unwrap_or(entry)
}

/// A display name made safe to be one path component.
///
/// `/` would open a directory nobody created and a leading `.` hides the entry from an `ls`
/// that did not ask; a name that is entirely separators still has to leave something behind,
/// or two conversations collapse onto one path.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// The UTC day a timestamp falls in — the tree's bucketing rule, in one place.
fn day_of(ts: SystemTime) -> NaiveDate {
    DateTime::<Utc>::from(ts).date_naive()
}

/// The half-open span a date directory stands for.
fn window_of(date: NaiveDate) -> Window {
    let start = Utc
        .from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists"))
        .into();
    let end = Utc
        .from_utc_datetime(
            &date
                .succ_opt()
                .expect("a date has a successor")
                .and_hms_opt(0, 0, 0)
                .expect("midnight exists"),
        )
        .into();
    Window { start, end }
}

#[cfg(test)]
#[path = "messenger_tests.rs"]
mod messenger_tests;
