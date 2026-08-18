//! Slack as a [`MessengerSource`]: which call answers which of the lane's questions, and how
//! a Slack message becomes a [`Message`].
//!
//! Thin by design. Everything that is *not* here — the tree, the line format, the retry
//! budget, which errno a failure answers with — is the lane's and is written once. What is
//! left is Slack's own vocabulary: a `ts` that is both a timestamp and a message id, a thread
//! keyed by `thread_ts`, an `im` that has a partner instead of a name.
//!
//! # It reads as a person
//!
//! The credential is the installing user's token, so the tree is one person's view of their
//! Slack — their channels, their DMs, and the history they can already see. This is Slack's
//! own supported shape, not a workaround; see [`SlackConfig::user_token`]. A bot-token install
//! still serves a tree, but a smaller one: the bot's own channel memberships, and no DMs at
//! all, which is what [`capabilities`](SlackSource::capabilities) reports.

use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::BoxFuture;

use super::accessor::{SlackAccessor, SlackConfig};
use super::super::error::SourceResult;
use super::super::source::{
    Author, Capabilities, ConvId, ConvKind, Conversation, FileRef, MessengerSource, Message,
    MsgId, Thread, User, Window,
};

/// Conversation kinds behind each section, widest first.
///
/// Slack lists both kinds of a section in one call, but wants a scope per *type* and fails
/// the whole call when one is missing — `types=public_channel,private_channel` on a token
/// without `groups:read` answers `missing_scope` and no channels at all. So a section that
/// cannot have both drops the second kind and asks again, which is the one place a missing
/// scope is absorbed rather than propagated: the request can be narrowed, so narrowing it is
/// an answer rather than a guess.
const CHANNEL_TYPES: &[&str] = &["public_channel", "private_channel"];
const DM_TYPES: &[&str] = &["im", "mpim"];

pub struct SlackSource {
    api: SlackAccessor,
    /// Whether the install granted a user token. A bot token cannot see DMs at all, so this
    /// decides whether `dms/` exists rather than whether it is empty.
    reads_as_user: bool,
}

impl SlackSource {
    pub fn new(config: &SlackConfig) -> std::io::Result<Self> {
        Ok(SlackSource {
            api: SlackAccessor::new(config)?,
            reads_as_user: config.user_token.as_deref().is_some_and(|t| !t.is_empty()),
        })
    }

    /// List one section, narrowing the request when a scope is missing.
    async fn section(&self, types: &[&str]) -> SourceResult<(Vec<Value>, bool)> {
        match self.api.list_conversations(&types.join(",")).await {
            Ok(v) => Ok(v),
            // Ask for less rather than answer with nothing: a token with `channels:read` and
            // no `groups:read` still has public channels to show.
            Err(e) if e.is_scope_missing() && types.len() > 1 => {
                self.api.list_conversations(types[0]).await
            }
            Err(e) => Err(e),
        }
    }
}

impl MessengerSource for SlackSource {
    fn conversations<'a>(&'a self) -> BoxFuture<'a, SourceResult<(Vec<Conversation>, bool)>> {
        Box::pin(async move {
        let (chans, t1) = self.section(CHANNEL_TYPES).await?;
        let mut out: Vec<Conversation> = chans.iter().filter_map(|c| conversation(c, &[])).collect();

        let mut truncated = t1;
        if self.reads_as_user {
            let (dms, t2) = self.section(DM_TYPES).await?;
            truncated |= t2;
            // A one-to-one DM has a partner where a channel has a name, so naming those needs
            // the member list. Fetched only when there is a DM to name — a workspace whose
            // token sees none must not pay for a roster nothing will use.
            let members = if dms.iter().any(|d| d.get("user").is_some()) {
                self.api.list_users().await.map(|(m, _)| m).unwrap_or_default()
            } else {
                Vec::new()
            };
            out.extend(dms.iter().filter_map(|c| conversation(c, &members)));
        }
            Ok((out, truncated))
        })
    }

    fn history<'a>(
        &'a self,
        conv: &'a ConvId,
        window: Window,
    ) -> BoxFuture<'a, SourceResult<(Vec<Message>, bool)>> {
        Box::pin(async move {
        let (msgs, truncated) = self
            .api
            .conversation_history(&conv.0, &to_ts(window.start), &to_ts(window.end))
            .await?;
        // Slack's window is inclusive at both ends and the tree's is half-open, so a message
        // landing exactly on midnight would be served under two days. Trimmed here rather
        // than by asking for one microsecond less, which encodes the same rule as a value
        // nobody reading the request would recognize.
        let msgs = msgs
            .into_iter()
            .filter_map(|m| message(&m))
            .filter(|m| m.ts >= window.start && m.ts < window.end)
            .collect();
            Ok((msgs, truncated))
        })
    }

    fn scan<'a>(
        &'a self,
        conv: &'a ConvId,
        max_pages: usize,
    ) -> BoxFuture<'a, SourceResult<(Vec<Message>, bool)>> {
        Box::pin(async move {
            let (msgs, truncated) = self.api.scan_history(&conv.0, max_pages).await?;
            Ok((msgs.iter().filter_map(message).collect(), truncated))
        })
    }

    fn thread<'a>(
        &'a self,
        conv: &'a ConvId,
        root: &'a MsgId,
    ) -> BoxFuture<'a, SourceResult<(Vec<Message>, bool)>> {
        Box::pin(async move {
            let (msgs, truncated) = self.api.conversation_replies(&conv.0, &root.0).await?;
            Ok((msgs.iter().filter_map(message).collect(), truncated))
        })
    }

    fn users<'a>(&'a self) -> BoxFuture<'a, SourceResult<(Vec<User>, bool)>> {
        Box::pin(async move {
            let (members, truncated) = self.api.list_users().await?;
            Ok((members.iter().filter_map(user).collect(), truncated))
        })
    }

    fn fetch_file<'a>(
        &'a self,
        file: &'a FileRef,
        range: Option<Range<u64>>,
    ) -> BoxFuture<'a, SourceResult<(Vec<u8>, bool)>> {
        Box::pin(async move { self.api.download_file(&file.url, range, file.size).await })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            channels: true,
            // A bot token has no DMs to enumerate, so the section is absent rather than
            // empty — an empty one would say this person has no DMs.
            dms: self.reads_as_user,
        }
    }
}

// ---- Slack's vocabulary ---------------------------------------------------------------

/// One conversation, or `None` for a row with no id — which is not a conversation whatever
/// else it is.
fn conversation(c: &Value, members: &[Value]) -> Option<Conversation> {
    let id = c.get("id").and_then(Value::as_str)?;
    let is_im = c.get("is_im").and_then(Value::as_bool).unwrap_or(false);
    let is_mpim = c.get("is_mpim").and_then(Value::as_bool).unwrap_or(false);

    // A one-to-one DM carries its partner's id where a channel carries a name. Falling back
    // to that id keeps the entry addressable when the roster does not have them — a partner
    // outside the workspace's own member list.
    let name = match c.get("name").and_then(Value::as_str) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => {
            let partner = c.get("user").and_then(Value::as_str).unwrap_or_default();
            members
                .iter()
                .find(|m| m.get("id").and_then(Value::as_str) == Some(partner))
                .and_then(display_name)
                .unwrap_or_else(|| partner.to_string())
        }
    };

    Some(Conversation {
        id: ConvId(id.to_string()),
        name,
        kind: if is_im || is_mpim {
            ConvKind::Dm
        } else {
            ConvKind::Channel
        },
    })
}

/// One message. `None` for a row without a `ts`, which is the id, the timestamp and the
/// thread key all at once — there is nothing to serve without it.
fn message(m: &Value) -> Option<Message> {
    let ts = m.get("ts").and_then(Value::as_str)?;
    let at = parse_ts(ts)?;

    // A thread is keyed by `thread_ts`: equal to `ts` on the root, pointing back on a reply.
    let thread = m
        .get("thread_ts")
        .and_then(Value::as_str)
        .map(|root| Thread {
            root: (root != ts).then(|| MsgId(root.to_string())),
            replies: m
                .get("reply_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
        });

    Some(Message {
        id: MsgId(ts.to_string()),
        ts: at,
        from: Author {
            // A webhook message has neither a `user` nor a name, only a `bot_id` — which is
            // still an id the platform vouches for, unlike the `username` beside it.
            id: m
                .get("user")
                .or_else(|| m.get("bot_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: None,
            // Whatever the message asserted for itself. Never merged with the resolved name;
            // see `Author`.
            claimed: m
                .get("username")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        },
        text: m
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        thread,
        files: m
            .get("files")
            .and_then(Value::as_array)
            .map(|fs| fs.iter().filter_map(file).collect())
            .unwrap_or_default(),
        raw: m.clone(),
    })
}

/// One attachment. `None` unless it has everything a download needs: somewhere to fetch from
/// and the length that fetch will be checked against.
fn file(f: &Value) -> Option<FileRef> {
    Some(FileRef {
        id: f.get("id").and_then(Value::as_str)?.to_string(),
        name: f
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("untitled")
            .to_string(),
        size: f.get("size").and_then(Value::as_u64)?,
        url: f
            .get("url_private_download")
            .or_else(|| f.get("url_private"))
            .and_then(Value::as_str)?
            .to_string(),
    })
}

fn user(m: &Value) -> Option<User> {
    let id = m.get("id").and_then(Value::as_str)?;
    Some(User {
        id: id.to_string(),
        name: display_name(m).unwrap_or_else(|| id.to_string()),
        record: m.clone(),
    })
}

/// The name to show a person by, in the order Slack itself prefers.
fn display_name(m: &Value) -> Option<String> {
    for key in ["display_name", "real_name", "name"] {
        // `profile` holds the richer forms; the bare `name` is the handle.
        let v = m
            .get("profile")
            .and_then(|p| p.get(key))
            .or_else(|| m.get(key))
            .and_then(Value::as_str);
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            return Some(v.to_string());
        }
    }
    None
}

/// `1785737875.341929` as an instant.
///
/// Parsed in two integer halves rather than as one float: a Slack `ts` is also the message's
/// id, and `f64` has no exact representation for most of them — a round trip through one
/// would produce a `ts` that names a different message, or none.
fn parse_ts(ts: &str) -> Option<SystemTime> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    let secs: u64 = secs.parse().ok()?;
    // Right-pad, so `.34` is 340,000µs and not 34.
    let mut micros = frac.chars().filter(char::is_ascii_digit).collect::<String>();
    micros.truncate(6);
    while micros.len() < 6 {
        micros.push('0');
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_micros(micros.parse().ok()?))
}

/// An instant as the string Slack's `oldest`/`latest` take.
fn to_ts(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:06}", d.as_secs(), d.subsec_micros())
}


#[cfg(test)]
#[path = "source_tests.rs"]
mod source_tests;
