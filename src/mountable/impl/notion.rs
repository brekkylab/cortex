//! A read-oriented [`Mountable`] backend over a Notion workspace.
//!
//! Notion pages/blocks are projected onto a filesystem tree:
//! ```text
//! /pages/<title>__<page-id>/page.json      — metadata + markdown body + raw blocks
//! /pages/<title>__<page-id>/<child>__<id>/ — nested child pages, recursively
//! ```
//! `/pages` lists only top-level (workspace) pages; the `<page-id>` is the part
//! after the last `__`. `page.json` is rendered on read.
//!
//! The Notion API is async (reqwest) and so is this backend — each `Mountable`
//! op `.await`s the client directly; no runtime lives here. A `page.json`'s
//! bytes are rendered once and cached briefly so a `stat` + `open` pair costs
//! one render, and both can report the real size a guest kernel needs (no
//! `direct_io` here).
//!
//! Read-only: page/block writes and the domain command channel are not exposed.
//! Writes surface as [`CortexError::ReadOnly`] (not `Unsupported`) so one
//! read-only source does not disable writes for a whole workspace mount.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use async_trait::async_trait;

use crate::mountable::{FileExt, FileHandle};
use crate::{CortexError, Dirent, DirentKind, Mountable, OpenOptions, Result, Stat};

const API: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";
/// Recursion ceiling for the block tree.
const MAX_BLOCK_DEPTH: usize = 10;
/// Waits before the 1st and 2nd retry of a rate-limited/5xx request.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];
/// Upper bound on a single retry wait, so a large `Retry-After` can't wedge an op.
const MAX_BACKOFF: Duration = Duration::from_secs(3);
/// How long a rendered `page.json` stays cached (a `stat`+`open` reuse it).
const RENDER_TTL: Duration = Duration::from_secs(15);

/// Connection settings for [`NotionVolume`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NotionConfig {
    pub api_key: String,
}

/// A rendered `page.json`: its bytes plus the page's timestamps.
#[derive(Clone)]
struct Rendered {
    bytes: Arc<Vec<u8>>,
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

/// A Notion-backed volume.
pub struct NotionVolume {
    client: reqwest::Client,
    api_key: String,
    /// `page-id -> (fetched-at, rendered)`, evicted after [`RENDER_TTL`].
    cache: Mutex<HashMap<String, (Instant, Rendered)>>,
}

impl NotionVolume {
    pub fn new(cfg: &NotionConfig) -> Result<Self> {
        // Bound every request: a hung upstream call would otherwise wedge the FUSE
        // op (and any process touching the mount) indefinitely. The client builds
        // synchronously; requests run later on whatever runtime drives the async
        // ops.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| CortexError::Io(io::Error::other(e)))?;
        Ok(Self {
            client,
            api_key: cfg.api_key.clone(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    // ---- Notion API client (async) ------------------------------------------

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Bearer {}", self.api_key))
            .header("Notion-Version", NOTION_VERSION)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
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

    /// Pages shared with the integration (search filtered to pages), all pages.
    async fn search_pages(&self) -> Result<Vec<Value>> {
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

    async fn get_page(&self, id: &str) -> Result<Value> {
        if !valid_notion_id(id) {
            return Err(CortexError::NotFound);
        }
        self.send(self.client.get(format!("{API}/pages/{id}"))).await
    }

    /// All immediate block children of `id`, paging through every result.
    async fn list_children(&self, id: &str) -> Result<Vec<Value>> {
        if !valid_notion_id(id) {
            return Err(CortexError::NotFound);
        }
        let mut results = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            // `.query()` encodes the pairs (and any escaping) rather than us
            // splicing them into the URL by hand.
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

    /// Block children recursively, embedding nested blocks under a `children`
    /// key. `child_page`/`child_database` blocks are not descended into (they
    /// surface as subdirectories). Recursion stops at [`MAX_BLOCK_DEPTH`].
    fn list_block_tree<'a>(
        &'a self,
        id: String,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Value>>> + Send + 'a>> {
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

    /// The rendered `page.json` for `page_id`, served from cache when fresh.
    async fn render_cached(&self, page_id: &str) -> Result<Rendered> {
        if let Some((at, r)) = self.cache.lock().unwrap().get(page_id) {
            if at.elapsed() < RENDER_TTL {
                return Ok(r.clone());
            }
        }
        let page = self.get_page(page_id).await?;
        let blocks = self.list_block_tree(page_id.to_string(), 0).await?;
        let normalized = normalize_page(&page, &blocks);
        let bytes = serde_json::to_vec_pretty(&normalized).map_err(io_other)?;
        let rendered = Rendered {
            bytes: Arc::new(bytes),
            mtime: page_time(&page, "last_edited_time"),
            ctime: page_time(&page, "created_time"),
        };
        self.cache
            .lock()
            .unwrap()
            .insert(page_id.to_string(), (Instant::now(), rendered.clone()));
        Ok(rendered)
    }

    /// Top-level (workspace) pages as `<title>__<id>` dir entries.
    async fn top_level_page_dirs(&self) -> Result<Vec<Dirent>> {
        let pages = self.search_pages().await?;
        Ok(pages
            .iter()
            .filter(|p| {
                p.get("parent")
                    .and_then(|x| x.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("workspace")
            })
            .map(|p| Dirent::new(page_dirname(p), DirentKind::Dir))
            .collect())
    }

    /// Contents of a page dir: `page.json` plus a subdir per `child_page` block.
    async fn page_dir_entries(&self, page_id: &str) -> Result<Vec<Dirent>> {
        let blocks = self.list_children(page_id).await?;
        let mut out = vec![Dirent::new("page.json", DirentKind::File)];
        for b in &blocks {
            if b.get("type").and_then(|t| t.as_str()) != Some("child_page") {
                continue;
            }
            let child_title = b
                .get("child_page")
                .and_then(|c| c.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("untitled");
            let child_id = b.get("id").and_then(|i| i.as_str()).unwrap_or("");
            out.push(Dirent::new(
                format!("{}__{}", sanitize_name(child_title), child_id),
                DirentKind::Dir,
            ));
        }
        Ok(out)
    }
}

#[async_trait]
impl Mountable for NotionVolume {
    type Handle = NotionHandle;

    async fn stat(&self, path: &Path) -> Result<Stat> {
        let segs = segments(path);
        match segs.as_slice() {
            [] | [_] => Ok(Stat::new(DirentKind::Dir, 0)),
            [p, rest @ ..] if p == "pages" && !rest.is_empty() => {
                let is_json = rest.last().map(String::as_str) == Some("page.json");
                let dir = if is_json {
                    // `/pages/page.json` has no enclosing page dir.
                    let Some(dir) = rest.iter().nth_back(1) else {
                        return Err(CortexError::NotFound);
                    };
                    dir.as_str()
                } else {
                    rest.last().unwrap().as_str()
                };
                let id = page_id(dir);
                if is_json {
                    // Render so the guest kernel sees the real size (no direct_io).
                    Ok(self.render_cached(&id).await?.stat())
                } else {
                    // Confirm the page dir exists (and pick up its times) cheaply.
                    let page = self.get_page(&id).await?;
                    let mut st = Stat::new(DirentKind::Dir, 0);
                    st.mtime = page_time(&page, "last_edited_time");
                    st.ctime = page_time(&page, "created_time");
                    Ok(st)
                }
            }
            _ => Err(CortexError::NotFound),
        }
    }

    async fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let segs = segments(path);
        match segs.as_slice() {
            [] => Ok(vec![Dirent::new("pages", DirentKind::Dir)]),
            [p] if p == "pages" => self.top_level_page_dirs().await,
            [p, rest @ ..] if p == "pages" && !rest.is_empty() => {
                let last = rest.last().unwrap();
                if last == "page.json" {
                    return Err(CortexError::NotADirectory);
                }
                self.page_dir_entries(&page_id(last)).await
            }
            _ => Err(CortexError::NotFound),
        }
    }

    async fn mkdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    async fn unlink(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    async fn rmdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    async fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        options.validate()?;
        if options.write || options.create || options.create_new || options.truncate {
            return Err(CortexError::ReadOnly);
        }
        let segs = segments(path);
        if segs.len() >= 3 && segs[0] == "pages" && segs.last().map(String::as_str) == Some("page.json")
        {
            let id = page_id(&segs[segs.len() - 2]);
            let r = self.render_cached(&id).await?;
            let stat = r.stat();
            return Ok((NotionHandle { data: r.bytes }, stat));
        }
        // Directories (and everything else) aren't openable as files.
        Err(CortexError::IsADirectory)
    }
}

/// An open `page.json`: its rendered bytes. Reads are served from memory; there
/// is no network on the read path (render happened at `open`/`stat`).
pub struct NotionHandle {
    data: Arc<Vec<u8>>,
}

#[async_trait]
impl FileExt for NotionHandle {
    async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let data = &self.data;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let from = offset as usize;
        let n = (data.len() - from).min(buf.len());
        buf[..n].copy_from_slice(&data[from..from + n]);
        Ok(n)
    }

    async fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[async_trait]
impl FileHandle for NotionHandle {
    async fn truncate(&self, _size: u64) -> Result<()> {
        Err(CortexError::ReadOnly)
    }
}

// ---- helpers ----------------------------------------------------------------

fn io_other<E: std::fmt::Display>(e: E) -> CortexError {
    CortexError::Io(io::Error::other(e.to_string()))
}

/// Turn a finished response into JSON, or map a non-2xx status.
async fn finish(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CortexError::NotFound);
        }
        return Err(io_other(format!("notion API {status}: {body}")));
    }
    // A 2xx whose body will not parse is a broken response, not an empty page —
    // surface it rather than letting `normalize_page`'s field defaults render a
    // silently-blank page.json.
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

/// Page id encoded as the part after the last `__` in a directory name.
fn page_id(dir_name: &str) -> String {
    dir_name
        .rsplit_once("__")
        .map(|(_, id)| id)
        .unwrap_or(dir_name)
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
    let parent_id = parent.get(parent_type).and_then(|v| v.as_str()).unwrap_or("");
    let content_blocks: Vec<Value> = blocks
        .iter()
        .filter(|b| {
            let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            t != "child_page" && t != "child_database"
        })
        .cloned()
        .collect();
    json!({
        "page_id": page.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "title": extract_title(page),
        "url": page.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "created_time": page.get("created_time").and_then(|v| v.as_str()).unwrap_or(""),
        "last_edited_time": page.get("last_edited_time").and_then(|v| v.as_str()).unwrap_or(""),
        "parent_type": parent_type,
        "parent_id": parent_id,
        "archived": page.get("archived").and_then(|v| v.as_bool()).unwrap_or(false),
        "markdown": blocks_to_markdown(&content_blocks),
        "blocks": content_blocks,
    })
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
            let language = content.get("language").and_then(|v| v.as_str()).unwrap_or("");
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
        "child_page" | "child_database" => String::new(),
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

    #[test]
    fn notion_id_validation_rejects_url_escapes() {
        assert!(valid_notion_id("22222222222222222222222222222222"));
        assert!(valid_notion_id("22222222-2222-2222-2222-222222222222"));
        for bad in ["../pages/2222?", "..%2Fpages", "abc/def", "x?y", "x#y", ""] {
            assert!(!valid_notion_id(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn page_id_is_after_last_dunder() {
        assert_eq!(page_id("My_Title__abc123"), "abc123");
        assert_eq!(page_id("a__b__c"), "c");
        assert_eq!(page_id("no-dunder"), "no-dunder");
    }

    /// Live smoke check: reads real pages through the Notion API. Ignored by
    /// default; run explicitly with a token shared with the target pages:
    ///
    ///   NOTION_API_KEY=ntn_… cargo test -p cortex --features notion \
    ///     notion_live_read -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires NOTION_API_KEY + network"]
    async fn notion_live_read() {
        let api_key =
            std::env::var("NOTION_API_KEY").expect("set NOTION_API_KEY to run this live check");
        let vol = NotionVolume::new(&NotionConfig { api_key }).expect("build NotionVolume");

        let root = vol.list(Path::new("/")).await.expect("list /");
        let root_names: Vec<_> = root.iter().map(|e| e.name.clone()).collect();
        println!("root: {root_names:?}");
        assert!(root_names.iter().any(|n| n == "pages"));

        let pages = vol.list(Path::new("/pages")).await.expect("list /pages");
        println!("{} top-level page dir(s):", pages.len());
        for p in &pages {
            println!("  - {}", p.name);
        }
        let Some(first) = pages.first() else {
            println!("no pages shared with this integration — share a page and retry");
            return;
        };
        let dir = &first.name;

        let json_path = format!("/pages/{dir}/page.json");
        let st = vol.stat(Path::new(&json_path)).await.expect("stat page.json");
        assert_eq!(st.kind, DirentKind::File);
        assert!(st.size > 0, "page.json size should be non-zero");

        let (h, ost) = vol
            .open(Path::new(&json_path), OpenOptions::read_only())
            .await
            .expect("open page.json");
        assert_eq!(ost.size, st.size, "open stat size == stat size");
        let mut buf = vec![0u8; ost.size as usize];
        h.read_exact_at(&mut buf, 0).await.expect("read page.json");
        let text = String::from_utf8_lossy(&buf);
        println!("--- {json_path} ({} bytes) ---", buf.len());
        println!("{}", &text[..text.len().min(1500)]);
        for key in ["page_id", "title", "markdown", "blocks"] {
            assert!(text.contains(key), "page.json should contain {key:?}");
        }
    }
}
