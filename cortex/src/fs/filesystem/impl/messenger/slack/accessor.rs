//! The Slack Web API client behind the Slack mount.
//!
//! Three things make this unlike [`NotionFs`](crate::fs::NotionFs)'s inline
//! client:
//!
//! - **Slack signals failure inside a 200** (`{"ok": false, "error": …}`), so a status-only
//!   check reads every failure as an empty success. [`SlackAccessor::call`] is the single
//!   gate that checks `ok` — file downloads aside, which are plain HTTP.
//! - **It reads as a person, not as an app**, so the credential is that person's user
//!   token; see [`SlackConfig::user_token`].
//! - **Tokens don't expire** unless the app opts into rotation, which this client does not
//!   implement and Slack does not let an app undo — a rotating app's mount breaks every 12
//!   hours when its token does.
//!
//! # What is Slack-specific about failing, and what is not
//!
//! Slack's `error` code is the whole basis of the tree's soft-fail set: a bot that was never
//! invited to one channel must not break the tree, while a bad token must not be served as an
//! empty workspace. But that *rule* is the lane's, not Slack's — every messenger in the lane
//! draws the same line, and only the vocabulary on each side of it differs.
//!
//! So [`class_of`] is the entire Slack-specific part: it says which codes reach one
//! conversation, which reach a whole kind, and which are about the credential. What the tree
//! then does with a class, and which errno a filesystem answers with, live once in
//! [`messenger::error`](super::super::error).
//!
//! The code is carried whole rather than folded into a message, because classifying from
//! formatted text is what makes a rename upstream into a silent behaviour change.

use std::time::Duration;

use serde_json::Value;

use super::super::error::{ApiError, ErrorClass, SourceError, SourceResult};
use super::super::retry::{next_wait, retry_after};
use std::io;

/// Credentials for a Slack filesystem.
///
/// Beside the client that uses them, as `NotionConfig` is: there is no declarative spec to
/// carry them any more, so a consumer builds this and hands it to `SlackSource::new`, then mounts the
/// `MessengerFs` around it.
///
/// Both tokens are optional and at least one is required, which [`SlackAccessor::new`] checks
/// — either alone is a usable mount, for the reason [`user_token`](Self::user_token) gives.
///
/// What is deliberately **not** here is the workspace's id and name. A mount is identified by
/// where it is mounted, and carrying Slack's own identifiers alongside would give it a second
/// name that can disagree when a workspace is renamed.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackConfig {
    /// The installing user's token (`xoxp-`), and the one a mount wants.
    ///
    /// The tree is one person's view of their Slack, so reading it as that person is the
    /// point: a bot token narrows it to the bot's own channel memberships, drops DMs
    /// entirely, and is refused `search.messages` outright.
    pub user_token: Option<String>,

    /// The bot token (`xoxb-`), when the install granted bot scopes. A fallback: it serves a
    /// smaller tree rather than none.
    pub bot_token: Option<String>,

    /// Origin to reach Slack at instead of `slack.com`, for a mock or a gateway. `None` =
    /// production. Also narrows where an attachment download may send the token, because a
    /// deployment that serves the API serves its files too.
    pub base_url: Option<String>,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Whether a token is *present* is the thing a log is for — a bot-only install explains
        // a tree with no DMs — and it is not the secret.
        let held = |t: &Option<String>| if t.is_some() { "[redacted]" } else { "None" };
        f.debug_struct("SlackConfig")
            .field("user_token", &held(&self.user_token))
            .field("bot_token", &held(&self.bot_token))
            .field("base_url", &self.base_url)
            .finish()
    }
}

const SLACK_API: &str = "https://slack.com/api";

/// The API origin for a config: real Slack by default, or `{base_url}/slack/api` when set
/// — the mock/gateway layout.
fn api_base(base_url: Option<&str>) -> String {
    match base_url {
        Some(b) => format!("{}/slack/api", b.trim_end_matches('/')),
        None => SLACK_API.to_string(),
    }
}

/// The one gate on where an attachment download may send the mount's token.
///
/// `url_private_download` comes from API data, so a message's file metadata naming some
/// other host would otherwise leak the token to it. A `base_url` deployment (mock or
/// gateway) serves files from its own origin, which is then the only host allowed.
///
/// A free function so it can be tested for what it decides rather than by watching a
/// request fail to leave: every url it accepts is one the caller then sends a token to,
/// which is not a thing a unit test should be doing.
fn check_file_host(url: &reqwest::Url, base_url: Option<&str>) -> SourceResult<()> {
    // An *origin*, not a host: scheme, host and port together. A host check alone accepts
    // `http://files.slack.com/…`, and the token then crosses the wire in clear — which is a
    // whole Slack account to anyone on the path. In gateway mode it also accepts every port on
    // the gateway's host, which turns one attacker-supplied file URL into an authenticated GET
    // against whatever else is listening there.
    //
    // The URL is API data — `url_private_download` out of a message's file metadata — so this
    // is the one place a hostile response is stopped from choosing where the credential goes.
    let port = |u: &reqwest::Url| u.port_or_known_default();
    let ok = match base_url.and_then(|b| reqwest::Url::parse(b).ok()) {
        // Mock or gateway deployment: the configured origin decides, so a plain-HTTP local
        // mock still works — the operator chose it, and only for the origin they named.
        Some(base) => {
            url.scheme() == base.scheme()
                && url.host_str().is_some()
                && url.host_str() == base.host_str()
                && port(url) == port(&base)
        }
        // Slack itself is always HTTPS on the default port. The leading dot is what keeps
        // `evilslack.com` and `slack.com.evil.test` out.
        None => {
            url.scheme() == "https"
                && port(url) == Some(443)
                && url
                    .host_str()
                    .is_some_and(|h| h == "slack.com" || h.ends_with(".slack.com"))
        }
    };
    if !ok {
        // The origin and not the URL: a path or query is not this gate's business, and an
        // error is a place a credential must never appear.
        return Err(SourceError::io(format!(
            "slack file url origin {}://{}{} is not one this mount may send the token to",
            url.scheme(),
            url.host_str().unwrap_or("<no host>"),
            url.port().map(|p| format!(":{p}")).unwrap_or_default(),
        )));
    }
    Ok(())
}

/// Items per page requested. Slack caps `conversations.history` at 999 (the general
/// pagination ceiling is 1000 and "may vary per method") and recommends no more than 200 —
/// and either way it is only a request, which the docs say plainly: fewer may come back
/// "even if the end of the conversation history hasn't been reached", and a rate-limited
/// app gets 15 objects per response regardless. Nothing here may assume a full page.
///
/// A day's window and the two directory listings each tend to fit in one page, so for most of
/// what this client asks the size decides nothing. Where it decides something is a walk long
/// enough to page, and there it trades requests against nothing else: every page is held until
/// the walk ends either way, so the peak is the whole listing whatever the page size was. A
/// larger page would spend fewer requests on it — and Slack advises against one, which settles
/// a trade with nothing on the other side.
const PAGE_LIMIT: usize = 200;
/// Pages a walk will follow before deciding the cursor is broken rather than long.
///
/// Not a budget. A budget stops early and hands back part of an answer as if it were the
/// whole one, which is the failure this lane exists to refuse — a listing short by two
/// thousand channels is indistinguishable from a workspace that has two thousand fewer. The
/// only thing that legitimately ends a cursor walk is running out of cursor, so this is set
/// where nothing real can reach it: two million items at [`PAGE_LIMIT`], where the largest
/// Enterprise Grid roster is a few hundred thousand. Reaching it means a server handed out
/// cursors forever, which is a fault and answers as one.
const PAGE_GUARD: usize = 10_000;

/// Errors that mean "this conversation is not readable by this token", as opposed to "the
/// request was wrong" or "Slack is down". A bot that was never invited to one channel must
/// not break the whole tree, so these classify as [`ErrorClass::ConversationDenied`] and the
/// tree serves that one conversation empty.
///
/// `not_authed`/`invalid_auth` are deliberately **not** here: those are about the token
/// itself, so every conversation would fail the same way and absorbing them would present an
/// empty workspace as a complete one. Neither is `missing_scope`, which governs a whole
/// *kind* of conversation — see [`ErrorClass::ScopeMissing`].
const CONVERSATION_DENIED: &[&str] = &[
    "not_in_channel",
    "channel_not_found",
    // Defence only: the read methods do not return this. Archiving closes a channel to new
    // messages and leaves the old ones readable — do not read this entry as a reason to
    // keep archived channels out of the tree.
    "is_archived",
    "restricted_action",
    "no_permission",
];

/// Codes about the credential itself rather than about anything it was asked for.
const UNAUTHENTICATED: &[&str] = &[
    "not_authed",
    "invalid_auth",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "ekm_access_denied",
];

/// A construction failure, as the error `new` answers with. Building a store is not a read of
/// one, so it does not go through [`SourceError`].
fn other(msg: impl std::fmt::Display) -> io::Error {
    io::Error::other(msg.to_string())
}

/// Slack's vocabulary in the lane's terms.
///
/// This function is the whole of what is Slack-specific about failing: how far each class
/// reaches, what the tree does about it and which errno a filesystem answers with are written
/// once in `messenger/error.rs`, so a source added later inherits the policy instead of
/// copying it.
fn class_of(code: &str) -> ErrorClass {
    if CONVERSATION_DENIED.contains(&code) {
        ErrorClass::ConversationDenied
    } else if code == "missing_scope" {
        ErrorClass::ScopeMissing
    } else if UNAUTHENTICATED.contains(&code) {
        ErrorClass::Unauthenticated
    } else {
        ErrorClass::Other
    }
}

pub struct SlackAccessor {
    client: reqwest::Client,
    /// For file bytes only: refuses redirects (see [`SlackAccessor::download_file`]).
    files: reqwest::Client,
    config: SlackConfig,
    /// Resolved API origin — real Slack or the config's `base_url` (see [`api_base`]).
    api_base: String,
}

impl SlackAccessor {
    pub fn new(config: &SlackConfig) -> io::Result<Self> {
        // One credential is the minimum: with neither, every call would 401 and the mount
        // would present an empty workspace as a complete one. Rejecting here makes it a
        // realization failure with a clear cause instead.
        if token(&config.user_token).is_none() && token(&config.bot_token).is_none() {
            return Err(other(
                "slack volume has neither a user token nor a bot token",
            ));
        }
        Ok(Self {
            // Bound every request: a hung upstream call answered behind a filesystem op
            // would otherwise wedge that op (and any process touching the mount) forever.
            // A timeout makes it recoverable.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(other)?,
            // Separate so refusing redirects does not change an API call.
            files: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(other)?,
            config: config.clone(),
            api_base: api_base(config.base_url.as_deref()),
        })
    }

    /// Whether this mount could search at all. Slack refuses `search.messages` to bot
    /// tokens, so a bot-only mount has no search — callers ask this rather than discovering
    /// it from an error.
    ///
    /// A user token makes search *possible*, not certain: whether the install actually
    /// granted `search:read` is not visible in the token, so a call can still come back
    /// `missing_scope`.
    pub fn search_available(&self) -> bool {
        token(&self.config.user_token).is_some()
    }

    /// The token every call uses: the user's own, falling back to the bot's.
    ///
    /// Deliberately not per-method. The mount is one person's view of their Slack, so
    /// reading it as that person is the point — a bot token would silently narrow the tree
    /// to the bot's own memberships and drop DMs entirely. The fallback exists only for a
    /// bot-only install.
    fn token(&self) -> &str {
        token(&self.config.user_token)
            .or_else(|| token(&self.config.bot_token))
            // `new` rejects a config with neither, so one is always present.
            .unwrap_or_default()
    }

    /// Call one Slack method and return its (`ok: true`) body.
    ///
    /// The single gate on the API. Slack answers HTTP 200 with `{"ok": false, "error": …}`
    /// for application errors, so this checks `ok` and turns a failure into a typed
    /// [`ApiError`] — without it, every permission error and rate limit would read as
    /// a successful empty response. A 429 or 5xx (transport-level) and `error:
    /// "ratelimited"` (application-level) both retry with bounded backoff.
    async fn call(&self, method: &str, params: &[(&str, String)]) -> SourceResult<Value> {
        // Not a `From` impl: `url` is `reqwest`'s dependency, not ours, and taking it as a
        // direct one to name its error type would be a dependency for one conversion.
        let url = reqwest::Url::parse_with_params(
            &format!("{}/{method}", self.api_base),
            params.iter().map(|(k, v)| (*k, v.as_str())),
        )
        .map_err(SourceError::io)?;
        let token = self.token();
        let mut retries = 0u32;
        let mut long_spent = false;
        loop {
            let resp = self
                .client
                .get(url.clone())
                .bearer_auth(token)
                .send()
                .await?;
            let status = resp.status();
            // Read before the body is consumed below, because the JSON path could not see it
            // afterwards. Documented on the 429; whether it also rides the in-band
            // `ratelimited` reply is not, so this reads it either way and the absent case
            // falls back to the exponential path rather than assuming.
            let asked = retry_after(&resp);
            if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && let Some((wait, long)) = next_wait(asked, retries, long_spent)
            {
                if long {
                    long_spent = true;
                } else {
                    retries += 1;
                }
                tokio::time::sleep(wait).await;
                continue;
            }
            let v: Value = resp.error_for_status()?.json().await?;
            if v.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(v);
            }
            let code = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_string();
            // Slack also reports a rate limit in-band (200 + ok:false); retry it on the same
            // budget, honouring a `Retry-After` if one came with it and backing off if not.
            if code == "ratelimited"
                && let Some((wait, long)) = next_wait(asked, retries, long_spent)
            {
                if long {
                    long_spent = true;
                } else {
                    retries += 1;
                }
                tokio::time::sleep(wait).await;
                continue;
            }
            // `missing_scope` carries what was needed vs granted — the difference between a
            // mystery and an actionable message.
            let detail = match (v.get("needed"), v.get("provided")) {
                (Some(n), p) => Some(format!(
                    "needed: {}; provided: {}",
                    n.as_str().unwrap_or("?"),
                    p.and_then(Value::as_str).unwrap_or("(none)")
                )),
                // Otherwise whatever `Retry-After` said, which is what a caller giving up
                // while throttled needs to tell a wait from a dead mount.
                _ => asked.map(|d| format!("retry in {}s", d.as_secs())),
            };
            return Err(SourceError::Api(ApiError {
                op: method.to_string(),
                class: class_of(&code),
                code,
                detail,
            }));
        }
    }

    /// Walk a cursor-paginated method to its end, collecting `items_key` across pages.
    ///
    /// To its *end*: there is no page ceiling to report, because a ceiling reports a partial
    /// listing as a whole one and nothing downstream could tell the difference. What ends the
    /// walk is Slack running out of cursor — or [`PAGE_GUARD`], which is a fault.
    async fn paginate(
        &self,
        method: &str,
        params: &[(&str, String)],
        items_key: &str,
    ) -> SourceResult<Vec<Value>> {
        walk(PAGE_GUARD, method, |cursor| async move {
            let mut p = params.to_vec();
            p.push(("limit", PAGE_LIMIT.to_string()));
            if let Some(c) = cursor {
                p.push(("cursor", c));
            }
            let v = self.call(method, &p).await?;
            Ok((page_items(&v, items_key), next_cursor(&v)))
        })
        .await
    }

    /// Conversations of `types` the token can see (`conversations.list`).
    ///
    /// Archived channels are included: archiving closes a channel to new messages and
    /// leaves the old ones readable, so excluding them would drop a finished project's
    /// whole record while it is still there to read.
    pub async fn list_conversations(&self, types: &str) -> SourceResult<Vec<Value>> {
        self.paginate(
            "conversations.list",
            &[("types", types.to_string())],
            "channels",
        )
        .await
    }

    /// Top-level messages in `channel` between `oldest` and `latest` (unix seconds as
    /// Slack's string ts), oldest-first.
    ///
    /// `conversations.history` returns thread **roots** and standalone messages only; a
    /// root's replies need [`Self::conversation_replies`]. That split is why the mount
    /// serves them as separate files.
    pub async fn conversation_history(
        &self,
        channel: &str,
        oldest: &str,
        latest: &str,
    ) -> SourceResult<Vec<Value>> {
        let mut msgs = self
            .paginate(
                "conversations.history",
                &[
                    ("channel", channel.to_string()),
                    ("oldest", oldest.to_string()),
                    ("latest", latest.to_string()),
                    ("inclusive", "true".to_string()),
                ],
                "messages",
            )
            .await?;
        msgs.sort_by(|a, b| ts_of(a).total_cmp(&ts_of(b)));
        Ok(msgs)
    }

    /// A thread: its root followed by every reply, oldest-first.
    pub async fn conversation_replies(
        &self,
        channel: &str,
        ts: &str,
    ) -> SourceResult<Vec<Value>> {
        let mut msgs = self
            .paginate(
                "conversations.replies",
                &[("channel", channel.to_string()), ("ts", ts.to_string())],
                "messages",
            )
            .await?;
        msgs.sort_by(|a, b| ts_of(a).total_cmp(&ts_of(b)));
        Ok(msgs)
    }

    /// Workspace members (`users.list`), bots/deleted/Slackbot included — the volume decides
    /// what to list, and a message's author may well be a bot whose profile the reader
    /// wants to resolve.
    ///
    /// One call answers both naming and the `users/` profiles: the response carries each
    /// member's whole record, not just their id.
    pub async fn list_users(&self) -> SourceResult<Vec<Value>> {
        self.paginate("users.list", &[], "members").await
    }

    /// The app name behind a `bot_id` (`bots.info`). An incoming webhook's message carries
    /// that id and nothing else to name it by, so this is the only way. Empty when Slack
    /// reports no name.
    pub async fn bot_info(&self, bot_id: &str) -> SourceResult<String> {
        let v = self
            .call("bots.info", &[("bot", bot_id.to_string())])
            .await?;
        Ok(v.get("bot")
            .and_then(|b| b.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// One user (`users.info`), for an id the member list didn't cover — a DM partner
    /// outside the workspace's own member list.
    pub async fn user_info(&self, user: &str) -> SourceResult<Value> {
        let v = self
            .call("users.info", &[("user", user.to_string())])
            .await?;
        Ok(v.get("user").cloned().unwrap_or(Value::Null))
    }

    /// Download a Slack-hosted file. `url` comes from a message's `files[]`
    /// (`url_private_download`) and needs the bearer token: without an accepted one the CDN
    /// redirects to a web login answering 200 with HTML, which is no error and would be
    /// served as the file. So redirects are refused, and the body must be `size` bytes
    /// unless the status is 206 — only a served range may be short.
    ///
    /// `range` maps to an HTTP `Range` header; a 200 means it was ignored and the caller
    /// slices the whole body itself, which the second return value reports.
    pub async fn download_file(
        &self,
        url: &str,
        range: Option<std::ops::Range<u64>>,
        size: Option<u64>,
    ) -> SourceResult<(Vec<u8>, bool)> {
        // A read of no bytes needs no request. The inclusive-end conversion below cannot
        // express one — `8..8` becomes `bytes=8-8`, which asks for the single byte the
        // caller said it did not want, and a 206 would hand it back unsliced while an
        // in-process slice returns nothing for the same input.
        if let Some(r) = &range
            && r.end <= r.start
        {
            return Ok((Vec::new(), true));
        }
        let parsed = reqwest::Url::parse(url).map_err(SourceError::io)?;
        check_file_host(&parsed, self.config.base_url.as_deref())?;
        // Same credential as the API calls: a file the person can see in Slack is one their
        // own token can fetch.
        let mut req = self.files.get(parsed).bearer_auth(self.token());
        let ranged = range.is_some();
        if let Some(r) = &range {
            // Inclusive-end, per HTTP: a 0..10 read asks for bytes 0-9.
            req = req.header(
                reqwest::header::RANGE,
                format!("bytes={}-{}", r.start, r.end.saturating_sub(1).max(r.start)),
            );
        }
        let resp = req.send().await?.error_for_status()?;
        let status = resp.status();
        if status.is_redirection() {
            // `error_for_status` passes a 3xx, so this has to say so itself.
            return Err(SourceError::io(format!(
                "slack file url redirected ({status}); the token was not accepted"
            )));
        }
        // 206 means the range was applied; a 200 to a ranged request means it wasn't, and
        // the caller must slice the full body itself.
        let served_range = ranged && status == reqwest::StatusCode::PARTIAL_CONTENT;
        let bytes = resp.bytes().await?.to_vec();
        // Only against a length the listing actually gave. `None` is a platform that does not
        // say one, and comparing against nothing would either pass everything or fail
        // everything — see `FileRef::size` for what that forfeits.
        if let Some(size) = size
            && !served_range
            && bytes.len() as u64 != size
        {
            return Err(SourceError::io(format!(
                "slack file download was {} bytes, not the listed {size}",
                bytes.len()
            )));
        }
        Ok((bytes, served_range))
    }

    /// Message search over Slack's own index (`search.messages`), which reaches inside
    /// files Slack has indexed. Needs the user token — see [`Self::search_available`].
    pub async fn search_messages(&self, query: &str, count: usize) -> SourceResult<Value> {
        if !self.search_available() {
            return Err(SourceError::io(
                "slack search needs a user token (xoxp-); this volume was configured with a \
                 bot token only",
            ));
        }
        self.call(
            "search.messages",
            &[
                ("query", query.to_string()),
                ("count", count.to_string()),
                ("sort", "timestamp".to_string()),
            ],
        )
        .await
    }
}

/// A configured token, treating an empty string as absent (a round-tripped config can carry
/// `""` where a scope was never granted).
fn token(t: &Option<String>) -> Option<&str> {
    t.as_deref().filter(|t| !t.is_empty())
}

/// A message's `ts` as a float (0.0 when absent/unparseable), for ordering.
fn ts_of(m: &Value) -> f64 {
    m.get("ts")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// The items of one page, by the key the method keeps them under.
///
/// A key that is absent or is not an array reads as an empty page rather than a failure: Slack
/// answers `ok: true` with the key missing for a conversation that has nothing, and a source
/// that treated the two as the same would turn every quiet channel into an error.
fn page_items(v: &Value, items_key: &str) -> Vec<Value> {
    v.get(items_key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Where the next page is, or `None` when this was the last.
///
/// Slack signals the end two ways and both have to read as the end: the member is absent, or it
/// is present and empty. Reading an empty string as a cursor asks for page one again forever,
/// which is why the emptiness check is here rather than left to a caller to remember.
fn next_cursor(v: &Value) -> Option<String> {
    v.get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(String::from)
}

/// Follow a cursor to its end, or fail.
///
/// There is no third outcome, and the reason is what "absent" means in the tree this feeds. A
/// day with no messages is not an empty file but no name at all, and a conversation the
/// listing did not mention is unreachable by path as well. So a listing that came back short
/// does not read as short — it reads as "those days held nothing" and "that channel does not
/// exist", which is a wrong answer rather than an incomplete one. A caller cannot correct for
/// it either, because nothing in a directory can say that it is not all of it.
///
/// The pages already collected are therefore dropped along with the failure. That is a real
/// cost — a walk that dies on page 41 of 60 spent forty requests to report an error — and it
/// is the cheaper of the two mistakes: the caller learns that it did not work and can ask
/// again, where a short answer teaches it something untrue and it stops asking.
///
/// `page` is handed the cursor to ask with and answers with that page's items and the cursor
/// after it. Where a service keeps its cursor, and what a request looks like, stays in the
/// closure — which is what would let a second service reuse this.
async fn walk<F, Fut>(guard: usize, method: &str, mut page: F) -> SourceResult<Vec<Value>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = SourceResult<(Vec<Value>, Option<String>)>>,
{
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..guard {
        let (items, next) = page(cursor.take()).await?;
        // Before the cursor check, so a last page's items are kept rather than dropped with
        // the cursor that ended.
        out.extend(items);
        match next {
            None => return Ok(out),
            Some(c) => cursor = Some(c),
        }
    }
    Err(SourceError::io(format!(
        "{method}: {guard} pages and the cursor had not ended"
    )))
}

#[cfg(test)]
#[path = "accessor_tests.rs"]
mod accessor_tests;
