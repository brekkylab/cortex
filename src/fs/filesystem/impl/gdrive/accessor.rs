use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use tokio::sync::Mutex;

/// Google's OAuth origin; one token endpoint serves every Google API.
pub(crate) const OAUTH_ORIGIN: &str = "https://oauth2.googleapis.com";

/// Where to reach each Google service. `None` = the real host.
///
/// Drive, Docs, Sheets and Slides are four APIs on four hosts, and a deployment that is
/// not production Google may put any of them anywhere — so no single origin stands in for
/// all of them and each is overridable on its own. Whatever is set here is an *origin*:
/// this code appends only the suffix the official API uses, so the same paths address a
/// mock and production alike.
///
/// **Deployment-level only: the token endpoint receives the app's client secret, so none
/// of this may be user-suppliable.** It derives `Deserialize` for a config file read by
/// whoever runs the mount, not for a field filled in from a request — pointing `oauth`
/// somewhere is pointing the client secret there.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GdriveOrigins {
    /// Serves the OAuth token endpoint (`{oauth}/token`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
    /// Serves `drive/v3` (`{drive}/v3/files`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    /// Serves the Docs API (`{docs}/v1/documents/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Serves the Sheets API (`{sheets}/v4/spreadsheets/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheets: Option<String>,
    /// Serves the Slides API (`{slides}/v1/presentations/…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slides: Option<String>,
}

impl GdriveOrigins {
    /// Whether nothing is overridden, so the field can stay out of a serialized
    /// config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Every service behind one host, laid out the way Google's own paths read:
    /// `{host}/oauth2`, `{host}/drive` and so on. A convenience for a deployment that
    /// fronts all of them, not a substitute for the per-service knobs.
    pub fn behind(host: &str) -> Self {
        let h = host.trim_end_matches('/');
        let at = |service: &str| Some(format!("{h}/{service}"));
        Self {
            oauth: at("oauth2"),
            drive: at("drive"),
            docs: at("docs"),
            sheets: at("sheets"),
            slides: at("slides"),
        }
    }

    /// `over` if set, else `default`, without a trailing slash.
    pub(crate) fn origin(over: &Option<String>, default: &str) -> String {
        over.as_deref()
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_string()
    }
}

/// Default service origins, without the version suffix this code appends; each is
/// overridable through [`GdriveOrigins`].
const DRIVE_ORIGIN: &str = "https://www.googleapis.com/drive";
/// The Docs-editors types live behind their own APIs, on their own hosts. Drive can
/// only *export* them; their structure (paragraph indices, formulas, slide geometry)
/// exists nowhere else.
const DOCS_ORIGIN: &str = "https://docs.googleapis.com";
const SHEETS_ORIGIN: &str = "https://sheets.googleapis.com";
const SLIDES_ORIGIN: &str = "https://slides.googleapis.com";

/// Every host this accessor talks to, resolved once from a config.
///
/// Each is an origin from [`GdriveOrigins`] plus the version suffix the official API uses,
/// so nothing here encodes any one deployment's path layout: point `GdriveOrigins::drive` at
/// a gateway and `/v3` still follows, exactly as it does against Google.
#[derive(Clone)]
struct Endpoints {
    drive: String,
    token: String,
    docs: String,
    sheets: String,
    slides: String,
}

fn endpoints(o: &GdriveOrigins) -> Endpoints {
    Endpoints {
        drive: format!("{}/v3", GdriveOrigins::origin(&o.drive, DRIVE_ORIGIN)),
        token: format!("{}/token", GdriveOrigins::origin(&o.oauth, OAUTH_ORIGIN)),
        docs: format!("{}/v1", GdriveOrigins::origin(&o.docs, DOCS_ORIGIN)),
        sheets: format!("{}/v4", GdriveOrigins::origin(&o.sheets, SHEETS_ORIGIN)),
        slides: format!("{}/v1", GdriveOrigins::origin(&o.slides, SLIDES_ORIGIN)),
    }
}

/// Per-file fields every listing requests: exactly what the mount needs to shape an
/// entry. Joined at request time, so the mask can't grow a stray space or lose a comma.
const FILE_FIELDS: &[&str] = &[
    "id",
    "name",
    "mimeType",
    // Shared-drive scoping: children of a shared drive must be listed with it.
    "driveId",
    // Drive's own size: set for blobs *and* Docs-editors files, absent for folders and
    // shortcuts (backlot also omits it on native docs, so don't rely on either shape).
    //
    // Exact for a blob, but never a document's length: a document is served as its API's
    // JSON, which Drive's number does not describe (4,775 listed against 133,625 of JSON
    // on one document). Kept so `read_window` can refuse before a byte moves, and so a
    // blob listed *without* a size is told apart from one sized zero.
    "size",
    "modifiedTime",
    "createdTime",
];

/// Hard cap on listing pages (1000 files/page, 100 drives/page) so a
/// duplicate/looping `nextPageToken` (a known Drive API pathology with some
/// query/corpora combos) can't spin forever.
const MAX_PAGES: usize = 50;

/// Retry budget for a rate-limited/5xx request, kept low because these calls sit
/// behind a FUSE/WebDAV op the agent blocks on.
const MAX_RETRIES: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(16);
const JITTER_MAX_MS: u64 = 1000;

/// Ceiling on one document's JSON.
///
/// A document has no ranges: a read of any part of it produces the whole thing, so its
/// size sets the memory a single read costs — body, parsed tree, indented output.
///
/// 64 MiB against a measured worst case of 2.5 MB leaves room far past any real document
/// while bounding one read's footprint. The JSON stays small where an export does not: a
/// 60 MB document is 2.5 MB of it, and a 401 MB workbook is 242,935 bytes, where its
/// 382 MiB export would be far over this ceiling.
pub(super) const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

/// Whether a 403 body names a limit that clears by waiting.
///
/// Drive answers a per-user or per-project rate limit with 403 and a `reason` — a 429
/// is only one of the shapes it uses. The other 403s (`insufficientFilePermissions`,
/// `dailyLimitExceeded`) do not clear by retrying, so they stay terminal.
fn is_rate_limit(body: &str) -> bool {
    const RETRYABLE: [&str; 3] = [
        "rateLimitExceeded",
        "userRateLimitExceeded",
        "sharingRateLimitExceeded",
    ];
    let reasons: Vec<String> = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            Some(
                v.pointer("/error/errors")?
                    .as_array()?
                    .iter()
                    .filter_map(|e| e.get("reason")?.as_str().map(str::to_string))
                    .collect(),
            )
        })
        .unwrap_or_default();
    if !reasons.is_empty() {
        return reasons.iter().any(|r| RETRYABLE.contains(&r.as_str()));
    }
    // No parseable `errors[]`: fall back to the text, so a shape we have not seen
    // still lands on the ladder rather than failing a wait-and-retry condition.
    RETRYABLE.iter().any(|r| body.contains(r))
}

/// The first `n` characters of `s`, cut on a character boundary.
fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Read a response body, refusing at `limit` rather than after it.
///
/// Checking `Content-Length` before buffering cannot work: none of these endpoints
/// declares a length (measured on Docs, Slides, `spreadsheets.get` and `values:batchGet`:
/// no `Content-Length`, no `Transfer-Encoding`, just an HTTP/2 stream). Reading frame by
/// frame refuses before the whole body is allocated.
async fn body_within(
    mut resp: reqwest::Response,
    limit: u64,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() as u64 + chunk.len() as u64 > limit {
            anyhow::bail!("{what} is over the {limit} byte limit for a whole-document read");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A tab name as an A1 range: quoted, with any literal quote doubled.
///
/// A bare name that looks like a cell reference is one. Measured on a real spreadsheet:
/// `ranges=연간 요약` answered `'연간 요약'!A1:Z968`, while `ranges=A1` answered
/// `'연간 요약'!A1` — the *first* sheet's cell, not a sheet of that name (`B2` and `A:A`
/// likewise), so the caller would attach someone else's cells to the tab. Quoting is what
/// A1 notation specifies for a name, and changes nothing for ordinary ones.
fn quote_a1(tab: &str) -> String {
    format!("'{}'", tab.replace('\'', "''"))
}

/// Exponential backoff with jitter for retry `n` (0-based), per Google's API
/// guidance: `min(2^n s + rand(0..=1000ms), maximum_backoff)`.
///
/// The jitter comes from the clock rather than a random-number crate: it only has to keep
/// two callers that hit the same 429 from waking together, and the nanoseconds between
/// their reads do that.
fn backoff_delay(n: u32) -> Duration {
    let base = Duration::from_secs(1u64 << n.min(16));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = Duration::from_millis(nanos % (JITTER_MAX_MS + 1));
    (base + jitter).min(MAX_BACKOFF)
}

/// The `Retry-After` delay, if present (delta-seconds only; the HTTP-date form
/// is treated as absent and falls back to [`backoff_delay`]).
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GdriveConfig {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// Where to reach each Google service, when not production Google (an enterprise
    /// mock or a gateway). Deployment-level only — the token endpoint receives the
    /// app's client secret, so a caller must never fill this from user input.
    #[serde(default, skip_serializing_if = "GdriveOrigins::is_default")]
    pub origins: GdriveOrigins,
}

/// Holds Google OAuth credentials (one refresh token) and a cached access token. The
/// mount is read-only, so the token needs `https://www.googleapis.com/auth/drive.readonly`.
pub struct GdriveAccessor {
    client: reqwest::Client,
    config: GdriveConfig,
    /// Every API host, resolved once from [`GdriveConfig::origins`].
    urls: Endpoints,
    /// Cached OAuth access token + its expiry. Refreshed proactively before
    /// expiry and on a 401 (see [`Self::send_with_refresh`]).
    access_token: Mutex<Option<(String, Instant)>>,
}

impl GdriveAccessor {
    pub fn new(config: &GdriveConfig) -> anyhow::Result<Self> {
        let urls = endpoints(&config.origins);
        Ok(Self {
            // A hung upstream call would otherwise wedge the filesystem op, and any
            // process touching the mount, forever.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            config: config.clone(),
            urls,
            access_token: Mutex::new(None),
        })
    }

    async fn token(&self) -> anyhow::Result<String> {
        let mut guard = self.access_token.lock().await;
        // Reuse a cached token until it's within 60s of expiry (proactive refresh
        // avoids the "everything 401s after ~1h" failure).
        if let Some((t, exp)) = guard.as_ref()
            && *exp > Instant::now() + Duration::from_secs(60)
        {
            return Ok(t.clone());
        }
        let resp = self
            .client
            .post(&self.urls.token)
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("refresh_token", self.config.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("google token exchange {status}: {body}");
        }
        let v: Value = serde_json::from_str(&body)?;
        let token = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("no access_token in response"))?
            .to_string();
        let expires_in = v.get("expires_in").and_then(|e| e.as_u64()).unwrap_or(3600);
        *guard = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(expires_in),
        ));
        Ok(token)
    }

    /// Send a request built from the current access token, retrying transient
    /// failures. A 401 (expired/revoked despite proactive refresh) drops the
    /// token, refreshes, and retries once; a 429/5xx retries a bounded number
    /// of times honoring `Retry-After`. Every call this accessor makes is an
    /// idempotent GET, so the 5xx retry is always safe. Non-retryable statuses
    /// are returned to the caller, which classifies them via `error_for_status`.
    async fn send_with_refresh(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        self.send_retrying(build, MAX_RETRIES).await
    }

    /// As [`Self::send_with_refresh`], with a caller-chosen retry ceiling for a call
    /// whose failure the caller shrugs off.
    async fn send_retrying(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
        max_retries: u32,
    ) -> anyhow::Result<reqwest::Response> {
        let mut token = self.token().await?;
        let mut refreshed = false;
        let mut retries = 0u32;
        loop {
            let resp = build(&token).send().await?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                *self.access_token.lock().await = None;
                token = self.token().await?;
                refreshed = true;
                continue;
            }
            // Drive reports a per-user rate limit as 403 with a `reason`, not as 429, so
            // the status alone would give up on a condition that clears by waiting.
            // Reading the reason consumes the response, which is fine: a 403 not retried
            // fails either way, and its body is the explanation.
            if status == reqwest::StatusCode::FORBIDDEN {
                let body = resp.text().await.unwrap_or_default();
                if is_rate_limit(&body) && retries < max_retries {
                    let wait = backoff_delay(retries);
                    retries += 1;
                    tokio::time::sleep(wait).await;
                    continue;
                }
                anyhow::bail!("gdrive 403: {}", first_chars(&body, 300));
            }
            let retryable =
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if retryable && retries < max_retries {
                // An explicit Retry-After wins (capped so the caller isn't
                // blocked too long); otherwise exponential backoff with jitter.
                let wait = match retry_after(&resp) {
                    Some(d) => d.min(MAX_BACKOFF),
                    None => backoff_delay(retries),
                };
                retries += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn get_json(&self, url: reqwest::Url) -> anyhow::Result<Value> {
        let resp = self
            .send_with_refresh(|t| self.client.get(url.clone()).bearer_auth(t))
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Shared `files.list` pagination: one `q`, optional shared-drive scoping,
    /// truncated at `limit` collected files.
    async fn list_files_q(
        &self,
        q: &str,
        drive_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>> {
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                eprintln!("gdrive files.list: reached page cap {MAX_PAGES}; listing truncated");
                break;
            }
            let mut params: Vec<(&str, String)> = vec![
                ("q", q.to_string()),
                (
                    "fields",
                    format!("nextPageToken,files({})", FILE_FIELDS.join(",")),
                ),
                ("pageSize", "1000".to_string()),
                // An explicit order so two `ls` of one folder agree; newest-first is
                // what a person scanning a Drive folder expects.
                ("orderBy", "modifiedTime desc".to_string()),
            ];
            if let Some(d) = drive_id {
                params.push(("corpora", "drive".to_string()));
                params.push(("driveId", d.to_string()));
                params.push(("includeItemsFromAllDrives", "true".to_string()));
                params.push(("supportsAllDrives", "true".to_string()));
            }
            if let Some(pt) = &page_token {
                params.push(("pageToken", pt.clone()));
            }
            let url =
                reqwest::Url::parse_with_params(&format!("{}/files", self.urls.drive), &params)?;
            let v = self.get_json(url).await?;
            if let Some(arr) = v.get("files").and_then(|f| f.as_array()) {
                files.extend(arr.iter().cloned());
            }
            if files.len() >= limit {
                files.truncate(limit);
                eprintln!("gdrive files.list: reached cap {limit}; listing truncated");
                break;
            }
            let next = v
                .get("nextPageToken")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            // Stop on no token, or a token identical to the one we just used
            // (would otherwise re-fetch the same page forever).
            if next.is_none() || next == page_token {
                break;
            }
            page_token = next;
        }
        Ok(files)
    }

    /// The immediate, non-trashed children of `folder_id` ("root" for the My
    /// Drive root). `drive_id` is set when listing inside a shared drive.
    pub async fn list_files(
        &self,
        folder_id: &str,
        drive_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>> {
        let q = format!("'{folder_id}' in parents and trashed=false");
        self.list_files_q(&q, drive_id, limit).await
    }

    /// Items shared with the account ("Shared with me"). They carry no
    /// `parents`, so they are unreachable through the folder tree — this is the
    /// only listing that surfaces them.
    pub async fn list_shared_with_me(&self, limit: usize) -> anyhow::Result<Vec<Value>> {
        self.list_files_q("sharedWithMe=true and trashed=false", None, limit)
            .await
    }

    /// A blob file's bytes (`files.get?alt=media`), or just one window of them.
    /// A Docs-editors document has no bytes and 403s here; it is served as its own
    /// API's JSON instead (see [`Self::document_json`] and friends).
    ///
    /// The range makes serving originals affordable; the caller sizes it. Without `Range`,
    /// every chunk read would pull the whole object, so one `grep` over a folder of 5 MB
    /// PDFs would transfer gigabytes to look at a few kilobytes.
    pub async fn download(
        &self,
        id: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> anyhow::Result<Vec<u8>> {
        // An empty window is not a request: the no-`Range` arm below would pull the
        // whole object to answer with nothing.
        if matches!(&range, Some(r) if r.end <= r.start) {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/files/{id}?alt=media&supportsAllDrives=true",
            self.urls.drive
        );
        let resp = self
            .send_with_refresh(|t| {
                let req = self.client.get(&url).bearer_auth(t);
                match &range {
                    // HTTP byte ranges are inclusive at both ends.
                    Some(r) if r.end > r.start => {
                        req.header("Range", format!("bytes={}-{}", r.start, r.end - 1))
                    }
                    _ => req,
                }
            })
            .await?;
        // A range starting at or past EOF answers 416. For a reader walking a
        // file to its end that is a clean EOF, not a failure.
        if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(Vec::new());
        }
        Ok(resp.error_for_status()?.bytes().await?.to_vec())
    }

    /// A Google Doc's own structure (`documents.get`).
    ///
    /// Not an export: paragraphs, styles, tables, footnotes and — crucially for
    /// editing — the character indices every `batchUpdate` addresses. Measured on
    /// a real account: 59 KB to 2.4 MB, roughly 10-9,000x the exported text,
    /// because every run carries its styling.
    pub async fn document_json(&self, id: &str) -> anyhow::Result<Vec<u8>> {
        self.get_pretty(&format!("{}/documents/{id}", self.urls.docs))
            .await
    }

    /// A presentation's own structure (`presentations.get`): pages, shapes,
    /// transforms, speaker notes. Measured 1.3-2.2 MB even for a 6 KB deck —
    /// slide geometry dwarfs the text.
    pub async fn presentation_json(&self, id: &str) -> anyhow::Result<Vec<u8>> {
        self.get_pretty(&format!("{}/presentations/{id}", self.urls.slides))
            .await
    }

    /// A spreadsheet's structure, without cell data (`spreadsheets.get`).
    ///
    /// 3.7-19.5 KB measured, and it carries what addressing a cell needs: sheet
    /// ids, titles, grid extents, named ranges, charts, conditional formats.
    pub async fn spreadsheet_json(&self, id: &str) -> anyhow::Result<Vec<u8>> {
        self.get_pretty(&format!("{}/spreadsheets/{id}", self.urls.sheets))
            .await
    }

    /// Cell values for the named tabs (`spreadsheets.values.batchGet`).
    ///
    /// Not `includeGridData=true`: that bills per **allocated** cell at 578-920 bytes
    /// each (measured), and a tab allocates 1000x26 whether or not one cell is filled —
    /// one real workbook had 210,125 allocated cells, an estimated 189 MB. `batchGet`
    /// returns the used range only: 443 B to 105 KB per tab for that workbook.
    ///
    /// Values are formatted as the sheet displays them, so what a reader greps is
    /// what a person sees in the cell.
    pub async fn sheet_values_batch(&self, id: &str, tabs: &[String]) -> anyhow::Result<Value> {
        let quoted: Vec<String> = tabs.iter().map(|t| quote_a1(t)).collect();
        let mut params: Vec<(&str, &str)> = vec![
            ("majorDimension", "ROWS"),
            ("valueRenderOption", "FORMATTED_VALUE"),
            ("dateTimeRenderOption", "FORMATTED_STRING"),
        ];
        params.extend(quoted.iter().map(|r| ("ranges", r.as_str())));
        let url = reqwest::Url::parse_with_params(
            &format!("{}/spreadsheets/{id}/values:batchGet", self.urls.sheets),
            &params,
        )?;
        let resp = self
            .send_with_refresh(|t| self.client.get(url.clone()).bearer_auth(t))
            .await?
            .error_for_status()?;
        // Bounded while reading, not after: the tree parsed from this costs several
        // times its bytes, and the budget that decides how much of it is kept applies
        // after the parse — too late to protect anything.
        let raw = body_within(resp, MAX_DOCUMENT_BYTES, "spreadsheet values").await?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// GET a JSON API response, pretty-printed so a reader can scan it as lines rather
    /// than parse one long string.
    ///
    /// Refuses a response over [`MAX_DOCUMENT_BYTES`], while the body is read (see
    /// [`body_within`]). A document is all-or-nothing and its memory is spent here — raw
    /// body, the `Value` parsed from it (several times its size), the indented copy — so
    /// past some size the answer has to be "no" rather than a gigabyte of allocations.
    async fn get_pretty(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .send_with_refresh(|t| self.client.get(url).bearer_auth(t))
            .await?
            .error_for_status()?;
        let raw = body_within(resp, MAX_DOCUMENT_BYTES, "document").await?;
        let v: Value = serde_json::from_slice(&raw)?;
        drop(raw);
        let mut bytes = serde_json::to_vec_pretty(&v)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Shared drives visible to the account.
    ///
    /// One attempt, off the retry ladder: the caller treats a failure as "this account
    /// has none", and five backoffs would block the first `ls` of a mount for ~33 s to
    /// produce an answer that is then discarded.
    pub async fn list_shared_drives(&self) -> anyhow::Result<Vec<Value>> {
        let mut drives = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                break;
            }
            let mut params: Vec<(&str, String)> = vec![
                ("fields", "nextPageToken,drives(id,name)".to_string()),
                ("pageSize", "100".to_string()),
            ];
            if let Some(pt) = &page_token {
                params.push(("pageToken", pt.clone()));
            }
            let url =
                reqwest::Url::parse_with_params(&format!("{}/drives", self.urls.drive), &params)?;
            let v: Value = self
                .send_retrying(|t| self.client.get(url.clone()).bearer_auth(t), 0)
                .await?
                .error_for_status()?
                .json()
                .await?;
            if let Some(arr) = v.get("drives").and_then(|d| d.as_array()) {
                drives.extend(arr.iter().cloned());
            }
            let next = v
                .get("nextPageToken")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            if next.is_none() || next == page_token {
                break;
            }
            page_token = next;
        }
        Ok(drives)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five hosts in production, each overridable on its own, and the version suffix
    /// is the official one either way, so nothing depends on a deployment's path layout.
    /// Checked through `endpoints`, where every origin is read.
    #[test]
    fn each_service_keeps_its_official_path_under_any_origin() {
        let e = endpoints(&GdriveOrigins::default());
        assert_eq!(e.drive, "https://www.googleapis.com/drive/v3");
        assert_eq!(e.token, "https://oauth2.googleapis.com/token");
        assert_eq!(e.docs, "https://docs.googleapis.com/v1");
        assert_eq!(e.sheets, "https://sheets.googleapis.com/v4");
        assert_eq!(e.slides, "https://slides.googleapis.com/v1");

        // One service moves, the rest stay on Google.
        let e = endpoints(&GdriveOrigins {
            sheets: Some("http://localhost:9000/sheets-api/".into()),
            ..Default::default()
        });
        assert_eq!(e.sheets, "http://localhost:9000/sheets-api/v4");
        assert_eq!(e.docs, "https://docs.googleapis.com/v1");

        // `behind` covers one host serving all of them; trailing slash tolerated.
        let e = endpoints(&GdriveOrigins::behind("http://localhost:8000/"));
        assert_eq!(e.drive, "http://localhost:8000/drive/v3");
        assert_eq!(e.token, "http://localhost:8000/oauth2/token");
        assert_eq!(e.docs, "http://localhost:8000/docs/v1");
        assert_eq!(e.sheets, "http://localhost:8000/sheets/v4");
        assert_eq!(e.slides, "http://localhost:8000/slides/v1");

        // An origins that overrides nothing says so, which keeps it out of a serialized
        // config.
        assert!(!GdriveOrigins::behind("https://mock.example.com/").is_default());
        assert!(
            GdriveOrigins::default().is_default(),
            "nothing set stays absent"
        );
    }

    /// A tab is named by a person but read as A1 notation, where a name that looks
    /// like a cell reference *is* one (measured: `ranges=A1` returned the first
    /// sheet's A1 cell, not the sheet named `A1`).
    #[test]
    fn a_tab_name_is_quoted_so_it_stays_a_name() {
        assert_eq!(quote_a1("연간 요약"), "'연간 요약'");
        assert_eq!(quote_a1("1. 매출 요약+상세"), "'1. 매출 요약+상세'");
        // Unquoted, each of these addresses cells instead of a sheet.
        assert_eq!(quote_a1("A1"), "'A1'");
        assert_eq!(quote_a1("A:A"), "'A:A'");
        assert_eq!(quote_a1("Sheet1!B2"), "'Sheet1!B2'");
        // A quote in the name closes the quoting unless doubled.
        assert_eq!(quote_a1("it's"), "'it''s'");
    }

    /// Drive uses 403 for a limit that clears by waiting, which the status alone reads
    /// as terminal.
    #[test]
    fn a_403_that_clears_by_waiting_is_told_apart_from_one_that_does_not() {
        let body = |reason: &str| {
            serde_json::json!({
                "error": { "code": 403, "errors": [{ "reason": reason, "message": "x" }] }
            })
            .to_string()
        };
        for reason in [
            "rateLimitExceeded",
            "userRateLimitExceeded",
            "sharingRateLimitExceeded",
        ] {
            assert!(is_rate_limit(&body(reason)), "{reason} clears by waiting");
        }
        for reason in [
            "insufficientFilePermissions",
            "dailyLimitExceeded",
            "appNotAuthorizedToFile",
        ] {
            assert!(!is_rate_limit(&body(reason)), "{reason} does not");
        }
        // A shape with no parseable `errors[]` still lands on the ladder if it says so.
        assert!(is_rate_limit("Rate Limit Exceeded: userRateLimitExceeded"));
        assert!(!is_rate_limit("<html>403 Forbidden</html>"));
        assert!(!is_rate_limit(""));
    }

    #[test]
    fn an_error_body_is_cut_on_a_character_boundary() {
        assert_eq!(first_chars("한글 오류 메시지", 4), "한글 오");
        assert_eq!(first_chars("short", 300), "short");
    }

    #[test]
    fn backoff_is_exponential_jittered_and_capped() {
        for _ in 0..100 {
            let d0 = backoff_delay(0);
            assert!(
                d0 >= Duration::from_secs(1) && d0 <= Duration::from_millis(2000),
                "{d0:?}"
            );
            let d2 = backoff_delay(2);
            assert!(
                d2 >= Duration::from_secs(4) && d2 <= Duration::from_millis(5000),
                "{d2:?}"
            );
            // large n → capped at MAX_BACKOFF; 2^4=16s + jitter > cap
            assert_eq!(backoff_delay(4), MAX_BACKOFF);
            assert_eq!(backoff_delay(10), MAX_BACKOFF);
        }
    }
}
