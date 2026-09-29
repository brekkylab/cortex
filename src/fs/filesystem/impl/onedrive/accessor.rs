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
/// Two hosts, because Graph is one API: files, metadata and search all live under
/// `graph.microsoft.com`. Whatever is set here is an *origin* —
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
/// nothing else. Joined at request time, so the mask cannot grow a stray space or lose a
/// comma.
///
/// A driveItem states `size` exactly for every file, `.docx`, `.xlsx` and `.pptx`
/// included, which is why this store has no placeholder, no padding and no
/// remembered-length map.
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
/// [`OnedriveAccessor::get_item_by_id`] selects the same way, so a listing without the URL
/// and the refetch meant to rescue it both come back without one.
const DOWNLOAD_URL_SELECT: &str = "content.downloadUrl";

/// The instance annotation a download URL actually arrives under. See
/// [`DOWNLOAD_URL_SELECT`], which is spelled differently on purpose.
pub(super) const DOWNLOAD_URL_KEY: &str = "@microsoft.graph.downloadUrl";

/// Hard cap on listing pages so a duplicate or looping `@odata.nextLink` cannot spin
/// forever.
const MAX_PAGES: usize = 50;

/// Retry budget for a throttled or 5xx request.
const MAX_RETRIES: u32 = 5;
/// Ceiling on the *base* of a backoff this code computed, which is ours to shorten. Jitter
/// rides on top of it rather than being clamped away by it; see [`backoff_delay`].
const MAX_BACKOFF: Duration = Duration::from_secs(16);
/// Jitter added to a backoff, so two callers throttled together do not wake together.
const JITTER_MAX_MS: u64 = 1000;

/// How long one call may spend *waiting* across the whole retry ladder.
///
/// A different kind of number from [`MAX_BACKOFF`], and not interchangeable with it.
/// Microsoft's guidance is to wait exactly what `Retry-After` says, because usage keeps
/// accruing while a client is throttled: coming back early makes the throttle last longer.
/// So there is no "retry sooner" option, only waiting as told or giving up — and shortening
/// the wait while keeping the retry is the one combination worse than either, since it
/// spends the retry budget inside the window and extends it on every attempt.
///
/// Waiting cannot be unbounded either. A FUSE op is a synchronous callback and the session
/// loop is single-threaded, so a sleep here is not one blocked process but the whole mount
/// not answering anybody.
///
/// A budget for the ladder rather than a cap on one sleep, because what blocks the mount is
/// their **sum**: `MAX_RETRIES` waits of 30 s each is 150 s, which is what a per-sleep cap
/// would permit while claiming a 30 s ceiling.
///
/// It bounds the waiting and not the whole call: each attempt may still spend up to the
/// `reqwest` request timeout set in [`OnedriveAccessor::new`] on the wire. Requests are
/// progress; sleeping is not.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

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
    /// When the service last said to stop asking, and until when. See
    /// [`refuse_while_throttled`](Self::refuse_while_throttled).
    throttled_until: Mutex<Option<Instant>>,
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
            throttled_until: Mutex::new(None),
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
    /// Graph reports throttling as `429` with `Retry-After` and means it, so no other
    /// status needs classifying as a rate limit. A wait it asks for is
    /// honoured as asked or not taken at all; see [`MAX_RETRY_AFTER`] for why there is no
    /// third option. A backoff this code computed for itself is capped at [`MAX_BACKOFF`],
    /// which is a different thing and safe to shorten.
    async fn send_retrying(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
        max_retries: u32,
    ) -> anyhow::Result<reqwest::Response> {
        self.refuse_while_throttled().await?;
        let mut token = self.token().await?;
        let mut refreshed = false;
        let mut retries = 0u32;
        let mut slept = Duration::ZERO;
        loop {
            let resp = build(&token).send().await?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                *self.access_token.lock().await = None;
                token = self.token().await?;
                refreshed = true;
                continue;
            }
            let throttled = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            let asked = retry_after(resp.headers());
            if (throttled || status.is_server_error()) && retries < max_retries {
                let Some(wait) = next_wait(asked, slept, retries) else {
                    // More than this call may spend waiting. Reported rather than slept
                    // through, and the service told to expect nothing from us meanwhile.
                    self.pause_until(Instant::now() + asked.unwrap_or_default())
                        .await;
                    return Ok(resp);
                };
                retries += 1;
                slept += wait;
                tokio::time::sleep(wait).await;
                continue;
            }
            // Out of retries on a throttle is the same answer as refusing to wait one: what
            // follows must not be more requests.
            if throttled && let Some(d) = asked {
                self.pause_until(Instant::now() + d).await;
            }
            return Ok(resp);
        }
    }

    /// Refuse to send at all while the service has told us to wait.
    ///
    /// Giving up on a throttle is only half of what Microsoft asks for. Its instruction is
    /// to *pause the client* — "failure to honor Retry-After may result in more throttling
    /// ... even though the calls fail, they still count toward usage limits" — so a
    /// give-up that leaves the next call free to fire immediately is worse than waiting.
    /// Measured without this gate: twenty listings under a 900 s throttle sent twenty
    /// requests in 14 ms, against the very limit that caused the throttle.
    ///
    /// A mount is the hostile case for this. Its callers re-ask constantly — Finder, the
    /// NFS client under FUSE-T, a `find` walking a thousand directories — and a failed
    /// listing is deliberately not cached, so every re-ask would be a fresh request.
    async fn refuse_while_throttled(&self) -> anyhow::Result<()> {
        let until = *self.throttled_until.lock().await;
        if let Some(until) = until
            && let Some(left) = until.checked_duration_since(Instant::now())
        {
            anyhow::bail!("onedrive: throttled, {} s left to wait", left.as_secs());
        }
        Ok(())
    }

    /// Hold every request until `until`. Never shortens a pause already in force.
    async fn pause_until(&self, until: Instant) {
        let mut guard = self.throttled_until.lock().await;
        if guard.is_none_or(|current| until > current) {
            *guard = Some(until);
        }
    }

    async fn send_with_refresh(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        self.send_retrying(build, MAX_RETRIES).await
    }

    /// A GET whose body is JSON, bounded on the way in.
    ///
    /// A failure keeps reqwest's own error underneath, so the *status* survives as a
    /// status and a caller can ask what it was rather than search the message for digits.
    /// Graph's error body goes on top as context, because that is where the AADSTS code
    /// and the correlation id are, and neither is recoverable from a status.
    ///
    /// Safe to let reqwest's message name the URL here: a Graph URL carries a path and a
    /// `$select`, and the credential travels in a header. The download host is the other
    /// way round, which is why [`download`](Self::download) does not do this.
    async fn get_json(&self, url: &str) -> anyhow::Result<Value> {
        let resp = self
            .send_with_refresh(|t| self.client.get(url).bearer_auth(t))
            .await?;
        if let Err(failed) = resp.error_for_status_ref() {
            // `unwrap_or_default` and not `?`: the status is what a caller classifies on,
            // so it has to survive a body that will not read. Propagating the body's error
            // instead would drop the status, and a 404 whose body was cut off would stop
            // being an absence.
            let body = body_within(resp, MAX_BODY_BYTES, "error")
                .await
                .unwrap_or_default();
            return Err(anyhow::Error::new(failed).context(format!(
                "graph: {}",
                first_chars(&String::from_utf8_lossy(&body), 300)
            )));
        }
        let raw = body_within(resp, MAX_BODY_BYTES, "listing").await?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// The Graph address of a folder's children.
    ///
    /// Paths are native, so `/me/drive/root:/A/B:/children` answers in one request rather
    /// than one listing per directory walked.
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

    /// One folder's children, addressed by path.
    pub async fn list_children(&self, path: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        self.pages_from(self.children_url(path), limit).await
    }

    /// The same listing addressed by the folder's id rather than by its path.
    ///
    /// The slow half of resolution, for the paths the fast half cannot spell. A path is
    /// one string and the service answers to one normalization of it; an id is the
    /// service's own. See [`OnedriveFs::list_dir`](super::OnedriveFs::list_dir).
    pub async fn list_children_of_id(&self, id: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let select = format!("{},{}", ITEM_FIELDS.join(","), DOWNLOAD_URL_SELECT);
        let url = format!(
            "{}/me/drive/items/{}/children?$select={select}&$top=200",
            self.urls.graph,
            encode_segment(id)
        );
        self.pages_from(url, limit).await
    }

    /// Follow `@odata.nextLink` from `url` until the folder ends or `limit` is reached.
    async fn pages_from(&self, url: String, limit: usize) -> anyhow::Result<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();
        let mut url = url;
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

    /// One item by id, for the case a listing cannot answer: a fresh download URL after
    /// the cached one has expired.
    ///
    /// By id rather than by path, and that is load-bearing rather than tidy. A path has
    /// two Unicode spellings and the service answers only to the one it stored — measured
    /// against the live service, `root:/문서:/children` is `200` composed and `404`
    /// decomposed — while an id is opaque ASCII the service itself minted. This refresh
    /// happens mid-read, where a `404` would surface as a file that vanished halfway
    /// through, so it is the last place that should depend on a spelling. It also survives
    /// a rename between the listing and the read, which a path does not.
    pub async fn get_item_by_id(&self, id: &str) -> anyhow::Result<Value> {
        let select = format!("{},{}", ITEM_FIELDS.join(","), DOWNLOAD_URL_SELECT);
        let url = format!(
            "{}/me/drive/items/{}?$select={select}",
            self.urls.graph,
            encode_segment(id)
        );
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
        // `without_url`: reqwest attaches the request URL to a transport error and
        // `Display` prints its query string, and *this* URL's query string is the grant. A refused connection or a
        // timeout would otherwise put a token that hands over the file into whatever reads
        // the error. The error's kind survives, so `is_timeout` and `is_connect` still work.
        let resp = req.send().await.map_err(reqwest::Error::without_url)?;
        let status = resp.status();
        // Past the end of the file. A walk that runs off the end asks for this, and it is
        // an ordinary end rather than an error.
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok((range.map(|r| r.start).unwrap_or(0), Vec::new()));
        }
        if !status.is_success() {
            // The status and nothing else, deliberately. `error_for_status` names the URL
            // in its message, and *this* URL is preauthenticated: the token that grants
            // the file is in its query string, so an error carrying it into a log hands
            // the file to whoever reads the log.
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

/// The wait to take before retrying, or `None` when this call has no more waiting to give.
///
/// Kept apart from [`OnedriveAccessor::send_retrying`] because it is the whole of the
/// policy and it is arithmetic: the ladder around it only sleeps and counts.
///
/// `slept` is what the call has already spent, which is what makes [`MAX_RETRY_AFTER`] a
/// budget for the ladder rather than a cap on one sleep. The two agree on a single long
/// wait and diverge on a run of short ones — five waits of seven seconds is thirty-five,
/// which a per-sleep cap permits while claiming a thirty-second ceiling.
fn next_wait(asked: Option<Duration>, slept: Duration, retries: u32) -> Option<Duration> {
    match asked {
        Some(d) if slept + d > MAX_RETRY_AFTER => None,
        Some(d) => Some(d),
        None => Some(backoff_delay(retries)),
    }
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
    // The cap applies to the base, then jitter rides on top. Capping the sum instead
    // discards the jitter exactly when two callers are most likely to collide: at
    // `MAX_RETRIES`, `base` already equals `MAX_BACKOFF`, so `(base + jitter).min(cap)` is
    // `cap` for every caller and they wake together — which is what this exists to prevent.
    Duration::from_secs(base)
        .min(MAX_BACKOFF)
        .saturating_add(Duration::from_millis(jitter))
}

/// `Retry-After` as a duration, or `None` when the header states no wait worth taking.
///
/// Delta-seconds only. The HTTP-date form is legal and Graph does not send it, and a date
/// parsed against a skewed clock is worse than a backoff.
///
/// A zero is `None` rather than a zero wait. "Avoid immediate retries, because all requests
/// accrue against your usage limits" is the one thing the guidance names outright, and a
/// header of `0` from a gateway would otherwise produce exactly that: a full ladder of
/// requests with no delay between them at all.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each service keeps its official path under any origin and moves only when told. The
    /// mock moves both together, so only this can see one dragging the other along.
    #[test]
    fn each_service_keeps_its_official_path_under_any_origin() {
        const TOKEN: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
        let at = |o: OnedriveOrigins| {
            let e = endpoints(&o);
            (e.token, e.graph)
        };
        let graph = Some("http://127.0.0.1:9/g".to_string());
        assert_eq!(
            at(Default::default()),
            (TOKEN.into(), "https://graph.microsoft.com/v1.0".into())
        );
        let moved = at(OnedriveOrigins {
            graph,
            ..Default::default()
        });
        assert_eq!(moved, (TOKEN.into(), "http://127.0.0.1:9/g/v1.0".into()));
    }

    /// A path is encoded segment by segment: separators survive, everything else does not.
    #[test]
    fn a_path_is_encoded_by_segment() {
        assert_eq!(encode_path("/a/b c/d"), "a/b%20c/d");
        assert_eq!(
            encode_path("보고/서.pdf"),
            "%EB%B3%B4%EA%B3%A0/%EC%84%9C.pdf"
        );
        assert_eq!([encode_path("a?b"), encode_path("a#b")], ["a%3Fb", "a%23b"]);
    }

    /// A config's `Debug` does not print the secrets it holds.
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

    /// The retry policy is a budget for the ladder, not a cap on one sleep: a run of waits that
    /// each fit stops once their sum would not. See [`next_wait`].
    #[test]
    fn a_wait_is_taken_only_while_the_ladder_can_still_afford_it() {
        let s = Duration::from_secs;
        let all = MAX_RETRY_AFTER;
        assert_eq!(next_wait(Some(s(10)), s(0), 0), Some(s(10)), "as asked");
        assert_eq!(
            next_wait(Some(all), s(0), 0),
            Some(all),
            "the whole budget at once"
        );
        assert_eq!(
            next_wait(Some(all + s(1)), s(0), 0),
            None,
            "past it, refused outright"
        );
        // What a per-sleep cap gets wrong: each of these fits on its own.
        assert_eq!(next_wait(Some(s(7)), s(21), 0), Some(s(7)), "28 <= 30");
        assert_eq!(next_wait(Some(s(7)), s(28), 0), None, "35 > 30");
    }
}
