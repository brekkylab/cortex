//! A read-oriented [`FileSystem`] store over a Notion workspace.
//!
//! Notion pages/blocks are projected onto a filesystem tree:
//! ```text
//! /pages/<title>__<page-id>/page.json         — metadata + markdown body + raw blocks
//! /pages/<title>__<page-id>/<child>__<id>/    — nested child pages, recursively
//! /pages/<title>__<page-id>/<db>__db__<id>/   — a database in the page
//! /pages/.../<db>__db__<id>/database.json     — its schema and a row index
//! /pages/.../<db>__db__<id>/<row>__<id>/      — a row, which is a page like any other
//! ```
//! `/pages` lists only top-level (workspace) pages; the `<page-id>` is the part
//! after the last `__`. `page.json` is rendered on read.
//!
//! One path level per page, whatever the block depth: sub-pages inside a two-column layout's
//! `column`, two blocks below the page, still get a directory directly under it.
//!
//! A database row *is* a page (the API returns it as one, with properties and a block body),
//! so a row directory is a page directory and the recursion continues inside it. Only the
//! database is a new kind of node, told apart by [`DB_MARKER`] in its directory name.
//!
//! Each operation `.await`s the async (reqwest) client directly; no runtime lives here. A
//! `page.json` is rendered once and cached, so a `stat` and the reads after it share one
//! render, which lets a guest kernel see the real size (no `direct_io` here).
//!
//! # Render cost
//!
//! A render is a `retrieve` plus a walk of the block tree: one request per block with
//! children, to [`MAX_BLOCK_DEPTH`]. Measured from a desktop client, 0.4-2.5s per page,
//! charged to whichever operation asks first (usually the `stat` of `page.json`).
//!
//! So a render is served without asking Notion for [`FRESH`]. Past that, one `retrieve` checks
//! whether the page was edited; an unchanged page keeps its render, so only a changed page
//! pays the walk again.
//!
//! With [`NotionFs::with_cache_dir`] renders also outlive the process, since a restarted
//! client's reader reopens the same pages. Each entry carries the edit stamp it was built
//! from, so a restart costs one `retrieve` per page. Nothing is written without a directory,
//! and what is written is the page's own json, under whatever protection the caller gives it.
//!
//! Read-only: page/block writes and the domain command channel are not exposed. Every
//! mutating method keeps [`FileSystem`]'s `ReadOnlyFilesystem` default rather than answering
//! `Unsupported`, so one read-only source does not disable writes for a whole mount.

use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};

use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, Stat},
};

const API: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";
/// Recursion ceiling for the block tree.
const MAX_BLOCK_DEPTH: usize = 10;
/// Waits before the 1st and 2nd retry of a rate-limited/5xx request.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];
/// Upper bound on a single retry wait, so a large `Retry-After` can't wedge an op.
const MAX_BACKOFF: Duration = Duration::from_secs(3);
/// How long a render is served without asking Notion anything.
///
/// Short, since nothing here learns of an edit made in a browser. Past it the render is
/// revalidated, not dropped, so looking again costs one `retrieve`, not a block walk.
const FRESH: Duration = Duration::from_secs(15);

/// How long a *listing* is served from what is kept, without asking Notion anything.
///
/// Longer than [`FRESH`]: a stale name costs a click that then revalidates, but a stale size
/// or body shows the wrong file. So the tree may lag a minute; content may not lag at all.
const LISTING_TTL: Duration = Duration::from_secs(60);

/// The top-level listing's file. Not an id, so the sweep leaves it alone.
const ROOTS_FILE: &str = "_roots.json";

/// How many renders are kept on disk, when there is a directory for them.
const DISK_CAP: usize = 1000;

/// Writes between sweeps of the directory, so the sweep's `read_dir` is not paid per write.
const SWEEP_EVERY: u32 = 64;

/// How many renders are kept.
///
/// A count bound rather than a TTL sweep: a reader walking a large workspace touches every
/// page once, and nothing else would ever drop an entry.
const CACHE_CAP: usize = 256;

/// Connection settings for a [`NotionFs`].
///
/// `Debug` redacts the key, since a log is not where a caller keeps its configuration.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotionConfig {
    pub api_key: String,
}

impl std::fmt::Debug for NotionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConfig")
            .field("api_key", &"[redacted]")
            .finish()
    }
}

/// A rendered `page.json`: its bytes, the page's timestamps, and the sub-page
/// directories that go beside it.
///
/// Directories and bytes come from the same tree, so they cannot disagree: a `child_page`
/// block renders as a marker because its directory carries the content, so that directory
/// must exist for exactly the blocks marked.
#[derive(Clone)]
struct Rendered {
    bytes: Arc<Vec<u8>>,
    /// `<sanitized-title>__<id>` per `child_page` block, at whatever depth it sat.
    child_dirs: Arc<Vec<String>>,
    mtime: Option<SystemTime>,
    ctime: Option<SystemTime>,
}

impl Rendered {
    fn stat(&self) -> Stat {
        let mut st = Stat::new(DirentKind::File, self.bytes.len() as u64);
        st.mtime = self.mtime;
        st.ctime = self.ctime;
        st
    }
}

/// One render, and what decides what it may still answer.
#[derive(Clone)]
struct Kept {
    /// The `forget` epoch this was admitted under; anything from an earlier one no longer
    /// answers.
    epoch: u64,
    /// When Notion last confirmed this render. `None` for one read from disk and unchecked
    /// this run: it may answer a *listing*, never content.
    checked: Option<Instant>,
    /// When this render was last put in place, confirmed or not.
    seen: Instant,
    rendered: Rendered,
}

/// The renders being kept, and the order to drop them in.
#[derive(Default)]
struct Renders {
    by_id: HashMap<String, Kept>,
    /// Ids in first-rendered order, oldest dropped first at the cap. Not LRU: a reader walks a
    /// tree once, so insertion order is visit order, and a use index would cost more than it
    /// saves.
    order: VecDeque<String>,
}

impl Renders {
    /// What is kept for `id` under `epoch`, as a copy, so no caller holds the lock past this
    /// and re-enters it by accident.
    fn get(&self, id: &str, epoch: u64) -> Option<Kept> {
        self.by_id.get(id).filter(|k| k.epoch == epoch).cloned()
    }

    /// Record that Notion confirmed this render unchanged; both windows restart, as for a
    /// fresh render.
    fn confirm(&mut self, id: &str) {
        if let Some(kept) = self.by_id.get_mut(id) {
            let now = Instant::now();
            kept.checked = Some(now);
            kept.seen = now;
        }
    }

    fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
    }

    /// Keep `rendered`. `checked` is false for one read from disk: see [`Kept::checked`].
    fn admit(&mut self, id: String, rendered: Rendered, checked: bool, epoch: u64) {
        let kept = Kept {
            epoch,
            checked: checked.then(Instant::now),
            seen: Instant::now(),
            rendered,
        };
        if self.by_id.insert(id.clone(), kept).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.by_id.remove(&oldest);
            }
        }
    }
}

/// A render, as it is kept between runs.
///
/// Times are epoch milliseconds, not Notion's strings: what is compared is the render's
/// `SystemTime`, and a format is one more thing to agree about. `bytes` is the rendered json,
/// UTF-8 since `serde_json` produced it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Stored {
    /// The page's `last_edited_time`; required, since without it an entry could never be
    /// revalidated.
    edited_ms: u64,
    created_ms: Option<u64>,
    child_dirs: Vec<String>,
    bytes: String,
}

/// Directory names as dirents.
fn dirs_of(names: &[String]) -> Vec<Dirent> {
    names
        .iter()
        .map(|name| Dirent::new(name.clone(), DirentKind::Dir))
        .collect()
}

fn epoch_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn at_ms(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

/// The workspace's top-level page directories, and when they were last read.
///
/// Plain TTL, not revalidated: it comes from `search`, with no page to retrieve and no edit
/// stamp to compare.
struct Roots {
    seen: Instant,
    names: Arc<Vec<String>>,
}

/// A Notion workspace's pages, served as a read-only tree; the module doc has the layout.
pub struct NotionFs {
    client: reqwest::Client,
    api_key: String,
    renders: Mutex<Renders>,
    roots: Mutex<Option<Roots>>,
    /// Refreshes asked for; nothing kept from before the last one answers again.
    epoch: std::sync::atomic::AtomicU64,
    /// Where renders outlive the process; see [`NotionFs::with_cache_dir`].
    cache_dir: Option<PathBuf>,
    /// Writes since the directory was last swept.
    writes: Mutex<u32>,
}

impl NotionFs {
    pub fn new(cfg: &NotionConfig) -> io::Result<Self> {
        // A hung upstream call would otherwise wedge the FUSE op, and any process touching
        // the mount, indefinitely.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            api_key: cfg.api_key.clone(),
            renders: Mutex::new(Renders::default()),
            roots: Mutex::new(None),
            epoch: std::sync::atomic::AtomicU64::new(0),
            cache_dir: None,
            writes: Mutex::new(0),
        })
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ask the source again for everything.
    ///
    /// For changes nothing here can see, such as a deleted page leaving its parent's listing
    /// (served from a render) wrong. Nothing kept before this answers again: not renders in
    /// memory or on disk, nor the top-level listing, whose file is removed. This fetches
    /// nothing; pages are re-rendered as they are visited.
    fn forget_kept(&self) {
        self.epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.roots.lock().unwrap() = None;
        self.renders.lock().unwrap().clear();
        if let Some(dir) = self.cache_dir.as_ref() {
            let _ = std::fs::remove_file(dir.join(ROOTS_FILE));
        }
    }

    /// Keep renders in `dir`, so they survive this process.
    ///
    /// Not part of [`NotionConfig`]: a cache location is a fact about the host, not the
    /// connection, and a config carrying a path would carry it to machines without it. The
    /// directory is created on first write and holds each page's rendered json, so it
    /// deserves whatever protection the pages do.
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    /// The render kept for `id` between runs, if there is one and it parses.
    ///
    /// Every failure is a miss: an unreadable cache means re-rendering, never an error.
    fn kept_on_disk(&self, id: &str) -> Option<Rendered> {
        let path = self.entry_path(id)?;
        let stored: Stored = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        Some(Rendered {
            bytes: Arc::new(stored.bytes.into_bytes()),
            child_dirs: Arc::new(stored.child_dirs),
            mtime: Some(at_ms(stored.edited_ms)),
            ctime: stored.created_ms.map(at_ms),
        })
    }

    /// Keep `rendered` for the next run. One with no edit stamp is skipped, since nothing
    /// could decide whether it is current.
    fn keep_on_disk(&self, id: &str, rendered: &Rendered) {
        let (Some(path), Some(edited_ms)) =
            (self.entry_path(id), rendered.mtime.and_then(epoch_ms))
        else {
            return;
        };
        let Ok(bytes) = String::from_utf8(rendered.bytes.as_ref().clone()) else {
            return;
        };
        let stored = Stored {
            edited_ms,
            created_ms: rendered.ctime.and_then(epoch_ms),
            child_dirs: rendered.child_dirs.as_ref().clone(),
            bytes,
        };
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        // Written to a temporary and renamed: a half-written entry could still parse as valid,
        // truncated json and be taken for the page.
        let tmp = path.with_extension("tmp");
        if serde_json::to_vec(&stored)
            .ok()
            .and_then(|v| std::fs::write(&tmp, v).ok())
            .is_some()
            && std::fs::rename(&tmp, &path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
        self.sweep(dir);
    }

    /// The top-level directories kept from an earlier run, whatever their age.
    ///
    /// Age is ignored so the tree paints after a restart without a request; [`LISTING_TTL`]
    /// later the next listing asks Notion again and picks up new pages.
    fn roots_on_disk(&self) -> Option<Vec<String>> {
        let path = self.cache_dir.as_ref()?.join(ROOTS_FILE);
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    fn keep_roots_on_disk(&self, names: &[String]) {
        let Some(dir) = self.cache_dir.as_ref() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = dir.join(ROOTS_FILE);
        let tmp = path.with_extension("tmp");
        if serde_json::to_vec(names)
            .ok()
            .and_then(|v| std::fs::write(&tmp, v).ok())
            .is_some()
            && std::fs::rename(&tmp, &path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Where `id`'s entry lives, for an id that could be one.
    ///
    /// `valid_notion_id` ensures a uuid, so no separator or `..` can be smuggled into the name.
    fn entry_path(&self, id: &str) -> Option<PathBuf> {
        let dir = self.cache_dir.as_ref()?;
        valid_notion_id(id).then(|| dir.join(format!("{id}.json")))
    }

    /// Drop the oldest entries when there are too many, every so many writes.
    fn sweep(&self, dir: &Path) {
        {
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            if !writes.is_multiple_of(SWEEP_EVERY) {
                return;
            }
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut found: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                // Pages only; the roots file is not the sweep's to drop.
                let path = e.path();
                if !valid_notion_id(path.file_stem()?.to_str()?) {
                    return None;
                }
                Some((e.metadata().ok()?.modified().ok()?, path))
            })
            .collect();
        if found.len() <= DISK_CAP {
            return;
        }
        found.sort_by_key(|(at, _)| *at);
        for (_, path) in found.iter().take(found.len() - DISK_CAP) {
            let _ = std::fs::remove_file(path);
        }
    }

    // ---- Notion API client (async) ------------------------------------------

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Bearer {}", self.api_key))
            .header("Notion-Version", NOTION_VERSION)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> io::Result<Value> {
        // `0..=len`: the extra last attempt has no backoff, and the index gates it, so
        // `.iter()` cannot express this loop.
        #[allow(clippy::needless_range_loop)]
        for attempt in 0..=RETRY_BACKOFF.len() {
            let Some(this) = req.try_clone() else {
                return finish(self.authed(req).send().await.map_err(io_other)?).await;
            };
            let resp = self.authed(this).send().await.map_err(io_other)?;
            let status = resp.status();
            let retryable =
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if retryable && attempt < RETRY_BACKOFF.len() {
                let wait = retry_after(&resp)
                    .unwrap_or(RETRY_BACKOFF[attempt])
                    .min(MAX_BACKOFF);
                tokio::time::sleep(wait).await;
                continue;
            }
            return finish(resp).await;
        }
        unreachable!("the final attempt returns instead of retrying")
    }

    /// Every page shared with the integration, via `search` filtered to pages.
    async fn search_pages(&self) -> io::Result<Vec<Value>> {
        let mut results = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = json!({
                "filter": {"property": "object", "value": "page"},
                "page_size": 100,
            });
            if let Some(c) = &cursor {
                body["start_cursor"] = json!(c);
            }
            let v = self
                .send(self.client.post(format!("{API}/search")).json(&body))
                .await?;
            if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
                results.extend(arr.iter().cloned());
            }
            if !v.get("has_more").and_then(|h| h.as_bool()).unwrap_or(false) {
                break;
            }
            match v.get("next_cursor").and_then(|c| c.as_str()) {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        Ok(results)
    }

    async fn get_page(&self, id: &str) -> io::Result<Value> {
        if !valid_notion_id(id) {
            return Err(io::ErrorKind::NotFound.into());
        }
        self.send(self.client.get(format!("{API}/pages/{id}")))
            .await
    }

    async fn get_database(&self, id: &str) -> io::Result<Value> {
        if !valid_notion_id(id) {
            return Err(io::ErrorKind::NotFound.into());
        }
        self.send(self.client.get(format!("{API}/databases/{id}")))
            .await
    }

    /// Every row of a database, paging through the query.
    ///
    /// A row is a full page object: its `properties` are the row, and its blocks are read
    /// through its own directory. Notion omits a database's *templates* here though they are
    /// parented to it, so the tree shows rows only.
    async fn query_database(&self, id: &str) -> io::Result<Vec<Value>> {
        if !valid_notion_id(id) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let mut results = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = json!({ "page_size": 100 });
            if let Some(c) = &cursor {
                body["start_cursor"] = json!(c);
            }
            let v = self
                .send(
                    self.client
                        .post(format!("{API}/databases/{id}/query"))
                        .json(&body),
                )
                .await?;
            if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
                results.extend(arr.iter().cloned());
            }
            if !v.get("has_more").and_then(|h| h.as_bool()).unwrap_or(false) {
                break;
            }
            match v.get("next_cursor").and_then(|c| c.as_str()) {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        Ok(results)
    }

    /// All immediate block children of `id`, paging through every result.
    async fn list_children(&self, id: &str) -> io::Result<Vec<Value>> {
        if !valid_notion_id(id) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let mut results = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            // `.query()` handles escaping.
            let mut params: Vec<(&str, String)> = vec![("page_size", "100".to_string())];
            if let Some(c) = &cursor {
                params.push(("start_cursor", c.clone()));
            }
            let v = self
                .send(
                    self.client
                        .get(format!("{API}/blocks/{id}/children"))
                        .query(&params),
                )
                .await?;
            if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
                results.extend(arr.iter().cloned());
            }
            if !v.get("has_more").and_then(|h| h.as_bool()).unwrap_or(false) {
                break;
            }
            match v.get("next_cursor").and_then(|c| c.as_str()) {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        Ok(results)
    }

    /// Block children recursively, nested under a `children` key, to [`MAX_BLOCK_DEPTH`].
    /// `child_page` and `child_database` are not descended into: their own directories
    /// serve their contents.
    fn list_block_tree<'a>(
        &'a self,
        id: String,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Vec<Value>>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut blocks = self.list_children(&id).await?;
            if depth >= MAX_BLOCK_DEPTH {
                return Ok(blocks);
            }
            for block in &mut blocks {
                let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if btype == "child_page" || btype == "child_database" {
                    continue;
                }
                if block
                    .get("has_children")
                    .and_then(|h| h.as_bool())
                    .unwrap_or(false)
                {
                    let child_id = block
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let children = self.list_block_tree(child_id, depth + 1).await?;
                    block["children"] = Value::Array(children);
                }
            }
            Ok(blocks)
        })
    }

    // ---- Render + cache ------------------------------------------------------

    /// The rendered `page.json` for `page_id`.
    ///
    /// Inside [`FRESH`] nothing is asked; past it, one `retrieve` says whether the page was
    /// edited, and an unchanged page keeps its render. Only a changed page pays the block walk.
    async fn render_cached(&self, page_id: &str) -> io::Result<Rendered> {
        let epoch = self.epoch();
        let cached = self.renders.lock().unwrap().get(page_id, epoch);
        if let Some(kept) = &cached
            && kept.checked.is_some_and(|at| at.elapsed() < FRESH)
        {
            return Ok(kept.rendered.clone());
        }

        let page = self.get_page(page_id).await?;
        if let Some(kept) = &cached
            && still_current(&kept.rendered, &page)
        {
            self.renders.lock().unwrap().confirm(page_id);
            return Ok(kept.rendered.clone());
        }
        // A previous run's render is validated by the same retrieve, so a restart costs no
        // walk. Not after a refresh: the reader rejected kept renders, and removing a child
        // page need not move `last_edited_time`.
        if epoch == 0
            && let Some(rendered) = self.kept_on_disk(page_id)
            && still_current(&rendered, &page)
        {
            self.renders
                .lock()
                .unwrap()
                .admit(page_id.to_string(), rendered.clone(), true, epoch);
            return Ok(rendered);
        }

        let blocks = self.list_block_tree(page_id.to_string(), 0).await?;
        let mut child_dirs = Vec::new();
        collect_child_dirs(&blocks, &mut child_dirs);
        let normalized = normalize_page(&page, &blocks);
        let bytes = serde_json::to_vec_pretty(&normalized).map_err(io_other)?;
        let rendered = Rendered {
            bytes: Arc::new(bytes),
            child_dirs: Arc::new(child_dirs),
            mtime: page_time(&page, "last_edited_time"),
            ctime: page_time(&page, "created_time"),
        };
        self.renders
            .lock()
            .unwrap()
            .admit(page_id.to_string(), rendered.clone(), true, epoch);
        self.keep_on_disk(page_id, &rendered);
        Ok(rendered)
    }

    /// The rendered `database.json` for `db_id`, in the page render cache: database and page
    /// ids are both UUIDs and never collide.
    ///
    /// Its `child_dirs` are the rows, so an `ls` of a database and a read of its
    /// `database.json` share one query.
    async fn render_database_cached(&self, db_id: &str) -> io::Result<Rendered> {
        let epoch = self.epoch();
        let cached = self.renders.lock().unwrap().get(db_id, epoch);
        if let Some(kept) = &cached
            && kept.checked.is_some_and(|at| at.elapsed() < FRESH)
        {
            return Ok(kept.rendered.clone());
        }

        let db = self.get_database(db_id).await?;
        // The retrieve is cheap; the query below grows with the number of rows.
        if let Some(kept) = &cached
            && still_current(&kept.rendered, &db)
        {
            self.renders.lock().unwrap().confirm(db_id);
            return Ok(kept.rendered.clone());
        }
        if epoch == 0
            && let Some(rendered) = self.kept_on_disk(db_id)
            && still_current(&rendered, &db)
        {
            self.renders
                .lock()
                .unwrap()
                .admit(db_id.to_string(), rendered.clone(), true, epoch);
            return Ok(rendered);
        }
        let rows = self.query_database(db_id).await?;
        let child_dirs: Vec<String> = rows.iter().map(page_dirname).collect();
        let bytes = serde_json::to_vec_pretty(&normalize_database(&db, &rows, &child_dirs))
            .map_err(io_other)?;
        let rendered = Rendered {
            bytes: Arc::new(bytes),
            child_dirs: Arc::new(child_dirs),
            mtime: page_time(&db, "last_edited_time"),
            ctime: page_time(&db, "created_time"),
        };
        self.renders
            .lock()
            .unwrap()
            .admit(db_id.to_string(), rendered.clone(), true, epoch);
        self.keep_on_disk(db_id, &rendered);
        Ok(rendered)
    }

    /// Contents of a database dir: `database.json` plus a subdir per row.
    async fn database_dir_entries(&self, db_id: &str) -> io::Result<Vec<Dirent>> {
        let names = match self.kept_child_dirs(db_id) {
            Some(names) => names,
            None => self.render_database_cached(db_id).await?.child_dirs,
        };
        let mut out = vec![Dirent::new("database.json", DirentKind::File)];
        out.extend(dirs_of(&names));
        Ok(out)
    }

    /// The render behind a `<dir>/<file>.json` path.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) when file name and directory kind disagree
    /// (`database.json` in a page directory, `page.json` in a database's). Shared by `stat`
    /// and `read_at` so they cannot disagree.
    async fn render_for_file(&self, rest: &[String]) -> io::Result<Rendered> {
        // `/pages/page.json` has no enclosing directory.
        let [.., dir, file] = rest else {
            return Err(io::ErrorKind::NotFound.into());
        };
        match (file.as_str(), node(dir)) {
            ("page.json", Node::Page(id)) => self.render_cached(&id).await,
            ("database.json", Node::Database(id)) => self.render_database_cached(&id).await,
            _ => Err(io::ErrorKind::NotFound.into()),
        }
    }

    /// Top-level (workspace) pages as `<title>__<id>` dir entries.
    ///
    /// Kept for [`LISTING_TTL`], and read back from disk at any age: the paged `search` stands
    /// between a reader and the first thing they see.
    async fn top_level_page_dirs(&self) -> io::Result<Vec<Dirent>> {
        if let Some(roots) = self.roots.lock().unwrap().as_ref()
            && roots.seen.elapsed() < LISTING_TTL
        {
            return Ok(dirs_of(&roots.names));
        }
        if self.epoch() == 0
            && self.roots.lock().unwrap().is_none()
            && let Some(names) = self.roots_on_disk()
        {
            let names = Arc::new(names);
            *self.roots.lock().unwrap() = Some(Roots {
                seen: Instant::now(),
                names: names.clone(),
            });
            return Ok(dirs_of(&names));
        }
        let pages = self.search_pages().await?;
        let names: Vec<String> = pages
            .iter()
            .filter(|p| {
                p.get("parent")
                    .and_then(|x| x.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("workspace")
            })
            .map(page_dirname)
            .collect();
        self.keep_roots_on_disk(&names);
        let names = Arc::new(names);
        *self.roots.lock().unwrap() = Some(Roots {
            seen: Instant::now(),
            names: names.clone(),
        });
        Ok(dirs_of(&names))
    }

    /// The names a listing of `id` shows, from what is kept, with no request.
    ///
    /// An unconfirmed render from disk still answers a listing: opening the content goes
    /// through [`Self::render_cached`], which confirms. See [`LISTING_TTL`].
    fn kept_child_dirs(&self, id: &str) -> Option<Arc<Vec<String>>> {
        let epoch = self.epoch();
        if let Some(kept) = self.renders.lock().unwrap().get(id, epoch)
            && kept.seen.elapsed() < LISTING_TTL
        {
            return Some(kept.rendered.child_dirs.clone());
        }
        // Not after a refresh, the reader's only way to report a deleted page.
        if epoch != 0 {
            return None;
        }
        let rendered = self.kept_on_disk(id)?;
        let names = rendered.child_dirs.clone();
        self.renders
            .lock()
            .unwrap()
            .admit(id.to_string(), rendered, false, epoch);
        Some(names)
    }

    /// Contents of a page dir: `page.json` plus a subdir per `child_page` block.
    ///
    /// From the render, not one level of blocks: a child page sits wherever the layout puts
    /// it (a two-column page's immediate children are columns). An `ls` and the read after it
    /// share the cached render.
    async fn page_dir_entries(&self, page_id: &str) -> io::Result<Vec<Dirent>> {
        let names = match self.kept_child_dirs(page_id) {
            Some(names) => names,
            None => self.render_cached(page_id).await?.child_dirs,
        };
        let mut out = vec![Dirent::new("page.json", DirentKind::File)];
        out.extend(dirs_of(&names));
        Ok(out)
    }
}

/// Every mutating method keeps the trait's `ReadOnlyFilesystem` default, so a writer hears
/// it on the write; there is no open to hear it on.
impl FileSystem for NotionFs {
    fn forget<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.forget_kept() })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let segs = segments(path);
            match segs.as_slice() {
                [] => Ok(Stat::new(DirentKind::Dir, 0)),
                [p] if p == "pages" => Ok(Stat::new(DirentKind::Dir, 0)),
                [p, rest @ ..] if p == "pages" && !rest.is_empty() => {
                    let last = rest.last().unwrap().as_str();
                    if matches!(last, "page.json" | "database.json") {
                        // Render so the guest kernel sees the real size (no direct_io).
                        return Ok(self.render_for_file(rest).await?.stat());
                    }
                    // A fresh render carries this directory's times, and one usually exists
                    // since the reader just listed the parent.
                    let (Node::Page(id) | Node::Database(id)) = node(last);
                    if let Some(kept) = self.renders.lock().unwrap().get(&id, self.epoch())
                        && kept.checked.is_some_and(|at| at.elapsed() < FRESH)
                    {
                        let mut st = Stat::new(DirentKind::Dir, 0);
                        st.mtime = kept.rendered.mtime;
                        st.ctime = kept.rendered.ctime;
                        return Ok(st);
                    }
                    // Otherwise one retrieve confirms it exists and gives its times, where a
                    // listing would be a whole render.
                    let obj = match node(last) {
                        Node::Page(id) => self.get_page(&id).await?,
                        Node::Database(id) => self.get_database(&id).await?,
                    };
                    let mut st = Stat::new(DirentKind::Dir, 0);
                    st.mtime = page_time(&obj, "last_edited_time");
                    st.ctime = page_time(&obj, "created_time");
                    Ok(st)
                }
                _ => Err(io::ErrorKind::NotFound.into()),
            }
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let segs = segments(path);
            match segs.as_slice() {
                [] => Ok(vec![Dirent::new("pages", DirentKind::Dir)]),
                [p] if p == "pages" => self.top_level_page_dirs().await,
                [p, rest @ ..] if p == "pages" && !rest.is_empty() => {
                    let last = rest.last().unwrap();
                    if matches!(last.as_str(), "page.json" | "database.json") {
                        return Err(io::ErrorKind::NotADirectory.into());
                    }
                    match node(last) {
                        Node::Page(id) => self.page_dir_entries(&id).await,
                        Node::Database(id) => self.database_dir_entries(&id).await,
                    }
                }
                _ => Err(io::ErrorKind::NotFound.into()),
            }
        })
    }

    /// Served from the render cache: the [`stat`](Self::stat) that reported the size produced
    /// the bytes, and they are usually still within `FRESH` when the reads arrive.
    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            let segs = segments(path);
            // `page.json` and `database.json` are the only files; anything else is a directory.
            if segs.len() < 3
                || segs[0] != "pages"
                || !matches!(
                    segs.last().map(String::as_str),
                    Some("page.json" | "database.json")
                )
            {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            let data = self.render_for_file(&segs[1..]).await?.bytes;
            if offset >= data.len() as u64 {
                return Ok(0);
            }
            let from = offset as usize;
            let n = (data.len() - from).min(buf.len());
            buf[..n].copy_from_slice(&data[from..from + n]);
            Ok(n)
        })
    }
}

// ---- helpers ----------------------------------------------------------------

fn io_other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Turn a finished response into JSON, or map a non-2xx status.
async fn finish(resp: reqwest::Response) -> io::Result<Value> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(io::ErrorKind::NotFound.into());
        }
        return Err(io_other(format!("notion API {status}: {body}")));
    }
    // An unparsable 2xx is a broken response; surfaced so field defaults do not render a
    // silently blank page.json.
    serde_json::from_str(&body).map_err(io_other)
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Reject non-UUID ids before they reach a request URL.
fn valid_notion_id(s: &str) -> bool {
    matches!(s.len(), 32 | 36) && uuid::Uuid::try_parse(s).is_ok()
}

fn segments(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// What a directory segment in the tree names.
///
/// A Notion id does not say what it identifies, so the name carries it via [`DB_MARKER`].
/// That keeps a path resolvable on its own, with no ancestor fetched: [`FileSystem`]
/// addresses by path and passes no parent.
enum Node {
    Page(String),
    Database(String),
}

/// Marks a database directory: `<title>__db__<database-id>`.
///
/// Unambiguous: [`sanitize_name`] folds runs of `_`, so a title never contains `__`.
const DB_MARKER: &str = "__db__";

fn node(dir_name: &str) -> Node {
    match dir_name.rsplit_once(DB_MARKER) {
        Some((_, id)) => Node::Database(id.to_string()),
        None => Node::Page(page_id(dir_name)),
    }
}

/// The part of a directory name after the last `__`.
fn page_id(dir_name: &str) -> String {
    dir_name
        .rsplit_once("__")
        .map(|(_, id)| id)
        .unwrap_or(dir_name)
        .to_string()
}

/// Every `child_page` and `child_database` block in a tree, at whatever depth, as
/// a directory name.
///
/// At any depth, since a two-column page's immediate children are columns. Neither kind has
/// `children`, as the walk never descends into them.
fn collect_child_dirs(blocks: &[Value], out: &mut Vec<String>) {
    for b in blocks {
        let btype = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if btype == "child_page" || btype == "child_database" {
            let title = child_title(b.get(btype).unwrap_or(&Value::Null));
            let id = b.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let sep = if btype == "child_database" {
                DB_MARKER
            } else {
                "__"
            };
            out.push(format!("{}{sep}{id}", sanitize_name(&title)));
            continue;
        }
        if let Some(kids) = b.get("children").and_then(|c| c.as_array()) {
            collect_child_dirs(kids, out);
        }
    }
}

/// The `title` a `child_page`/`child_database` block's payload carries.
fn child_title(content: &Value) -> String {
    content
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("untitled")
        .to_string()
}

/// Directory name for a page: `<sanitized-title>__<id>`.
fn page_dirname(page: &Value) -> String {
    let title = extract_title(page);
    let id = page.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let label = if title.is_empty() {
        "untitled".to_string()
    } else {
        sanitize_name(&title)
    };
    format!("{label}__{id}")
}

fn extract_title(page: &Value) -> String {
    let props = match page.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return String::new(),
    };
    for prop in props.values() {
        if prop.get("type").and_then(|t| t.as_str()) == Some("title") {
            return prop
                .get("title")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.get("plain_text").and_then(|p| p.as_str()))
                        .collect::<String>()
                })
                .unwrap_or_default();
        }
    }
    String::new()
}

fn rfc3339_to_systemtime(s: &str) -> Option<SystemTime> {
    let secs = chrono::DateTime::parse_from_rfc3339(s).ok()?.timestamp();
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Whether `rendered` still describes `object`.
///
/// Notion has no etag; `last_edited_time` moves with a page's blocks. An object without one is
/// never unchanged: two absent times comparing equal would keep a stale render forever.
fn still_current(rendered: &Rendered, object: &Value) -> bool {
    let edited = page_time(object, "last_edited_time");
    edited.is_some() && rendered.mtime == edited
}

fn page_time(v: &Value, key: &str) -> Option<SystemTime> {
    v.get(key)
        .and_then(|x| x.as_str())
        .and_then(rfc3339_to_systemtime)
}

/// Page metadata + markdown body + raw blocks. `child_page`/`child_database`
/// blocks are excluded (they surface as subdirectories).
fn normalize_page(page: &Value, blocks: &[Value]) -> Value {
    let parent = page.get("parent").cloned().unwrap_or_else(|| json!({}));
    let parent_type = parent.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let parent_id = parent
        .get(parent_type)
        .and_then(|v| v.as_str())
        .unwrap_or("");
    json!({
        "page_id": page.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "title": extract_title(page),
        "icon": emoji_icon(page),
        "url": page.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "created_time": page.get("created_time").and_then(|v| v.as_str()).unwrap_or(""),
        "last_edited_time": page.get("last_edited_time").and_then(|v| v.as_str()).unwrap_or(""),
        "parent_type": parent_type,
        "parent_id": parent_id,
        "archived": page.get("archived").and_then(|v| v.as_bool()).unwrap_or(false),
        // Verbatim: for a database row this *is* the record, so a reader needs no second
        // shape per property type.
        "properties": page.get("properties").cloned().unwrap_or_else(|| json!({})),
        "markdown": blocks_to_markdown(blocks),
        "blocks": blocks,
    })
}

/// `database.json`: what the database is, its schema, and an index of its rows.
///
/// Each row carries its `dir`, so a reader need not rebuild the name, and its `icon`, so a
/// list of rows is distinguishable without a request per row. `properties` is Notion's schema,
/// verbatim.
fn normalize_database(db: &Value, rows: &[Value], dirs: &[String]) -> Value {
    let parent = db.get("parent").cloned().unwrap_or_else(|| json!({}));
    let parent_type = parent.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let parent_id = parent
        .get(parent_type)
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let title = db
        .get("title")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("plain_text").and_then(|p| p.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default();
    let rows: Vec<Value> = rows
        .iter()
        .zip(dirs)
        .map(|(r, dir)| {
            json!({
                "page_id": r.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "dir": dir,
                "icon": emoji_icon(r),
                "properties": r.get("properties").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect();
    json!({
        "database_id": db.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "title": title,
        "icon": emoji_icon(db),
        "url": db.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "is_inline": db.get("is_inline").and_then(|v| v.as_bool()).unwrap_or(false),
        "created_time": db.get("created_time").and_then(|v| v.as_str()).unwrap_or(""),
        "last_edited_time": db.get("last_edited_time").and_then(|v| v.as_str()).unwrap_or(""),
        "parent_type": parent_type,
        "parent_id": parent_id,
        "archived": db.get("archived").and_then(|v| v.as_bool()).unwrap_or(false),
        "properties": db.get("properties").cloned().unwrap_or_else(|| json!({})),
        "row_count": rows.len(),
        "rows": rows,
    })
}

/// A page or database's icon, when it is an emoji, and `null` otherwise.
///
/// Notion's `icon` is an emoji (`{"type":"emoji","emoji":"\u{1f4dd}"}`) or an
/// `external`/`file` image behind a URL. Only the emoji travels: a reader cannot fetch the
/// others (a `file` icon's signed URL expires), and an unrenderable icon is no better than none.
fn emoji_icon(obj: &Value) -> Option<&str> {
    let icon = obj.get("icon")?;
    if icon.get("type").and_then(|t| t.as_str()) != Some("emoji") {
        return None;
    }
    icon.get("emoji").and_then(|e| e.as_str())
}

/// Sanitize a name for a virtual path segment.
fn sanitize_name(name: &str) -> String {
    if name.trim().is_empty() {
        return "unknown".to_string();
    }
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c.is_whitespace() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.replace(' ', "_");
    let mut folded = String::with_capacity(cleaned.len());
    let mut prev_underscore = false;
    for c in cleaned.chars() {
        if c == '_' {
            if !prev_underscore {
                folded.push(c);
            }
            prev_underscore = true;
        } else {
            folded.push(c);
            prev_underscore = false;
        }
    }
    folded.trim_matches('_').chars().take(100).collect()
}

fn rich_text_to_md(list: &[Value]) -> String {
    let mut parts = String::new();
    for rt in list {
        let mut text = rt
            .get("plain_text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let flag = |k: &str| {
            rt.get("annotations")
                .and_then(|a| a.get(k))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        if flag("code") {
            text = format!("`{text}`");
        }
        if flag("bold") {
            text = format!("**{text}**");
        }
        if flag("italic") {
            text = format!("*{text}*");
        }
        if flag("strikethrough") {
            text = format!("~~{text}~~");
        }
        if let Some(href) = rt.get("href").and_then(|v| v.as_str())
            && !href.is_empty()
        {
            text = format!("[{text}]({href})");
        }
        parts.push_str(&text);
    }
    parts
}

fn block_to_md(block: &Value, indent: usize) -> String {
    let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let content = block.get(btype).cloned().unwrap_or_else(|| json!({}));
    let rich_text = content
        .get("rich_text")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let text = rich_text_to_md(&rich_text);
    let prefix = "  ".repeat(indent);

    match btype {
        "paragraph" => format!("{prefix}{text}"),
        "heading_1" => format!("# {text}"),
        "heading_2" => format!("## {text}"),
        "heading_3" => format!("### {text}"),
        "bulleted_list_item" => format!("{prefix}- {text}"),
        "numbered_list_item" => format!("{prefix}1. {text}"),
        "to_do" => {
            let checked = content
                .get("checked")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let marker = if checked { "x" } else { " " };
            format!("{prefix}- [{marker}] {text}")
        }
        "toggle" => format!("{prefix}<details><summary>{text}</summary></details>"),
        "code" => {
            let language = content
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("```{language}\n{text}\n```")
        }
        "quote" => format!("{prefix}> {text}"),
        "callout" => {
            let icon = content.get("icon");
            let emoji =
                if icon.and_then(|i| i.get("type")).and_then(|t| t.as_str()) == Some("emoji") {
                    icon.and_then(|i| i.get("emoji"))
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                } else {
                    ""
                };
            format!("{prefix}> {emoji} {text}")
        }
        "divider" => "---".to_string(),
        "image" => {
            let inner = content.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let img = content.get(inner).cloned().unwrap_or_else(|| json!({}));
            let url = img.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let caption = rich_text_to_md(
                &content
                    .get("caption")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default(),
            );
            format!("![{caption}]({url})")
        }
        "bookmark" => {
            let url = content.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let caption = rich_text_to_md(
                &content
                    .get("caption")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default(),
            );
            let label = if caption.is_empty() {
                url.to_string()
            } else {
                caption
            };
            format!("[{label}]({url})")
        }
        "equation" => {
            let expr = content
                .get("expression")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("$${expr}$$")
        }
        "table_of_contents" => "[TOC]".to_string(),
        // The content lives in the child's own directory; the line marks where it went
        // rather than leaving a gap.
        "child_page" => format!("{prefix}[page: {}]", child_title(&content)),
        "child_database" => format!("{prefix}[database: {}]", child_title(&content)),
        _ => {
            if text.is_empty() {
                String::new()
            } else {
                format!("{prefix}{text}")
            }
        }
    }
}

fn walk_block(block: &Value, indent: usize, lines: &mut Vec<String>) {
    let line = block_to_md(block, indent);
    let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if !line.is_empty() || btype == "paragraph" {
        lines.push(line);
    }
    if let Some(children) = block.get("children").and_then(|c| c.as_array()) {
        for child in children {
            walk_block(child, indent + 1, lines);
        }
    }
}

fn blocks_to_markdown(blocks: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for b in blocks {
        walk_block(b, 0, &mut lines);
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A render carrying `edited` as the page's last edit, the only field the cache reads.
    fn rendered(edited: Option<&str>) -> Rendered {
        Rendered {
            bytes: Arc::new(b"{}".to_vec()),
            child_dirs: Arc::new(vec![]),
            mtime: edited.and_then(rfc3339_to_systemtime),
            ctime: None,
        }
    }

    #[test]
    fn a_render_is_kept_only_while_the_page_says_it_has_not_moved() {
        let have = rendered(Some("2026-09-22T05:00:00.000Z"));

        assert!(still_current(
            &have,
            &json!({ "last_edited_time": "2026-09-22T05:00:00.000Z" })
        ));
        assert!(!still_current(
            &have,
            &json!({ "last_edited_time": "2026-09-22T06:00:00.000Z" })
        ));

        // Absent stamps are not agreement, or a stale render would be kept forever.
        assert!(!still_current(&have, &json!({})));
        assert!(!still_current(&rendered(None), &json!({})));
        assert!(!still_current(
            &rendered(None),
            &json!({ "last_edited_time": "2026-09-22T05:00:00.000Z" })
        ));
    }

    #[test]
    fn the_cache_drops_the_oldest_render_rather_than_growing() {
        let mut renders = Renders::default();
        for i in 0..CACHE_CAP + 2 {
            renders.admit(format!("page-{i}"), rendered(None), true, 0);
        }
        assert_eq!(renders.by_id.len(), CACHE_CAP);
        assert!(
            renders.get("page-0", 0).is_none(),
            "the first one visited went"
        );
        assert!(renders.get("page-1", 0).is_none());
        assert!(renders.get(&format!("page-{}", CACHE_CAP + 1), 0).is_some());

        // Re-admitting replaces rather than queueing twice, or the order would drop live entries.
        let before = renders.order.len();
        renders.admit(format!("page-{}", CACHE_CAP + 1), rendered(None), true, 0);
        assert_eq!(renders.order.len(), before);
    }

    fn store(dir: Option<&Path>) -> NotionFs {
        let fs = NotionFs::new(&NotionConfig {
            api_key: "secret".into(),
        })
        .unwrap();
        match dir {
            Some(d) => fs.with_cache_dir(d),
            None => fs,
        }
    }

    const ID: &str = "38ac4175-a910-810a-b4b6-e1bda771cd38";

    #[test]
    fn a_render_kept_on_disk_comes_back_as_what_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(Some(dir.path()));
        let mut kept = rendered(Some("2026-09-22T05:00:00.000Z"));
        kept.bytes = Arc::new(br#"{"title":"a page"}"#.to_vec());
        kept.child_dirs = Arc::new(vec!["child__38ac4175a910810ab4b6e1bda771cd38".into()]);

        fs.keep_on_disk(ID, &kept);
        let back = fs.kept_on_disk(ID).expect("an entry was written");
        assert_eq!(back.bytes, kept.bytes);
        assert_eq!(back.child_dirs, kept.child_dirs);
        // The stamp is what the next run revalidates with.
        assert_eq!(back.mtime, kept.mtime);
        assert!(still_current(
            &back,
            &json!({ "last_edited_time": "2026-09-22T05:00:00.000Z" })
        ));
    }

    #[test]
    fn nothing_is_kept_that_could_never_be_revalidated() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(Some(dir.path()));

        // No edit stamp: the next run could never find out the entry is wrong.
        fs.keep_on_disk(ID, &rendered(None));
        assert!(fs.kept_on_disk(ID).is_none());

        // A non-Notion id is not a file name, so `..` never reaches the path.
        fs.keep_on_disk(
            "../../etc/passwd",
            &rendered(Some("2026-09-22T05:00:00.000Z")),
        );
        assert!(fs.kept_on_disk("../../etc/passwd").is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn without_a_directory_there_is_no_disk_to_read_or_write() {
        let fs = store(None);
        fs.keep_on_disk(ID, &rendered(Some("2026-09-22T05:00:00.000Z")));
        assert!(fs.kept_on_disk(ID).is_none());
    }

    #[test]
    fn a_listing_comes_from_what_is_kept_without_asking_notion() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(Some(dir.path()));
        let mut kept = rendered(Some("2026-09-22T05:00:00.000Z"));
        kept.child_dirs = Arc::new(vec!["child__38ac4175a910810ab4b6e1bda771cd38".into()]);
        fs.keep_on_disk(ID, &kept);

        // The token is fake, so any request to Notion would fail.
        let names = fs
            .kept_child_dirs(ID)
            .expect("the entry answers the listing");
        assert_eq!(*names, *kept.child_dirs);

        // Not confirmed in memory, so the next `stat` or read still asks Notion.
        let in_memory = fs.renders.lock().unwrap().get(ID, 0).unwrap();
        assert!(in_memory.checked.is_none());
    }

    #[test]
    fn a_refresh_stops_everything_kept_from_answering() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(Some(dir.path()));
        let mut kept = rendered(Some("2026-09-22T05:00:00.000Z"));
        kept.child_dirs = Arc::new(vec!["deleted__38ac4175a910810ab4b6e1bda771cd38".into()]);
        fs.keep_on_disk(ID, &kept);
        fs.keep_roots_on_disk(&["Engineering_Logs__490e8208".to_string()]);
        assert!(
            fs.kept_child_dirs(ID).is_some(),
            "kept, before the reader says otherwise"
        );

        // A reader's Refresh, e.g. after deleting a page a kept listing still shows.
        fs.forget_kept();

        assert!(
            fs.kept_child_dirs(ID).is_none(),
            "the listing has to be asked for again, not answered from the render that has it"
        );
        assert!(
            fs.roots_on_disk().is_none(),
            "and the top-level listing goes with it"
        );
        // Renders stay on disk (dropping them would cost every page a block walk), but the
        // moved epoch keeps them from answering without Notion.
        assert!(fs.kept_on_disk(ID).is_some());
        assert!(fs.renders.lock().unwrap().get(ID, fs.epoch()).is_none());
    }

    #[test]
    fn the_top_level_listing_survives_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(Some(dir.path()));
        assert!(fs.roots_on_disk().is_none(), "nothing kept yet");

        let names = vec!["Engineering_Logs__490e8208".to_string()];
        fs.keep_roots_on_disk(&names);
        assert_eq!(fs.roots_on_disk().unwrap(), names);

        assert!(store(None).roots_on_disk().is_none());
    }

    #[test]
    fn the_directory_stops_growing_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let fs = store(Some(dir.path()));
        // 32-hex names pass `valid_notion_id`, so the sweep treats them as pages.
        for i in 0..DISK_CAP + 5 {
            std::fs::write(dir.path().join(format!("{i:032x}.json")), b"{}").unwrap();
        }
        std::fs::write(dir.path().join(ROOTS_FILE), b"[]").unwrap();

        for _ in 0..SWEEP_EVERY - 1 {
            fs.sweep(dir.path());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), DISK_CAP + 6);
        fs.sweep(dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), DISK_CAP + 1);
        assert!(
            dir.path().join(ROOTS_FILE).exists(),
            "the listing is not a page"
        );
    }

    #[test]
    fn confirming_a_render_starts_its_window_again() {
        let mut renders = Renders::default();
        renders.admit("page".into(), rendered(None), true, 0);
        let first = renders.get("page", 0).unwrap().checked.unwrap();

        renders.confirm("page");
        let second = renders.get("page", 0).unwrap().checked.unwrap();
        assert!(
            second >= first,
            "a confirmed render is as good as a fresh one"
        );

        // One read from disk is unconfirmed: it answers listings, not content.
        renders.admit("from-disk".into(), rendered(None), false, 0);
        let kept = renders.get("from-disk", 0).unwrap();
        assert!(kept.checked.is_none());
        assert!(kept.seen.elapsed() < LISTING_TTL);

        // Confirming a missing id inserts nothing: there are no bytes to carry.
        renders.confirm("missing");
        assert!(renders.get("missing", 0).is_none());
    }

    fn child(btype: &str, title: &str, id: &str) -> Value {
        json!({ "type": btype, "id": id, btype: { "title": title } })
    }

    /// Only an emoji icon is usable by a reader, so only it is carried.
    #[test]
    fn an_emoji_icon_travels_and_an_image_does_not() {
        let page = json!({ "icon": { "type": "emoji", "emoji": "\u{1f4dd}" } });
        assert_eq!(emoji_icon(&page), Some("\u{1f4dd}"));

        let uploaded = json!({
            "icon": { "type": "file", "file": { "url": "https://example.invalid/i.png" } }
        });
        assert_eq!(emoji_icon(&uploaded), None);
        let external = json!({ "icon": { "type": "external", "external": { "url": "x" } } });
        assert_eq!(emoji_icon(&external), None);
    }

    /// No icon is `null`, not a missing key a reader would have to tell from a malformed one.
    #[test]
    fn a_page_without_an_icon_still_has_the_key() {
        let rendered = normalize_page(&json!({ "id": "abc" }), &[]);
        assert_eq!(rendered.get("icon"), Some(&Value::Null));

        let icon = json!({ "type": "emoji", "emoji": "\u{1f4c1}" });
        let rendered = normalize_page(&json!({ "id": "abc", "icon": icon }), &[]);
        assert_eq!(rendered["icon"], json!("\u{1f4c1}"));
    }

    /// A reader drawing the row index never opens the rows, so the index must carry icons.
    #[test]
    fn a_rows_icon_reaches_the_index_beside_its_dir() {
        let rows = [
            json!({ "id": "r1", "icon": { "type": "emoji", "emoji": "\u{1f41b}" } }),
            json!({ "id": "r2" }),
        ];
        let dirs = ["bug__r1".to_string(), "plain__r2".to_string()];
        let rendered = normalize_database(&json!({ "id": "db" }), &rows, &dirs);

        assert_eq!(rendered["rows"][0]["icon"], json!("\u{1f41b}"));
        assert_eq!(rendered["rows"][1].get("icon"), Some(&Value::Null));
    }

    /// The name alone tells a page directory from a database's, so no parent is fetched.
    #[test]
    fn a_directory_name_says_which_kind_it_is() {
        let id = "2fd2589f-40ea-8115-95e3-c4970d29590c";
        assert!(matches!(node(&format!("자료실__{id}")), Node::Page(x) if x == id));
        assert!(
            matches!(node(&format!("프로젝트_자료실__db__{id}")), Node::Database(x) if x == id)
        );
    }

    /// A title cannot forge the marker: `sanitize_name` folds runs of `_`, so no `__` survives.
    #[test]
    fn a_title_cannot_forge_the_marker() {
        for title in ["vector db", "a__db", "db", "__db__", "x  db  y"] {
            let clean = sanitize_name(title);
            assert!(!clean.contains("__"), "{title:?} sanitized to {clean:?}");
        }
        let id = "2fd2589f-40ea-8115-95e3-c4970d29590c";
        let dir = format!("{}__{id}", sanitize_name("vector db"));
        assert!(matches!(node(&dir), Node::Page(x) if x == id));
    }

    /// Both kinds are collected at any depth inside layout blocks.
    #[test]
    fn child_pages_and_databases_are_collected_at_depth() {
        let blocks = vec![json!({
            "type": "column_list",
            "id": "col-list",
            "children": [json!({
                "type": "column",
                "id": "col",
                "children": [
                    child("child_page", "회의록", "aaaaaaaa-0000-0000-0000-000000000001"),
                    child("child_database", "프로젝트 일정", "bbbbbbbb-0000-0000-0000-000000000002"),
                ],
            })],
        })];
        let mut out = Vec::new();
        collect_child_dirs(&blocks, &mut out);
        assert_eq!(
            out,
            [
                "회의록__aaaaaaaa-0000-0000-0000-000000000001",
                "프로젝트_일정__db__bbbbbbbb-0000-0000-0000-000000000002",
            ]
        );
        // Each round-trips to its kind.
        assert!(matches!(node(&out[0]), Node::Page(_)));
        assert!(matches!(node(&out[1]), Node::Database(_)));
    }

    /// A database below a `child_page` belongs to that page's directory, not this listing.
    #[test]
    fn a_collected_child_is_not_walked_through() {
        let blocks = vec![json!({
            "type": "child_page",
            "id": "aaaaaaaa-0000-0000-0000-000000000001",
            "child_page": { "title": "부모" },
            "children": [child("child_database", "안쪽", "bbbbbbbb-0000-0000-0000-000000000002")],
        })];
        let mut out = Vec::new();
        collect_child_dirs(&blocks, &mut out);
        assert_eq!(out.len(), 1, "only the child page itself: {out:?}");
    }
}
