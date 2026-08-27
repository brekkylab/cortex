use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::origins::{OAUTH_ORIGIN, Origins};
use tokio::sync::Mutex;

/// The origin each service lives on, without the version suffix this code appends —
/// see [`Origins`], which overrides these one at a time.
const DRIVE_ORIGIN: &str = "https://www.googleapis.com/drive";

/// Every host this accessor talks to, resolved once from a config.
///
/// Each is an origin from [`Origins`] plus the version suffix the official API uses,
/// so nothing here encodes any one deployment's path layout: point `Origins::drive` at
/// a gateway and `/v3` still follows, exactly as it does against Google.
#[derive(Clone)]
struct Endpoints {
    drive: String,
    token: String,
}

fn endpoints(o: &Origins) -> Endpoints {
    Endpoints {
        drive: format!("{}/v3", Origins::origin(&o.drive, DRIVE_ORIGIN)),
        token: format!("{}/token", Origins::origin(&o.oauth, OAUTH_ORIGIN)),
    }
}

/// Per-file fields requested from every listing — exactly what the mount needs to
/// shape an entry. One entry per field, joined at request time: the
/// separators aren't hand-maintained, so a mask can't grow a stray space or a
/// missing comma the way a single hand-written literal can.
const FILE_FIELDS: &[&str] = &[
    "id",
    "name",
    "mimeType",
    // Shared-drive scoping: children of a shared drive must be listed with it.
    "driveId",
    // Drive's own size: populated for binary files *and* Docs-editors files,
    // absent for folders and shortcuts. (enterprise-mock omits it on native
    // docs — a divergence from Google, so don't rely on either shape.)
    //
    // For a document it is the size of what Drive *stores*, which is near what an
    // export hands back only when the document is mostly images. Over six measured:
    //
    //   401,518,068 -> 400,984,746    +0.13%   images
    //    60,063,731 ->  59,911,656    +0.25%   images
    //     3,588,735 ->   3,705,698     -3.3%   text
    //     8,762,033 ->  13,672,172      -56%   text
    //        26,210 ->      32,151      -23%   text
    //        17,796 ->     409,242    -2,300%  text
    //
    // Text compresses tighter in Drive's own form than in OOXML, so the number errs
    // *low* there and by any amount. It is therefore a one-sided guard: refusing a
    // document whose listed size already clears a ceiling never refuses one that would
    // have fitted, while a document it admits may still be far over — which is what the
    // frame counter is for. Never the entry's length, either; see `entry_size`.
    "size",
    // Where an export of a Docs-editors file can actually be fetched, per MIME type.
    //
    // `files.export` caps at 10 MB and answers `exportSizeLimitExceeded` past it, with
    // no partial download to work around it: a 5 MiB range and a 1 MiB range both drew
    // the same `403`, after 48 and 41 seconds spent rendering the export they refused.
    //
    // Nothing documents this link as the way around that. Google's guide offers it as
    // how to export *within a browser*, says nothing about exceeding 10 MB, and puts the
    // cap only on `files.export`. That it has no cap of its own is measured — a 401 MB
    // workbook came through — not promised. Read rather than built: the path
    // shape differs per type (`/spreadsheets/export` against
    // `/feeds/download/documents/export/Export`), and a URL this constructed would be
    // a second copy of a layout Google never promised to keep.
    "exportLinks",
    "modifiedTime",
    "createdTime",
    "webViewLink",
    // Nested sub-selection, so this one entry carries its own punctuation.
    "owners(displayName,emailAddress)",
];

/// Hard cap on listing pages (1000 files/page, 100 drives/page) so a
/// duplicate/looping `nextPageToken` (a known Drive API pathology with some
/// query/corpora combos) can't spin forever.
const MAX_PAGES: usize = 50;

/// Retry budget for a rate-limited/5xx request; same reasoning as the gmail
/// accessor's (these calls sit behind a FUSE/WebDAV op the agent blocks on, so
/// the worst-case total stays low).
const MAX_RETRIES: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(16);
const JITTER_MAX_MS: u64 = 1000;

/// Ceiling on one document's export.
///
/// An export honours no range: a read of any part of a document produces all of it, so
/// this is the memory one read costs, and it is held until the listing TTL runs out.
/// 64 MiB against measured exports of 32 KB to 14 MB leaves room well past anything
/// ordinary while keeping that footprint bounded.
///
/// It is not the only guard, and not the first one. Drive's listed `size` refuses a
/// document that is already over by that number before a byte moves — which is how a
/// 401 MB workbook in the corpus costs nothing to refuse rather than 64 MiB. But the
/// listed size errs low on text-heavy documents, by up to a factor of 23 measured, so
/// plenty of documents clear it and land here instead. This is the guard that holds.
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
/// The obvious guard — check `Content-Length`, then buffer — never fires here: an export
/// declares no length (measured: no `Content-Length`, no `Transfer-Encoding`, just an
/// HTTP/2 stream; `HEAD` answers a `content-length` of `0`, and `Range` is ignored), so
/// the only check that could run was the one after the whole body had already been
/// allocated. Reading frame by frame makes the limit mean what it says.
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

/// Whether a redirect target is one of Google's own content hosts.
///
/// Exports land on `doc-<something>.googleusercontent.com`, which is a family rather
/// than one name, so this matches the suffix. A bare `ends_with` would also accept
/// `evilgoogleusercontent.com`, hence the dot.
fn is_google_content_host(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|h| h == "googleusercontent.com" || h.ends_with(".googleusercontent.com"))
}

/// Exponential backoff with jitter for retry `n` (0-based), per Google's API
/// guidance: `min(2^n s + rand(0..=1000ms), maximum_backoff)`.
///
/// The jitter comes from the clock rather than a random-number crate. What it has to do
/// is keep two callers that hit the same 429 from waking together, and the nanosecond
/// they each read it apart is enough for that — a whole dependency for one line here
/// would not buy anything the retry can tell the difference between.
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
    /// app's client secret, so this is NOT part of the mount-create API; the backend
    /// injects it from its own config.
    #[serde(default, skip_serializing_if = "Origins::is_default")]
    pub origins: Origins,
}

/// Holds Google OAuth credentials (one refresh token) and a cached access
/// token. The mount is read-only and never transfers file content, so the token
/// needs `https://www.googleapis.com/auth/drive.readonly` (metadata-only scopes
/// would also do for listing, but `drive.readonly` is the documented one).
pub struct GdriveAccessor {
    client: reqwest::Client,
    /// The client the export path uses, which differs from the one above in exactly one
    /// way: it does not follow redirects.
    ///
    /// An export answers `307` to a host that is not the one the token belongs to, so
    /// who receives the token has to be this code's decision and not a policy default.
    /// See [`Self::export_document`].
    export_client: reqwest::Client,
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
            // Bound every request: a hung upstream call behind a filesystem op
            // would otherwise wedge the op (and any process touching the mount)
            // forever. A timeout makes it recoverable.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            // A longer ceiling than the API client's, because this one waits on a
            // document being *rendered* rather than on a JSON reply: 67 seconds
            // measured on a 401 MB workbook, 10 on a 60 MB document.
            export_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
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

    /// A Docs-editors document, exported, from a link the *listing* handed over.
    ///
    /// Two legs, on purpose. The link answers `307` to a `googleusercontent.com` host
    /// whose query carries its own signed grant, so the token belongs on the first leg
    /// and nowhere else; measured, that second host serves the bytes with no
    /// `Authorization` at all. Following the redirect inside the client would leave
    /// which host receives the token up to a library default, and that is not a default
    /// worth depending on — so this reads the `Location` and issues the second request
    /// itself, without the token.
    ///
    /// `expect` is the MIME the caller asked to export as, and it is checked. Every
    /// failure this endpoint produces is an HTML page (measured: `401` for a document
    /// the token may not read, `404` for an unknown id and for a format it does not
    /// serve), and a `200` carrying HTML is what a proxy or a captive portal looks
    /// like. Neither may become the bytes of a file.
    ///
    /// Nothing here is logged. The redirect's `dat=` parameter *is* a credential for
    /// that object, so the URL is as sensitive as the token that fetched it.
    pub async fn export_document(
        &self,
        link: &str,
        expect: &str,
        limit: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let first = self
            .send_with_refresh(|t| self.export_client.get(link).bearer_auth(t))
            .await?;
        let status = first.status();
        let resp = if status.is_redirection() {
            let to = first
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("export: {status} with no Location"))?;
            let to = reqwest::Url::parse(to).map_err(|e| anyhow::anyhow!("export: {e}"))?;
            // The host is the server's to name and ours to accept. Nothing is sent to it,
            // but its answer becomes the bytes of a file, so an unexpected host must not
            // get to supply them.
            if !is_google_content_host(&to) {
                anyhow::bail!(
                    "export redirected to {}, which is not a host this mount reads from",
                    to.host_str().unwrap_or("<no host>")
                );
            }
            self.export_client.get(to).send().await?
        } else {
            first
        };
        let resp = resp.error_for_status()?;
        let got = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if got != expect {
            anyhow::bail!("export answered {got:?} where {expect:?} was asked for");
        }
        body_within(resp, limit, "export").await
    }

    /// As [`Self::send_with_refresh`], with a caller-chosen retry ceiling.
    ///
    /// The full ladder is right for a call whose answer the caller needs, and wrong
    /// for one whose failure it shrugs off: the shared-drive listing is best-effort,
    /// and walking five backoffs made the first `ls` of a mount block 33 seconds to
    /// produce a result that was then discarded.
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
            // the status alone classifies it as terminal and the caller gives up on a
            // condition that clears by waiting. The reason is in the body, and reading
            // the body consumes the response — which is fine, because a 403 this loop
            // does not retry is a failure either way, and saying why beats handing back
            // a response whose only content is the explanation.
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
                // Ask Drive to order, rather than leaving it unspecified: two
                // `ls` of one folder should not disagree, and newest-first is
                // what a person scanning a Drive folder expects. (The mock
                // ignored `orderBy` until enterprise-mock#28 fixed it, which is
                // why this was done locally before.)
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

    /// A file's exact length, without downloading it: ask for one byte and read the
    /// total out of `Content-Range` (`bytes 0-0/119265498`).
    ///
    /// For the rare row Drive lists without a `size`. Measured on a 119 MB PDF: one
    /// byte, 0.68s. The alternative — reading the object to find out how long it is
    /// — is the thing every other guard here exists to avoid.
    pub async fn probe_len(&self, id: &str) -> anyhow::Result<u64> {
        let url = format!(
            "{}/files/{id}?alt=media&supportsAllDrives=true",
            self.urls.drive
        );
        let resp = self
            .send_with_refresh(|t| {
                self.client
                    .get(&url)
                    .bearer_auth(t)
                    .header("Range", "bytes=0-0")
            })
            .await?
            .error_for_status()?;
        let total = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.rsplit_once('/')
                    .map(|(_, total)| total.trim().to_string())
            })
            .ok_or_else(|| anyhow::anyhow!("gdrive probe {id}: no Content-Range in a 206"))?;
        total
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("gdrive probe {id}: bad total {total:?}: {e}"))
    }

    /// A blob file's bytes (`files.get?alt=media`), or just one window of them.
    ///
    /// A Docs-editors document has no bytes and 403s here — *"Only files with binary
    /// content can be downloaded. Use Export with Docs Editors files."* — so it goes
    /// through [`Self::export_document`] instead.
    ///
    /// The range is what makes serving originals affordable, and how wide to make it is
    /// the caller's to decide — see `GdriveFs::span`, which sizes it by whether the reader
    /// looks to be walking the file. Without `Range` at all, every chunk read would pull
    /// the whole object, so one `grep` over a folder of 5 MB PDFs would transfer gigabytes
    /// to look at a few kilobytes.
    pub async fn download(
        &self,
        id: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> anyhow::Result<Vec<u8>> {
        // An empty window is not a request. It used to fall through to the arm that
        // sends no `Range` at all, so `File::read_bytes(0)` pulled the whole object and
        // returned none of it — 20 MB to answer with an empty vector.
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

    /// Shared drives visible to the account.
    ///
    /// Deliberately off the retry ladder. The caller treats a failure as "this account
    /// has none", so walking five backoffs makes the first `ls` of a mount block for
    /// half a minute to produce an answer that is then discarded. One attempt, and the
    /// caller decides what to do with a failure.
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

    /// Two hosts in production, each overridable on its own, and the version suffix is
    /// the official one either way — so nothing here depends on how a particular
    /// deployment lays out its paths.
    #[test]
    fn each_service_keeps_its_official_path_under_any_origin() {
        let e = endpoints(&Origins::default());
        assert_eq!(e.drive, "https://www.googleapis.com/drive/v3");
        assert_eq!(e.token, "https://oauth2.googleapis.com/token");

        // One moves, the other stays on Google.
        let e = endpoints(&Origins {
            drive: Some("http://localhost:9000/drive-api/".into()),
            ..Default::default()
        });
        assert_eq!(e.drive, "http://localhost:9000/drive-api/v3");
        assert_eq!(e.token, "https://oauth2.googleapis.com/token");

        // `behind` covers one host serving both; trailing slash tolerated.
        let e = endpoints(&Origins::behind("http://localhost:8000/"));
        assert_eq!(e.drive, "http://localhost:8000/drive/v3");
        assert_eq!(e.token, "http://localhost:8000/oauth2/token");
    }

    /// Drive uses 403 for a limit that clears by waiting, which the status alone reads
    /// as terminal — measured in review: a 403 `rateLimitExceeded` gave up after one
    /// attempt while a 429 walked the whole ladder.
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
