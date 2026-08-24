//! [`MessengerFs`] — a messenger's conversations as a filesystem, synthesized along the
//! time axis.
//!
//! Written once and generic over [`MessengerSource`], which is the lane's whole design. A
//! messenger has no hierarchy to mirror, so every platform's tree has to be *invented*; four
//! adapters each inventing one is four layouts a document claims are the same.
//!
//! ```text
//! channels/<name>__<id>/
//!   2026/                     a year the conversation reaches
//!     08/                      a month of it — the unit that gets fetched
//!       2026-08-10.jsonl       a day that HAS messages, one line each
//!       threads/<root-id>.jsonl   a thread, under the month its root is in
//!       files/<stem>__<id>.ext attachments posted that month, outside a thread
//! dms/…                       only when the source can enumerate them
//! users/<name>__<id>.json
//! ```
//!
//! # Why the date axis is arithmetic
//!
//! No messenger has an endpoint for "which days does this conversation have" — all four ask
//! the same way, with a window. So the axis cannot be *discovered*; it is computed, from
//! [`Conversation::created`] up to today, and every platform carries `created` in the
//! conversation listing already.
//!
//! Which leaves the question of what a directory is worth. A day per day would name every
//! silent day, and on a long quiet channel that is nearly all of them — the reader here is an
//! agent, which cannot tell a quiet day from a failed request and pays a round trip to find
//! out. So the levels split the difference:
//!
//! * `<year>/<month>/` is arithmetic and always complete, so listing it spends no request
//!   past the one that listed the conversation itself — and a month holds under twelve
//!   entries however old the conversation is;
//! * inside a month, only a day that *has* messages is a name. A silent day is absent rather
//!   than an empty file, which is the distinction a listing can actually make.
//!
//! # One window fills three listings
//!
//! Entering a month is one [`history`](MessengerSource::history) window — plus the roster
//! [`assemble`](MessengerFs::assemble) resolves author names from, which is its own walk and
//! on a large workspace the larger half of the bill. What the window answers is every day
//! file in the month, the `threads/` listing (roots with replies) and the `files/` listing
//! together, so reading any day in it is free *while the month is still held*: past
//! [`ttl`](MessengerLimits::ttl), or once
//! [`text_budget`](MessengerLimits::text_budget) has evicted it, the next read pays again.
//! That is [`Body`], and it is cached whole.
//!
//! A day named directly costs its own window instead, not its month's: a `search` hit names
//! exactly one day, and buying the month around it would spend a busy channel's whole month
//! to answer one `cat`.
//!
//! # Two known limits
//!
//! A thread lives under its *root's* month, so replies written later do not appear under the
//! month they were written, and an attachment posted in a reply is in no `files/` listing at
//! all. Not fixable from here: a history window selects on a message's own timestamp, and
//! replies are not in it.
//!
//! And a listing is all of it or an error, never part of it. That is what a source promises,
//! and the reason it has to: a day this tree does not name reads as a day that held nothing,
//! and a conversation it does not name is unreachable by path as well — so a short answer is
//! not an incomplete answer here, it is a wrong one, and nothing in a directory could correct
//! for it. The price is that a walk broken off on its last page throws away every page before
//! it. Serving those instead would need somewhere to say they are not the whole, which is
//! what a `.partial` beside the listing would be, and it is not built.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use chrono::{Datelike, NaiveDate, TimeZone, Utc};

use super::limits::MessengerLimits;
use super::paths::{
    CHANNELS, DMS, FILES, JSONL, THREADS, USERS, conv_dir, day_file, day_of, day_of_file, entry, file_entry,
    month_dir, month_of, months, thread_file, year_dir,
};
use super::source::{ConvId, ConvKind, Conversation, FileRef, MessengerSource, MsgId, Window};
use super::{User, render_line};
use crate::BoxFuture;
use crate::fs::{Dirent, DirentKind, FileSystem, Stat};

/// A value and when it stops being reusable.
struct Cached<T> {
    value: T,
    at: Instant,
}

impl<T> Cached<T> {
    /// The value, if it is still within `ttl`.
    ///
    /// The window is passed in rather than read from a constant, because it is the store's
    /// budget and this type does not know which store it belongs to.
    fn get(entry: Option<&Cached<T>>, ttl: Duration) -> Option<&T> {
        entry.filter(|c| c.at.elapsed() < ttl).map(|c| &c.value)
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
/// cache entry and not three: `ls` of a month, `ls` of its `threads/`, `ls` of its `files/`
/// and `cat` of any day in it are four operations over one fetch.
struct Body {
    /// Rendered lines, by the file name each is served under.
    ///
    /// A month has one entry per day that has anything — which is why a silent day is not a
    /// name in the listing rather than a name with nothing behind it. A thread has exactly one,
    /// under `<root>.jsonl`. Resolving a file is a lookup in here, whichever scope it is.
    text: BTreeMap<String, Arc<Vec<u8>>>,
    /// Roots that have replies — the month's `threads/` listing. Empty for a thread.
    threads: Vec<MsgId>,
    /// Attachments posted in the month. Empty for a thread, whose attachments are the month's:
    /// one `files/` per month rather than the same file under a day and again under a thread.
    files: Vec<FileRef>,
}

impl Body {
    /// What the budget counts, which is every byte held.
    ///
    /// Not only the rendered text: a month of attachment-heavy messages carries a `FileRef`
    /// each, and a `name` and a `url` are real bytes the budget has to see or it is bounding
    /// the wrong thing. Ids and names are counted as their own lengths rather than by
    /// `size_of` — the fixed part of a `Vec` is noise next to what it points at.
    fn weight(&self) -> u64 {
        let text: u64 = self.text.values().map(|t| t.len() as u64).sum();
        let threads: u64 = self.threads.iter().map(|t| t.0.len() as u64).sum();
        let files: u64 = self
            .files
            .iter()
            .map(|f| (f.id.len() + f.name.len() + f.url.len()) as u64)
            .sum();
        text + threads + files
    }
}

#[derive(Default)]
struct Cache {
    convs: Option<Cached<Vec<Conversation>>>,
    users: Option<Cached<Vec<User>>>,
    /// Assembled scopes, days and threads together.
    ///
    /// One map and not two, because a thread *is* a day as far as this cache is concerned —
    /// same type, same lifecycle, same bytes to account for. Two maps would need one budget
    /// spanning both, which is a budget with two places to get it wrong.
    text: HashMap<Scope, Held>,
    /// What `text` currently holds, in the bytes the budget is written in.
    text_bytes: u64,
    /// Ticks on every use, so the least recently used scope is the one with the lowest stamp.
    ///
    /// A counter and not a clock: what eviction needs is an order, and an order does not need
    /// to know what time it is.
    clock: u64,
    /// The one attachment window held, as `(the path's segments, start, bytes)`.
    ///
    /// Segments and not the caller's `Path`, for the reason [`Scope`] is a key rather than a
    /// path: `resolve` normalizes, so `/a/b`, `a/b` and `//a//b/` are one file — and comparing
    /// the spelling would make them three cache identities, each paying its own fetch for the
    /// same bytes.
    ///
    /// One and not a map, which is what makes [`window`](MessengerLimits::window) a ceiling on
    /// the whole store rather
    /// than on each open file: a reader walking a document forwards keeps replacing it, and two
    /// readers alternating between large attachments replace each other's — slower, bounded, and
    /// rare, where a map would be unbounded because nothing here is told when a file is closed.
    window: Option<(Vec<String>, u64, Vec<u8>)>,
}

/// One assembled scope: a day of a conversation, or one thread inside it.
///
/// A key and not a path, because the cache is asked before a path exists — and because two
/// spellings of one day must not become two entries.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Scope {
    /// A whole month, which is what a listing needs: it has to name every day that has
    /// anything, and only the month's own window answers that.
    Month(ConvId, i32, u32),
    /// One day, which is what reading a day file needs.
    ///
    /// Separate from `Month` because the two questions cost differently. A month of a busy
    /// channel is hundreds of calls — 9,000 messages is 180 of them at Graph's fifty per page,
    /// 600 at Slack's unlisted fifteen — and spending that to answer `cat` on one day is the
    /// wrong trade, especially since a `search` hit names exactly one day. A month already
    /// held answers a day for free; a day never fetches its month.
    Day(ConvId, NaiveDate),
    Thread(ConvId, MsgId),
}

/// A held scope, with what eviction needs to know about it.
struct Held {
    body: Arc<Body>,
    at: Instant,
    /// The clock value of its last use. Eviction takes the smallest.
    used: u64,
}

/// A messenger as a read-only filesystem. See the module docs.
pub struct MessengerFs<S> {
    source: Arc<S>,
    /// The timestamp every synthesized directory reports.
    ///
    /// Fixed at construction for the reason [`WorkFs`](crate::fs::WorkFs)'s is: a
    /// directory advertising `now()` on each `stat` looks perpetually modified, and a guest
    /// that negotiated `AUTO_INVAL_DATA` watches that field to decide when to drop cached
    /// pages.
    born: SystemTime,
    cache: Mutex<Cache>,
    limits: MessengerLimits,
}

impl<S: MessengerSource> MessengerFs<S> {
    /// A mount over `source`, with the budgets this lane chose.
    pub fn new(source: S) -> Self {
        Self::with_limits(source, MessengerLimits::default())
    }

    /// A mount over `source` that spends what `limits` allows.
    ///
    /// What a host serving many mounts reaches for: every one of these holds its own caches, so
    /// the cost of one is the cost of a thousand divided by a thousand — see [`MessengerLimits`]
    /// for what each number buys.
    pub fn with_limits(source: S, limits: MessengerLimits) -> Self {
        MessengerFs {
            source: Arc::new(source),
            born: SystemTime::now(),
            cache: Mutex::new(Cache::default()),
            limits,
        }
    }

    /// The assembled scope, if it is held and still fresh.
    ///
    /// Touching it on the way out is what makes the eviction order *recency* and not insertion:
    /// the day a reader keeps coming back to outlives the one they passed through once.
    fn held(&self, scope: &Scope) -> Option<Arc<Body>> {
        let mut cache = self.cache.lock().unwrap();
        let clock = cache.clock + 1;
        let entry = cache.text.get_mut(scope)?;
        if entry.at.elapsed() >= self.limits.ttl {
            return None;
        }
        entry.used = clock;
        let body = entry.body.clone();
        cache.clock = clock;
        Some(body)
    }

    /// Hold an assembled scope, dropping the least recently used until the budget is met.
    ///
    /// The scan for a victim is linear, which is the right shape here: the map holds as many
    /// scopes as the budget has room for — tens, not thousands — and a heap would be more
    /// bookkeeping than the thing it speeds up.
    ///
    /// Never the entry just held, and not by a special case: it was touched last, so it has the
    /// highest stamp. That is what leaves a single day larger than the whole budget in place
    /// while it is being read, rather than refetching it for every chunk a kernel asks for.
    fn hold(&self, scope: Scope, body: Arc<Body>) {
        let weight = body.weight();
        let mut cache = self.cache.lock().unwrap();
        cache.clock += 1;
        let used = cache.clock;
        let replaced = cache.text.insert(
            scope,
            Held {
                body,
                at: Instant::now(),
                used,
            },
        );
        if let Some(old) = replaced {
            cache.text_bytes -= old.body.weight();
        }
        cache.text_bytes += weight;

        while cache.text_bytes > self.limits.text_budget && cache.text.len() > 1 {
            let Some(victim) = cache
                .text
                .iter()
                .min_by_key(|(_, held)| held.used)
                .map(|(scope, _)| scope.clone())
            else {
                break;
            };
            if let Some(gone) = cache.text.remove(&victim) {
                cache.text_bytes -= gone.body.weight();
            }
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
        if let Some(v) = Cached::get(self.cache.lock().unwrap().convs.as_ref(), self.limits.ttl) {
            return Ok(v.clone());
        }
        let convs = self.source.conversations().await?;
        self.cache.lock().unwrap().convs = Some(Cached::new(convs.clone()));
        Ok(convs)
    }

    async fn users(&self) -> io::Result<Vec<User>> {
        if let Some(v) = Cached::get(self.cache.lock().unwrap().users.as_ref(), self.limits.ttl) {
            return Ok(v.clone());
        }
        let users = self.source.users().await?;
        self.cache.lock().unwrap().users = Some(Cached::new(users.clone()));
        Ok(users)
    }

    /// A month, which is the one *fetch* the date axis costs — one window, however many pages
    /// the service breaks it into, plus the roster [`assemble`](Self::assemble) needs.
    ///
    /// What comes back is partitioned into a file per day that has anything — so the days are
    /// bought by the same fetch that named the month, and reading any of them afterwards is
    /// free. Fetching a day at a time would ask once per day to learn the same thing, and most
    /// days in most conversations have nothing to learn.
    async fn month(&self, conv: &ConvId, year: i32, month: u32) -> io::Result<Arc<Body>> {
        let key = Scope::Month(conv.clone(), year, month);
        if let Some(body) = self.held(&key) {
            return Ok(body);
        }
        let msgs = match self.source.history(conv, window_of(year, month)).await {
            Ok(msgs) => msgs,
            Err(e) if e.is_conversation_denied() => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let body = Arc::new(self.assemble(&msgs, None).await?);
        self.hold(key, body.clone());
        Ok(body)
    }

    /// One day, from whichever scope already answers it.
    ///
    /// The month first, because a listing usually came before the read and bought every day in
    /// it — that is what keeps `grep -r` over a month at one fetch rather than one per file.
    /// Failing that, the day's own window, which is a fetch for that day alone.
    async fn day(&self, conv: &ConvId, date: NaiveDate) -> io::Result<Arc<Body>> {
        if let Some(month) = self.held(&Scope::Month(conv.clone(), date.year(), date.month())) {
            return Ok(month);
        }
        let key = Scope::Day(conv.clone(), date);
        if let Some(body) = self.held(&key) {
            return Ok(body);
        }
        let msgs = match self.source.history(conv, window_of_day(date)).await {
            Ok(msgs) => msgs,
            Err(e) if e.is_conversation_denied() => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let body = Arc::new(self.assemble(&msgs, None).await?);
        self.hold(key, body.clone());
        Ok(body)
    }

    /// One thread, served as the single file `threads/<root>.jsonl`.
    ///
    /// A file and not a directory, unlike the day it hangs under: a thread has no sub-scopes of
    /// its own, and its attachments are the month's — one `files/` rather than the same
    /// attachment under a day and again under the thread that carried it.
    async fn thread(&self, conv: &ConvId, root: &MsgId) -> io::Result<Arc<Body>> {
        let key = Scope::Thread(conv.clone(), root.clone());
        if let Some(body) = self.held(&key) {
            return Ok(body);
        }
        let msgs = match self.source.thread(conv, root).await {
            Ok(msgs) => msgs,
            Err(e) if e.is_conversation_denied() => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        if msgs.is_empty() {
            return Err(io_err(io::ErrorKind::NotFound));
        }
        let name = thread_file(&root.0);
        let body = Arc::new(self.assemble(&msgs, Some(name)).await?);
        self.hold(key, body.clone());
        Ok(body)
    }

    /// Render a run of messages into what a scope serves.
    ///
    /// `as_one` names the single file everything goes into, which is what a thread wants. Left
    /// out, the run is partitioned by the UTC day each message falls in — a month's own shape,
    /// and the reason a day file exists only for a day that has one.
    async fn assemble(&self, msgs: &[super::Message], as_one: Option<String>) -> io::Result<Body> {
        // Names resolved once for the whole run rather than per line, because a line without a
        // name is a line nobody can `grep` for by person.
        //
        // Not cheap, and cheap in a different currency: a roster is at least
        // `ceil(members / 200)` requests — at least, because a page may come back short of what
        // it was asked for — so its price is set by how large the workspace is, where the
        // window's is set by how much the channel said. Two unrelated numbers, neither of which
        // dominates. What is worth knowing is that this one does not shrink when less is read:
        // three lines of one day cost the same roster as a year of the busiest channel. Held
        // for `ttl` like everything else, which is the only thing keeping the next scope from
        // paying it again.
        let by_id: HashMap<String, String> = self
            .users()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|u| (u.id, u.name))
            .collect();

        let mut text: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut threads = Vec::new();
        let mut files = Vec::new();
        for m in msgs {
            let mut m = m.clone();
            if m.from.name.is_none()
                && let Some(n) = by_id.get(&m.from.id)
            {
                m.from.name = Some(n.clone());
            }
            let into = as_one
                .clone()
                .unwrap_or_else(|| day_file(day_of(m.ts)));
            text.entry(into)
                .or_default()
                .extend_from_slice(&render_line(&m));
            // A root with replies, which is what `threads/` lists. A reply names its root and
            // is not itself one.
            if let Some(t) = &m.thread
                && t.root.is_none()
                && t.replies > 0
            {
                threads.push(m.id.clone());
            }
            // By id, because one file shared into two messages of the same month arrives
            // twice — and two entries of one name in a directory is a listing `ls` and `find`
            // cannot make sense of.
            for f in &m.files {
                if !files.iter().any(|held: &FileRef| held.id == f.id) {
                    files.push(f.clone());
                }
            }
        }
        Ok(Body {
            text: text.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            threads: if as_one.is_some() { Vec::new() } else { threads },
            files: if as_one.is_some() { Vec::new() } else { files },
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

    /// The conversation a `<section>/<dir>` pair names.
    ///
    /// The three date levels all need it — for `created` and nothing else — so the two steps
    /// they share are one call rather than three copies of it.
    async fn conv_of(&self, section: &str, dir: &str) -> io::Result<Conversation> {
        let kind = self
            .section_kind(section)
            .ok_or(io_err(io::ErrorKind::NotFound))?;
        self.conv_at(kind, dir).await
    }

    /// The top of the date axis: what the host pinned, or today in UTC.
    ///
    /// Read on every listing rather than fixed at construction, unlike
    /// [`born`](MessengerFs::born): a mount left open across midnight has to start listing the
    /// new day, where a directory's reported mtime must not move.
    fn today(&self) -> NaiveDate {
        self.limits.today.unwrap_or_else(|| Utc::now().date_naive())
    }

    /// Whether the conversation reaches `year`-`month` at all. Arithmetic; no request.
    fn reaches(&self, conv: &Conversation, year: i32, month: u32) -> bool {
        months(day_of(conv.created), self.today()).contains(&(year, month))
    }

    /// Resolve `tail` against the month itself, for the names a day fetch cannot answer.
    async fn in_month(
        &self,
        conv: &Conversation,
        y: &str,
        m: &str,
        tail: &[String],
    ) -> io::Result<Node> {
        let month = self.month_if_present(conv, y, m).await?;
        self.within(month, tail).await
    }

    /// The month `seg` names, or `NotFound` when the conversation never reached it.
    ///
    /// "Reached" is the calendar: from the month it was created in to this one. Outside that
    /// there is nothing to fetch and no request is spent finding out. Inside it, a month
    /// nothing was said in is a real month that is empty — the directory is there because the
    /// month was, and it lists nothing.
    async fn month_if_present(
        &self,
        conv: &Conversation,
        y: &str,
        m: &str,
    ) -> io::Result<Arc<Body>> {
        let (year, month) = month_of(y, m).ok_or(io_err(io::ErrorKind::NotFound))?;
        if !months(day_of(conv.created), self.today()).contains(&(year, month)) {
            return Err(io_err(io::ErrorKind::NotFound));
        }
        self.month(&conv.id, year, month).await
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
    async fn within(&self, day: Arc<Body>, rest: &[String]) -> io::Result<Node> {
        match rest {
            [] => Ok(Node::Dir),
            // Absent on a day nothing was said, for the reason `threads/` and `files/` are:
            // a nothing that is *there* reads as a nothing that was asked about. The day
            // itself is a directory because the day existed; the file is the messages, and
            // there are none.
            // A file is whatever the scope rendered under that name. Nothing is served for a
            // name it did not render — which is how a silent day is absent rather than zero
            // bytes, the same way `threads/` and `files/` are absent when empty.
            [f] if day.text.contains_key(f) => Ok(Node::Bytes(day.text[f].clone())),
            [f] if f == FILES && !day.files.is_empty() => Ok(Node::Dir),
            [f, name] if f == FILES => {
                // Matched by rebuilding the listed name rather than by parsing this one. The
                // extension now sits past the id, so `id_of` would take `F1.pdf` for the id —
                // and rebuilding is what keeps `resolve` unable to answer a name `listing`
                // would not have written.
                let file = day
                    .files
                    .iter()
                    .find(|f| file_entry(&f.name, &f.id) == *name)
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

            // users/<name>__<id>.json — and only that spelling. `trim_end_matches` strips the
            // suffix repeatedly and `id_of` falls back to the whole string, so a looser check
            // accepted `amy__U1`, `amy__U1.json.json`, `U1`, and `anything__U1.json` for one
            // user: four names a listing never wrote, and `resolve` may not answer what
            // `listing` would not name.
            [s, name] if s == USERS => {
                let want = id_of(name.strip_suffix(".json").ok_or(io_err(io::ErrorKind::NotFound))?);
                let user = self
                    .users()
                    .await?
                    .into_iter()
                    .find(|u| u.id == want && entry(&u.name, &u.id, ".json") == *name)
                    .ok_or(io_err(io::ErrorKind::NotFound))?;
                let bytes = serde_json::to_vec_pretty(&user.record).map_err(io::Error::other)?;
                Ok(Node::Bytes(Arc::new(bytes)))
            }

            // <section>/<conv>/…
            [s, conv_dir, rest @ ..] if self.section_kind(s).is_some() => {
                let kind = self.section_kind(s).expect("checked");
                let conv = self.conv_at(kind, conv_dir).await?;
                match rest {
                    // Each level is checked on the way down: `stat` has to refuse a year the
                    [] => Ok(Node::Dir),
                    // A year on its own, refused unless the conversation reached it. Checked
                    // here because `stat` is a way in that no listing went through.
                    [y] => {
                        let ok = year_of(y).is_some_and(|year| {
                            months(day_of(conv.created), self.today())
                                .iter()
                                .any(|(yy, _)| *yy == year)
                        });
                        if ok {
                            Ok(Node::Dir)
                        } else {
                            Err(io_err(io::ErrorKind::NotFound))
                        }
                    }
                    // A month on its own is arithmetic, and this arm is what keeps it that
                    // way: without it a bare month falls into `[y, m, tail @ ..]` below with an
                    // empty tail and fetches the whole month to answer `stat`. Nothing in the
                    // unit suite stats a month directory, so the cost only showed up once a
                    // kernel was in front of the tree — NFS looks up every entry a readdir
                    // named, so one `ls <year>` became twelve month fetches.
                    [y, m] => {
                        let (year, mon) =
                            month_of(y, m).ok_or(io_err(io::ErrorKind::NotFound))?;
                        if self.reaches(&conv, year, mon) {
                            Ok(Node::Dir)
                        } else {
                            Err(io_err(io::ErrorKind::NotFound))
                        }
                    }
                    // A day file names the one day it wants, so only that day is fetched. The
                    // arms below need the month itself — a listing has to name every day that
                    // has anything, and `threads/`/`files/` are the month's.
                    [y, m, name] if month_of(y, m).is_some() && name.ends_with(JSONL) => {
                        let (year, mon) = month_of(y, m).expect("checked");
                        let Some(date) = day_of_file(name, year, mon) else {
                            // Not a day file of this month: `threads` and `files` are handled
                            // below, and anything else is nothing.
                            return self.in_month(&conv, y, m, std::slice::from_ref(name)).await;
                        };
                        if !self.reaches(&conv, year, mon) {
                            return Err(io_err(io::ErrorKind::NotFound));
                        }
                        self.within(self.day(&conv.id, date).await?, std::slice::from_ref(name))
                            .await
                    }
                    [y, m, tail @ ..] => {
                        // The month is checked before it is fetched: `stat` has to refuse one
                        // the conversation never reached, and a listing is not the only way in.
                        let month = self.month_if_present(&conv, y, m).await?;
                        match tail {
                            // Gated for the reason the listing gates them: a directory that
                            // is there and empty reads as a nothing that was asked about.
                            // `stat` is a way in that no listing went through.
                            [t] if t == THREADS && !month.threads.is_empty() => Ok(Node::Dir),
                            [t, name] if t == THREADS => {
                                let root = MsgId(
                                    name.strip_suffix(JSONL)
                                        .ok_or(io_err(io::ErrorKind::NotFound))?
                                        .to_string(),
                                );
                                if !month.threads.contains(&root) {
                                    return Err(io_err(io::ErrorKind::NotFound));
                                }
                                let thread = self.thread(&conv.id, &root).await?;
                                self.within(thread, std::slice::from_ref(name)).await
                            }
                            _ => self.within(month, tail).await,
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
                .map(|u| Dirent::new(entry(&u.name, &u.id, ".json"), DirentKind::File))
                .collect()),

            [s] if self.section_kind(s).is_some() => {
                let kind = self.section_kind(s).expect("checked");
                Ok(self
                    .conversations()
                    .await?
                    .into_iter()
                    .filter(|c| c.kind == kind)
                    .map(|c| Dirent::new(conv_dir(&c), DirentKind::Dir))
                    .collect())
            }

            // The date axis. Arithmetic over the conversation's own `created` — no request
            // past the one that listed the conversation, and never a window: every month the
            // conversation ever had is here, whether or not anything was said in it.
            [s, cd] if self.section_kind(s).is_some() => {
                let conv = self.conv_of(s, cd).await?;
                let mut years: Vec<i32> = months(day_of(conv.created), self.today())
                    .into_iter()
                    .map(|(y, _)| y)
                    .collect();
                years.dedup();
                Ok(years
                    .into_iter()
                    .map(|y| Dirent::new(year_dir(y), DirentKind::Dir))
                    .collect())
            }

            [s, cd, y] if self.section_kind(s).is_some() => {
                let conv = self.conv_of(s, cd).await?;
                let year = year_of(y).ok_or(io_err(io::ErrorKind::NotFound))?;
                let listed: Vec<Dirent> = months(day_of(conv.created), self.today())
                    .into_iter()
                    .filter(|(yy, _)| *yy == year)
                    .map(|(_, m)| Dirent::new(month_dir(m), DirentKind::Dir))
                    .collect();
                if listed.is_empty() {
                    return Err(io_err(io::ErrorKind::NotFound));
                }
                Ok(listed)
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
                Node::File(f) => Ok(self.file(self.measure(path, &f).await?)),
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

                // Whole, which is every file the store is not going to window: one at or under
                // the ceiling, and one whose length nobody said — see [`FileRef::size`].
                Node::File(f) if f.size.is_none_or(|n| n <= self.limits.window) => {
                    // When the listing named a size the end is known without asking, and
                    // without this the last read of every file — the one the kernel makes to
                    // see EOF — would fetch the whole thing to be told nothing. An unsized
                    // file has no such shortcut and finds its end by running out of bytes.
                    if buf.is_empty() || f.size.is_some_and(|n| offset >= n) {
                        return Ok(0);
                    }
                    let held = self.serve_held(path, offset, buf);
                    if held > 0 {
                        return Ok(held);
                    }
                    // Unranged, which is what lets the source check the body against the length
                    // the listing promised — the check that catches a refused token answering
                    // 200 with a login page. It is also no more requests than a range: a window
                    // is larger than the file.
                    let (bytes, _) = self.source.fetch_file(&f, None).await?;
                    let n = copy_out(&bytes, buf, offset);
                    // Held like a window, because that is what it is: a file at or under the
                    // ceiling is one window that happens to cover the whole file. A kernel reads
                    // this in chunks of its own choosing, so without holding it the file is
                    // fetched once per chunk — the cost `windowed` exists to bound, paid in full
                    // by the files small enough to skip it.
                    self.cache.lock().unwrap().window = Some((segments(path), 0, bytes));
                    Ok(n)
                }

                // Larger than the ceiling *and* measured, which the guard above leaves.
                Node::File(f) => {
                    let size = f.size.expect("the arm above took every unsized file");
                    self.windowed(path, &f, size, buf, offset).await
                }
            }
        })
    }
}

impl<S: MessengerSource> MessengerFs<S> {
    /// Listings for the directories inside a month.
    ///
    /// Three shapes and no recursion: the month itself, its `threads/`, its `files/`. A thread
    /// is a file rather than a directory, so there is nothing below it to list.
    async fn list_dir(&self, segs: &[String]) -> io::Result<Vec<Dirent>> {
        let (Some(s), Some(cd), Some(y), Some(m)) =
            (segs.first(), segs.get(1), segs.get(2), segs.get(3))
        else {
            return Err(io_err(io::ErrorKind::NotFound));
        };
        let conv = self.conv_of(s, cd).await?;
        let month = self.month_if_present(&conv, y, m).await?;

        match &segs[4..] {
            // A file per day that has anything, plus whichever of the two directories is
            // non-empty. A day nothing was said on is not a name here — the month was fetched,
            // so this is knowledge rather than a guess, and a name with nothing behind it is
            // what `grep -r` would open for no reason.
            [] => {
                let mut out: Vec<Dirent> = month
                    .text
                    .keys()
                    .map(|n| Dirent::new(n.clone(), DirentKind::File))
                    .collect();
                if !month.threads.is_empty() {
                    out.push(Dirent::new(THREADS, DirentKind::Dir));
                }
                if !month.files.is_empty() {
                    out.push(Dirent::new(FILES, DirentKind::Dir));
                }
                Ok(out)
            }

            [t] if t == THREADS => Ok(month
                .threads
                .iter()
                .map(|r| Dirent::new(thread_file(&r.0), DirentKind::File))
                .collect()),

            // Sizes come from the listing that named these, so they cost nothing to include —
            // which saves a caller an `lstat` per entry that a plain `ls` never asked for.
            [f] if f == FILES => Ok(month
                .files
                .iter()
                .map(|f| {
                    let name = file_entry(&f.name, &f.id);
                    // Sizes come from the listing that named these, so including them costs
                    // nothing — which saves a caller an `lstat` per entry that a plain `ls`
                    // never asked for. A platform that did not say leaves the field empty
                    // rather than a number nobody measured.
                    match f.size {
                        Some(size) => {
                            let mut st = Stat::new(DirentKind::File, size);
                            st.mtime = Some(self.born);
                            Dirent::with_stat(name, st)
                        }
                        None => Dirent::new(name, DirentKind::File),
                    }
                })
                .collect()),

            _ => Err(io_err(io::ErrorKind::NotFound)),
        }
    }
}

impl<S: MessengerSource> MessengerFs<S> {
    /// An attachment past [`window`](MessengerLimits::window), served from the one window this
    /// store holds.
    ///
    /// A request the held window only partly covers must not stop there: the trait states that a
    /// short read is EOF, so a document would appear to end at a window boundary. Windows begin
    /// where a miss was rather than on a grid, so that seam lands mid-request whenever the first
    /// access was not at zero — a PDF reader opening at the trailer is the ordinary case.
    async fn windowed(
        &self,
        path: &Path,
        f: &FileRef,
        size: u64,
        buf: &mut [u8],
        offset: u64,
    ) -> io::Result<usize> {
        if buf.is_empty() || offset >= size {
            return Ok(0);
        }
        // Never past the end the listing named: a range beyond it is a request the source would
        // refuse, and refusing here costs no round trip.
        let want = (buf.len() as u64).min(size - offset) as usize;
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
            // wrong is bounded by the window on a file already past it.
            let end = (at + self.limits.window).min(size);
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
            self.cache.lock().unwrap().window = Some((segments(path), start, data));
        }
        Ok(filled)
    }

    /// How long an attachment is, asking only when the listing did not say.
    ///
    /// A size is what `stat` has to answer with, and there is no honest placeholder: zero says
    /// the file is empty and every tool believes it. So an unsized file is fetched, and the
    /// bytes are held under this path — which makes the read that follows free, so a `stat`
    /// then a `cat` costs the one request it would have cost anyway.
    ///
    /// The consequence is that `ls -l` over unsized attachments spends a request each, where a
    /// plain `ls` spends none: the listing leaves [`Dirent::stat`] empty rather than inventing
    /// a number, which is what that field being an `Option` is for.
    async fn measure(&self, path: &Path, f: &FileRef) -> io::Result<u64> {
        if let Some(n) = f.size {
            return Ok(n);
        }
        if let Some((held, _, bytes)) = self.cache.lock().unwrap().window.as_ref()
            && *held == segments(path)
        {
            return Ok(bytes.len() as u64);
        }
        let (bytes, _) = self.source.fetch_file(f, None).await?;
        let n = bytes.len() as u64;
        self.cache.lock().unwrap().window = Some((segments(path), 0, bytes));
        Ok(n)
    }

    /// Serve as much of `out` as the held window covers *for this path*, returning how much that
    /// was. Zero means it does not — either a different file is held, or this offset is outside
    /// the bytes that are.
    fn serve_held(&self, path: &Path, at: u64, out: &mut [u8]) -> usize {
        let held = self.cache.lock().unwrap();
        let Some((held_path, start, data)) = held.window.as_ref() else {
            return 0;
        };
        if *held_path != segments(path) || at < *start || at >= *start + data.len() as u64 {
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

/// The year `seg` names, or `None` when it is not the four-digit name a listing writes.
fn year_of(seg: &str) -> Option<i32> {
    // Digits only, for the reason `month_of` gives: `parse` takes a sign, and `+026` would be
    // a second spelling of a year.
    (seg.len() == 4 && seg.bytes().all(|b| b.is_ascii_digit()))
        .then(|| seg.parse().ok())
        .flatten()
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

/// The half-open span one day file stands for.
fn window_of_day(date: NaiveDate) -> Window {
    let at = |d: NaiveDate| {
        Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight exists"))
            .into()
    };
    Window {
        start: at(date),
        end: at(date.succ_opt().expect("a date has a successor")),
    }
}

/// The half-open span a month directory stands for.
fn window_of(year: i32, month: u32) -> Window {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("a month has a first");
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .expect("a month has a next");
    let at = |d: NaiveDate| {
        Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight exists"))
            .into()
    };
    Window {
        start: at(first),
        end: at(next),
    }
}

#[cfg(test)]
#[path = "messenger_tests.rs"]
mod messenger_tests;
