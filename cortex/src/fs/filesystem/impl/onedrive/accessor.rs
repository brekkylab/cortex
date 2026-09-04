use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use tokio::sync::Mutex;

/// The OAuth origin. One endpoint mints tokens for every Graph service.
pub(crate) const LOGIN_ORIGIN: &str = "https://login.microsoftonline.com";
/// The Graph origin, without the version suffix this code appends.
pub(crate) const GRAPH_ORIGIN: &str = "https://graph.microsoft.com";

/// Where to reach Microsoft's services. `None` = the real host.
///
/// Two hosts rather than Google's five, because Graph is one API: files, metadata and
/// search all live under `graph.microsoft.com`. Whatever is set here is an *origin* —
/// this code appends only the path the official API uses, so the same paths address a
/// mock and production alike.
///
/// **Deployment-level only: the token endpoint receives the app's client secret, so none
/// of this may be user-suppliable.** It derives `Deserialize` for a config file read by
/// whoever runs the mount, not for a field filled in from a request — pointing `login`
/// somewhere is pointing the client secret there.
///
/// The *content* host is deliberately absent. A download URL arrives from Graph itself
/// (`@microsoft.graph.downloadUrl`) and is followed as given; overriding it would mean
/// rewriting a URL the service minted, which a mock does not need — it returns its own.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OnedriveOrigins {
    /// Serves the token endpoint (`{login}/consumers/oauth2/v2.0/token`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Serves Microsoft Graph (`{graph}/v1.0/me/drive/...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
}

impl OnedriveOrigins {
    /// Whether nothing is overridden, so the field can stay out of a serialized config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Both services behind one host, laid out by name: `{host}/login`, `{host}/graph`.
    /// A convenience for a deployment that fronts them, not a substitute for the
    /// per-service knobs.
    pub fn behind(host: &str) -> Self {
        let h = host.trim_end_matches('/');
        Self {
            login: Some(format!("{h}/login")),
            graph: Some(format!("{h}/graph")),
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

/// Every host this accessor talks to, resolved once from a config.
///
/// Each is an origin from [`OnedriveOrigins`] plus the path the official API uses, so
/// nothing here encodes any one deployment's layout: point `OnedriveOrigins::graph` at a
/// gateway and `/v1.0` still follows, exactly as it does against Microsoft.
#[derive(Clone)]
struct Endpoints {
    token: String,
    graph: String,
}

fn endpoints(o: &OnedriveOrigins) -> Endpoints {
    Endpoints {
        // `consumers` and not `common`: this store targets personal Microsoft accounts.
        // A work or school tenant needs `/{tenant}` or `/organizations` here, and brings
        // SharePoint document libraries with it — a different tree, not a config knob.
        token: format!(
            "{}/consumers/oauth2/v2.0/token",
            OnedriveOrigins::origin(&o.login, LOGIN_ORIGIN)
        ),
        graph: format!("{}/v1.0", OnedriveOrigins::origin(&o.graph, GRAPH_ORIGIN)),
    }
}

/// Per-item fields asked of every listing — what the mount needs to shape an entry, and
/// nothing else. One entry per field, joined at request time: the separators are not
/// hand-maintained, so this cannot grow a stray space or a missing comma.
///
/// `size` is the one Google could not give. A driveItem states it exactly for every file
/// including `.docx`, `.xlsx` and `.pptx`, which is why this store has no placeholder, no
/// padding and no remembered-length map.
const ITEM_FIELDS: &[&str] = &[
    "id",
    "name",
    "size",
    "lastModifiedDateTime",
    "createdDateTime",
    // Which of the two an item is. A file has `file`, a folder has `folder`; an item with
    // neither — see `package` below — is neither.
    "file",
    "folder",
    // OneNote notebooks and the like: "a package instead of a folder or file", treated as
    // a folder by some clients and a file by others. Requested so the listing can drop it
    // rather than guess.
    "package",
    // `cTag` changes when content changes, `eTag` when anything does. `Stat` has a field
    // for one of them and a revalidating reader wants the content one.
    "cTag",
    "eTag",
];

/// What `$select` has to name to be given a preauthenticated download URL.
///
/// Not what the response calls it, and the difference is not cosmetic. Measured against the
/// live service: a `$select` naming [`DOWNLOAD_URL_KEY`] is accepted, answers `200`, and the
/// annotation is absent from every row — no error says so. `content.downloadUrl` returns it,
/// still spelled [`DOWNLOAD_URL_KEY`] in the JSON.
///
/// Asked for beside the fields above so a read does not cost a second round trip to learn
/// where the bytes are. Getting it wrong does not degrade to that second trip either:
/// [`OnedriveAccessor::get_item`] selects the same way, so a listing without the URL and
/// the refetch meant to rescue it both come back without one.
const DOWNLOAD_URL_SELECT: &str = "content.downloadUrl";

/// The instance annotation a download URL actually arrives under. See
/// [`DOWNLOAD_URL_SELECT`], which is spelled differently on purpose.
pub(super) const DOWNLOAD_URL_KEY: &str = "@microsoft.graph.downloadUrl";

/// Hard cap on listing pages so a duplicate or looping `@odata.nextLink` cannot spin
/// forever.
const MAX_PAGES: usize = 50;

/// Retry budget for a throttled or 5xx request.
const MAX_RETRIES: u32 = 5;
/// Ceiling on one wait, including one Graph asked for by `Retry-After`.
const MAX_BACKOFF: Duration = Duration::from_secs(16);
/// Jitter added to a backoff, so two callers throttled together do not wake together.
const JITTER_MAX_MS: u64 = 1000;

/// Ceiling on a response body read into memory.
///
/// Bounded while reading rather than after: a JSON listing costs several times its bytes
/// once parsed into a tree, so a body that arrives unbounded is already too late to
/// refuse.
pub(super) const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// What a mount needs to reach one OneDrive account.
///
/// `client_secret` is optional because a personal-account app registration is normally a
/// **public client**, which has no secret — the refresh grant simply omits it. A
/// confidential client sets it and the form gains one field.
#[derive(Clone, Serialize, Deserialize)]
pub struct OnedriveConfig {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub refresh_token: String,
    /// Deployment-level endpoint overrides. See [`OnedriveOrigins`].
    #[serde(default, skip_serializing_if = "OnedriveOrigins::is_default")]
    pub origins: OnedriveOrigins,
}

/// Hand-written so a secret cannot reach a log through a derived `Debug`. The struct is
/// three credentials and a routing table; printing it should say so and no more.
impl std::fmt::Debug for OnedriveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnedriveConfig")
            .field("client_id", &"<redacted>")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("refresh_token", &"<redacted>")
            .field("origins_overridden", &!self.origins.is_default())
            .finish()
    }
}

/// The HTTP half of the store: tokens, retries, and the three calls a read-only tree
/// makes. Knows nothing of paths as the mount means them, of `Stat`, or of `FileSystem` —
/// it speaks `serde_json::Value` and `Vec<u8>`.
pub struct OnedriveAccessor {
    client: reqwest::Client,
    config: OnedriveConfig,
    urls: Endpoints,
    access_token: Mutex<Option<(String, Instant)>>,
}

impl OnedriveAccessor {
    pub fn new(config: &OnedriveConfig) -> anyhow::Result<Self> {
        // Timeouts, because a binding is a synchronous callback over this: a hung upstream
        // wedges the FUSE op and then every process touching the mount.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Ok(Self {
            client,
            config: config.clone(),
            urls: endpoints(&config.origins),
            access_token: Mutex::new(None),
        })
    }

    /// A bearer token, from the cache when one has life left in it.
    ///
    /// The 60-second margin is what avoids the everything-401s-at-once failure: a token
    /// that expires mid-flight takes every concurrent request with it.
    ///
    /// The scope this store needs is `Files.Read offline_access`. Obtaining the refresh
    /// token is not this crate's job — the consent round trip belongs to whatever sets a
    /// mount up.
    async fn token(&self) -> anyhow::Result<String> {
        let mut guard = self.access_token.lock().await;
        if let Some((t, exp)) = guard.as_ref()
            && *exp > Instant::now() + Duration::from_secs(60)
        {
            return Ok(t.clone());
        }
        let mut form: Vec<(&str, &str)> = vec![
            ("client_id", self.config.client_id.as_str()),
            ("refresh_token", self.config.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];
        if let Some(secret) = self.config.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let resp = self
            .client
            .post(&self.urls.token)
            .form(&form)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The body of a *failed* exchange, which Microsoft answers with an error code
            // and a correlation id — no secret of ours is echoed back in it.
            anyhow::bail!(
                "microsoft token exchange {status}: {}",
                first_chars(&body, 300)
            );
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

    /// Send, refreshing the token once on a 401 and backing off on a throttle.
    ///
    /// `build` takes the token and is re-invoked per attempt, which is what lets a
    /// mid-flight refresh reach the retry. Every call this makes is an idempotent GET, so
    /// retrying a 5xx is unconditionally safe.
    ///
    /// Unlike Drive, Graph reports throttling as `429` with `Retry-After` and means it —
    /// there is no 403-that-is-really-a-rate-limit to classify. Microsoft's guidance is to
    /// wait exactly what the header says, because usage keeps accruing while a client is
    /// throttled, so a shorter wait makes the throttle last longer.
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
            let retryable =
                status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if retryable && retries < max_retries {
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

    async fn send_with_refresh(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        self.send_retrying(build, MAX_RETRIES).await
    }

    /// A GET whose body is JSON, bounded on the way in.
    async fn get_json(&self, url: &str) -> anyhow::Result<Value> {
        let resp = self
            .send_with_refresh(|t| self.client.get(url).bearer_auth(t))
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = body_within(resp, MAX_BODY_BYTES, "error").await?;
            anyhow::bail!(
                "graph {status}: {}",
                first_chars(&String::from_utf8_lossy(&body), 300)
            );
        }
        let raw = body_within(resp, MAX_BODY_BYTES, "listing").await?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// The Graph address of a folder's children.
    ///
    /// Paths are native here, which is the difference that removes an entire layer. Drive
    /// has no paths at all, so a four-segment path costs one `files.list` per directory to
    /// walk; Graph answers `/me/drive/root:/A/B:/children` in one request.
    ///
    /// The root is spelled differently from everything under it — `root/children`, not
    /// `root::/children` — because the colon form needs a path between its colons.
    fn children_url(&self, path: &str) -> String {
        let select = format!("{},{}", ITEM_FIELDS.join(","), DOWNLOAD_URL_SELECT);
        let base = if path.is_empty() || path == "/" {
            format!("{}/me/drive/root/children", self.urls.graph)
        } else {
            format!(
                "{}/me/drive/root:/{}:/children",
                self.urls.graph,
                encode_path(path)
            )
        };
        format!("{base}?$select={select}&$top=200")
    }

    /// One folder's children, following `@odata.nextLink` until the folder ends or
    /// `limit` is reached.
    pub async fn list_children(&self, path: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();
        let mut url = self.children_url(path);
        for _ in 0..MAX_PAGES {
            let v = self.get_json(&url).await?;
            if let Some(items) = v.get("value").and_then(|f| f.as_array()) {
                out.extend(items.iter().cloned());
            }
            if out.len() >= limit {
                out.truncate(limit);
                break;
            }
            // The continuation is a whole URL, already carrying the `$select` and the
            // service's own paging token. Rebuilding it from parts would drop the token.
            match v.get("@odata.nextLink").and_then(|n| n.as_str()) {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(out)
    }

    /// One item by path, for the case a listing cannot answer: a fresh download URL after
    /// the cached one has expired.
    pub async fn get_item(&self, path: &str) -> anyhow::Result<Value> {
        let select = format!("{},{}", ITEM_FIELDS.join(","), DOWNLOAD_URL_SELECT);
        let url = if path.is_empty() || path == "/" {
            format!("{}/me/drive/root?$select={select}", self.urls.graph)
        } else {
            format!(
                "{}/me/drive/root:/{}?$select={select}",
                self.urls.graph,
                encode_path(path)
            )
        };
        self.get_json(&url).await
    }

    /// A window of a file's bytes, from a URL Graph minted, with where they start.
    ///
    /// Returns `(at, bytes)` — the offset the bytes actually begin at, which is **not**
    /// necessarily the offset asked for. Microsoft documents that a ranged GET may ignore
    /// the header: *"If the range can't be generated the Range header may be ignored and
    /// an HTTP 200 response would be returned with the full contents of the file."* So a
    /// `200` is a whole file starting at zero, and the caller slices from there. Assuming
    /// the requested offset instead would serve bytes from the front of the file as though
    /// they came from the middle, with nothing to notice it by.
    ///
    /// A `206` states its own start in `Content-Range`, and that is what is returned
    /// rather than what was requested — the two agree in practice, and trusting the
    /// response over the intent is what makes them provably agree here.
    ///
    /// The URL is not this store's to keep: Microsoft calls it short-lived and warns it
    /// "can't be cached". An expired one answers 4xx, which the caller turns into one
    /// refetch. Nothing here retries, because retrying an expired URL cannot help.
    pub async fn download(
        &self,
        url: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> anyhow::Result<(u64, Vec<u8>)> {
        // The download URL is preauthenticated and takes no bearer token. Sending one
        // anyway is what breaks a CORS preflight in a browser and is simply noise here.
        let mut req = self.client.get(url);
        if let Some(r) = &range {
            if r.end <= r.start {
                return Ok((r.start, Vec::new()));
            }
            req = req.header("Range", format!("bytes={}-{}", r.start, r.end - 1));
        }
        let resp = req.send().await?;
        let status = resp.status();
        // Past the end of the file. A walk that runs off the end asks for this, and it is
        // an ordinary end rather than an error.
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok((range.map(|r| r.start).unwrap_or(0), Vec::new()));
        }
        if !status.is_success() {
            anyhow::bail!("onedrive download {status}");
        }
        let at = if range.is_some() {
            if status == reqwest::StatusCode::PARTIAL_CONTENT {
                content_range_start(resp.headers()).ok_or_else(|| {
                    anyhow::anyhow!("onedrive download: 206 without a usable Content-Range")
                })?
            } else {
                // A range was asked for and the whole file came back. Documented, and
                // correct to serve — from zero, which is where it starts.
                0
            }
        } else {
            0
        };
        let bytes = body_within(resp, MAX_BODY_BYTES, "file window").await?;
        Ok((at, bytes))
    }
}

/// The first `n` characters of `s`, on a character boundary.
fn first_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Read a response body, refusing it once it passes `limit`.
///
/// Bounded while reading, not after: `Response::bytes` buffers whatever arrives before it
/// can be inspected, and the tree parsed from a listing costs several times its bytes. The
/// partial buffer is local and dropped with the error, so nothing over the limit is kept.
async fn body_within(
    mut resp: reqwest::Response,
    limit: u64,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() as u64 + chunk.len() as u64 > limit {
            anyhow::bail!("onedrive {what}: over {limit} bytes");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The start offset a `206` states, out of `Content-Range: bytes 0-1023/2048`.
fn content_range_start(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    v.trim()
        .strip_prefix("bytes ")?
        .split_once('-')?
        .0
        .trim()
        .parse()
        .ok()
}

/// Percent-encode a path for the `root:/{path}:` form.
///
/// Segment by segment, so the separators survive: encoding the whole string would turn
/// `/` into `%2F` and address one long name instead of a path. `?` and `#` are what make
/// this load-bearing rather than tidy — either one raw would end the path and start a
/// query, and OneDrive allows neither in a name but a gateway is not obliged to agree.
pub(super) fn encode_path(path: &str) -> String {
    path.trim_matches('/')
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `2^n` seconds plus jitter, capped.
///
/// The jitter comes from the clock rather than a random-number crate: it only has to keep
/// two callers off the same wake-up, which a nanosecond count does.
fn backoff_delay(n: u32) -> Duration {
    let base = 1u64 << n.min(16);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % (JITTER_MAX_MS + 1))
        .unwrap_or(0);
    Duration::from_secs(base)
        .saturating_add(Duration::from_millis(jitter))
        .min(MAX_BACKOFF)
}

/// `Retry-After` as a duration. Delta-seconds only — the HTTP-date form is legal and
/// Graph does not send it, and a date parsed against a skewed clock is worse than a
/// backoff.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two hosts in production, each overridable on its own, and the official path follows
    /// either way — so nothing here depends on how a deployment lays out its paths.
    #[test]
    fn each_service_keeps_its_official_path_under_any_origin() {
        let e = endpoints(&OnedriveOrigins::default());
        assert_eq!(
            e.token,
            "https://login.microsoftonline.com/consumers/oauth2/v2.0/token"
        );
        assert_eq!(e.graph, "https://graph.microsoft.com/v1.0");

        let e = endpoints(&OnedriveOrigins {
            graph: Some("http://127.0.0.1:9/g".into()),
            ..Default::default()
        });
        assert_eq!(e.graph, "http://127.0.0.1:9/g/v1.0", "the override moves");
        assert_eq!(
            e.token, "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
            "and only it moves"
        );
    }

    /// A path addresses a path, not one long name — so the separators survive and
    /// everything else does not.
    #[test]
    fn a_path_is_encoded_by_segment() {
        assert_eq!(encode_path("/a/b c/d"), "a/b%20c/d");
        // UTF-8 percent-encoded byte by byte, and the separator left alone.
        assert_eq!(
            encode_path("보고/서.pdf"),
            "%EB%B3%B4%EA%B3%A0/%EC%84%9C.pdf"
        );
        // The two that would end the path and start something else.
        assert_eq!(encode_path("a?b"), "a%3Fb");
        assert_eq!(encode_path("a#b"), "a%23b");
    }

    /// A secret must not reach a log through a `Debug` nobody meant to call.
    #[test]
    fn a_config_does_not_print_what_it_holds() {
        let cfg = OnedriveConfig {
            client_id: "the-client".into(),
            client_secret: Some("the-secret".into()),
            refresh_token: "the-refresh-token".into(),
            origins: OnedriveOrigins::default(),
        };
        let shown = format!("{cfg:?}");
        for secret in ["the-client", "the-secret", "the-refresh-token"] {
            assert!(!shown.contains(secret), "{shown} leaks {secret}");
        }
        assert!(shown.contains("origins_overridden: false"));
    }

    /// The header a `206` states its own start in.
    #[test]
    fn a_content_range_states_where_the_bytes_begin() {
        let parse = |h: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::CONTENT_RANGE,
                reqwest::header::HeaderValue::from_str(h).unwrap(),
            );
            content_range_start(&headers)
        };
        assert_eq!(
            content_range_start(&reqwest::header::HeaderMap::new()),
            None,
            "a 206 with no Content-Range states nothing"
        );
        assert_eq!(parse("bytes 0-1023/2048"), Some(0));
        assert_eq!(parse("bytes 8388608-16777215/104857600"), Some(8_388_608));
        assert_eq!(parse("pages 1-2/3"), None, "a unit we did not ask for");
        assert_eq!(parse("bytes */2048"), None, "no start to take");
    }
}
