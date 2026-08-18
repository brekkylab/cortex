//! The Slack Web API client behind the Slack mount.
//!
//! Three things make this unlike [`NotionVolume`](crate::fs::NotionVolume)'s inline
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
/// carry them any more, so a consumer builds this and hands it to [`SlackFs::new`].
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
    let allowed = base_url.and_then(|b| {
        reqwest::Url::parse(b)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
    });
    let host = url.host_str().unwrap_or_default().to_string();
    let ok = match &allowed {
        // Mock/gateway deployment: the file host is the configured origin.
        Some(h) => &host == h,
        None => host == "slack.com" || host.ends_with(".slack.com"),
    };
    if !ok {
        return Err(SourceError::io(format!(
            "slack file url host {host:?} is not a Slack host; refusing to send token"
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
/// A day's window and the two directory listings each tend to fit in one page. A history
/// *walk* is the one caller bounded by pages rather than by messages, so a larger page
/// would reach further back for the same number of requests — but how much further cannot
/// be measured from here, while the cost is certain: every page of a walk is held in memory
/// at once. Unmeasurable gain, measurable cost, and past Slack's own advice — so the walk
/// asks for the same 200 as everything else.
const PAGE_LIMIT: usize = 200;
/// Pages one listing will walk before truncating. A backstop against an unbounded cursor
/// loop, not a budget — which is why reaching it is reported in the return value rather
/// than logged: this crate is a library, and the caller is the one that can say whether a
/// partial listing is servable.
const MAX_PAGES: usize = 50;

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
    /// [`SlackApiError`] — without it, every permission error and rate limit would read as
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

    /// Walk a cursor-paginated method, collecting `items_key` across pages. Stops at
    /// [`MAX_PAGES`] so a pathological workspace can't paginate without bound; the second
    /// return value is true when it stopped there, so a caller can say so rather than pass
    /// a partial off as the whole.
    async fn paginate(
        &self,
        method: &str,
        params: &[(&str, String)],
        items_key: &str,
    ) -> SourceResult<(Vec<Value>, bool)> {
        self.paginate_upto(method, params, items_key, MAX_PAGES)
            .await
    }

    /// [`Self::paginate`] with a ceiling the caller chooses, for a walk whose cost it
    /// budgets itself rather than one that only needs a backstop.
    async fn paginate_upto(
        &self,
        method: &str,
        params: &[(&str, String)],
        items_key: &str,
        max_pages: usize,
    ) -> SourceResult<(Vec<Value>, bool)> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages {
            let mut p = params.to_vec();
            p.push(("limit", PAGE_LIMIT.to_string()));
            if let Some(c) = &cursor {
                p.push(("cursor", c.clone()));
            }
            let v = self.call(method, &p).await?;
            if let Some(arr) = v.get(items_key).and_then(Value::as_array) {
                out.extend(arr.iter().cloned());
            }
            cursor = v
                .get("response_metadata")
                .and_then(|m| m.get("next_cursor"))
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(String::from);
            if cursor.is_none() {
                return Ok((out, false));
            }
        }
        Ok((out, true))
    }

    /// Conversations of `types` the token can see (`conversations.list`), and whether the
    /// walk was truncated.
    ///
    /// Archived channels are included: archiving closes a channel to new messages and
    /// leaves the old ones readable, so excluding them would drop a finished project's
    /// whole record while it is still there to read.
    pub async fn list_conversations(&self, types: &str) -> SourceResult<(Vec<Value>, bool)> {
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
    ) -> SourceResult<(Vec<Value>, bool)> {
        let (mut msgs, truncated) = self
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
        Ok((msgs, truncated))
    }

    /// A conversation's history from its newest message backwards, at most `max_pages`
    /// pages, returned oldest-first. The flag is true when the walk stopped at that ceiling
    /// — there is older history it did not reach.
    ///
    /// This is how the tree learns which days a conversation *has*. Slack has no endpoint
    /// for that question, and the alternative — a calendar range between `created` and the
    /// newest message — invents a directory for every silent day in between, which on a
    /// long quiet channel is nearly all of them.
    pub async fn scan_history(
        &self,
        channel: &str,
        max_pages: usize,
    ) -> SourceResult<(Vec<Value>, bool)> {
        let (mut msgs, truncated) = self
            .paginate_upto(
                "conversations.history",
                &[("channel", channel.to_string())],
                "messages",
                max_pages,
            )
            .await?;
        msgs.sort_by(|a, b| ts_of(a).total_cmp(&ts_of(b)));
        Ok((msgs, truncated))
    }

    /// A thread: its root followed by every reply, oldest-first.
    pub async fn conversation_replies(
        &self,
        channel: &str,
        ts: &str,
    ) -> SourceResult<(Vec<Value>, bool)> {
        let (mut msgs, truncated) = self
            .paginate(
                "conversations.replies",
                &[("channel", channel.to_string()), ("ts", ts.to_string())],
                "messages",
            )
            .await?;
        msgs.sort_by(|a, b| ts_of(a).total_cmp(&ts_of(b)));
        Ok((msgs, truncated))
    }

    /// Workspace members (`users.list`), bots/deleted/Slackbot included — the volume decides
    /// what to list, and a message's author may well be a bot whose profile the reader
    /// wants to resolve.
    ///
    /// One call answers both naming and the `users/` profiles: the response carries each
    /// member's whole record, not just their id.
    pub async fn list_users(&self) -> SourceResult<(Vec<Value>, bool)> {
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
        size: u64,
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
        if !served_range && bytes.len() as u64 != size {
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

#[cfg(test)]
#[path = "accessor_tests.rs"]
mod accessor_tests;
