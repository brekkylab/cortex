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
    Author, Capabilities, ConvId, ConvKind, Conversation, FileRef, MessengerIndex,
    MessengerSource, Message, MsgId, SearchHit, Thread, User, Window,
};

/// Both kinds of a section, asked for in one call.
///
/// Slack wants a scope per conversation *type* and fails the whole call when one is missing, so
/// `types=public_channel,private_channel` on a token without `groups:read` answers
/// `missing_scope` and no channels at all. Asking again for fewer types would turn that into a
/// tree of public channels only — which is worse, because nothing in the result says a kind is
/// missing: an agent reads a workspace with no private channels and concludes there are none.
/// So the failure propagates, and `ls /channels` says `missing_scope` with the grant to fix it.
const CHANNEL_TYPES: &[&str] = &["public_channel", "private_channel"];
const DM_TYPES: &[&str] = &["im", "mpim"];

pub struct SlackSource {
    api: SlackAccessor,
    /// Whether the install granted a user token. A bot token cannot see DMs at all, so this
    /// decides whether `dms/` exists rather than whether it is empty.
    reads_as_user: bool,
}

impl SlackSource {
    /// Build a source over an already-obtained token.
    ///
    /// No scope check here, deliberately. Slack answers `missing_scope` on the first call a
    /// missing grant governs, with the `needed`/`provided` pair the accessor turns into the
    /// message — so a misconfigured install already fails loudly, early, and naming what to
    /// grant, at `ls` instead of at mount. Checking up front would mean keeping a second copy of
    /// which scope each of Slack's methods wants, and a copy that drifts refuses a mount Slack
    /// would have served — a failure the API itself cannot produce.
    ///
    /// The scope table in [this module's docs](super) is for whoever grants the token, not for
    /// code to enforce.
    pub fn new(config: &SlackConfig) -> std::io::Result<Self> {
        Ok(SlackSource {
            api: SlackAccessor::new(config)?,
            reads_as_user: config.user_token.as_deref().is_some_and(|t| !t.is_empty()),
        })
    }


    /// Slack's own message index, as hits that point back into the tree.
    ///
    /// Inherent rather than a [`MessengerSource`] method, for the reason [`SearchHit`] gives.
    /// Needs the user token: Slack refuses `search.messages` to a bot, and
    /// [`SlackAccessor::search_available`] is what a caller asks before offering the feature.
    ///
    /// # Why the response is rewritten before it is converted
    ///
    /// A search match is a message object with two differences from the same message in
    /// `conversations.history`, and both would be silent:
    ///
    /// * **No `thread_ts`.** The match does not carry it, so a reply would look like a
    ///   conversation message and its path would name the wrong file. The permalink does carry
    ///   it, so for a *reply* it is spliced back in and the one Slack→[`Message`] converter
    ///   serves both paths. For a **root** it is not: a root's `thread_ts` points at itself, and
    ///   with no `reply_count` in the match the converter would render `"replies":0` onto a
    ///   root that has replies. An absent `thread` says "not stated", which is true; a zero
    ///   would say something false, and the file two paths away says it correctly.
    /// * **A different `username`.** In history that field is what a *webhook asserted about
    ///   itself*, which is why it lands in [`Author::claimed`]. In a search match it is the
    ///   account's own handle, which is not a claim at all — keeping it would file a fact as an
    ///   assertion, and the two are separate fields precisely so that cannot happen. So it is
    ///   dropped, and the author's name is resolved from the id like everywhere else.
    pub async fn search(&self, query: &str, count: usize) -> SourceResult<Vec<SearchHit>> {
        let answer = self.api.search_messages(query, count).await?;
        let matches = answer
            .get("messages")
            .and_then(|m| m.get("matches"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();

        let mut out = Vec::with_capacity(matches.len());
        for m in matches {
            // The match carries the channel's whole record — id, name and the `is_*` flags
            // that decide the section — so the same converter the listing uses builds it, with
            // no roster to resolve a DM's partner against.
            let Some(conv) = m.get("channel").and_then(|c| conversation(c, &[])) else {
                continue;
            };
            let mut m = m.clone();
            if let Some(obj) = m.as_object_mut() {
                obj.remove("username");
                let own = obj.get("ts").and_then(Value::as_str).unwrap_or_default().to_string();
                match permalink_thread_ts(obj.get("permalink")) {
                    // A reply: the root it belongs to is what decides which file it is in.
                    Some(root) if root != own => {
                        obj.insert("thread_ts".into(), Value::String(root));
                    }
                    // A root points at itself, and the index carries no `reply_count`. Splicing
                    // it in would render `"replies":0` onto a root that has four — a number the
                    // file has and this answer does not. So the thread is left off the line
                    // entirely, which is what an absent key already means here: not stated.
                    _ => {}
                }
            }
            let Some(msg) = message(&m) else { continue };
            // Only a reply needs it: a root's own instant already names its day.
            let thread_started = msg
                .thread
                .as_ref()
                .and_then(|t| t.root.as_ref())
                .and_then(|r| parse_ts(&r.0));
            out.push(SearchHit {
                conv,
                msg,
                thread_started,
            });
        }
        Ok(out)
    }

    /// List one section.
    ///
    /// No narrowing on a missing scope, unlike an earlier shape of this: [`new`](Self::new) has
    /// already refused a token that cannot read a kind, so a `missing_scope` here means one was
    /// revoked mid-session — and answering that with a quietly smaller tree is the silent
    /// failure the mount check exists to remove.
    async fn section(&self, types: &[&str]) -> SourceResult<Vec<Value>> {
        self.api.list_conversations(&types.join(",")).await
    }
}

impl MessengerSource for SlackSource {
    fn conversations<'a>(&'a self) -> BoxFuture<'a, SourceResult<Vec<Conversation>>> {
        Box::pin(async move {
        let chans = self.section(CHANNEL_TYPES).await?;
        let mut out: Vec<Conversation> = chans.iter().filter_map(|c| conversation(c, &[])).collect();

        if self.reads_as_user {
            let dms = self.section(DM_TYPES).await?;
            // A one-to-one DM has a partner where a channel has a name, so naming those needs
            // the member list. Fetched only when there is a DM to name — a workspace whose
            // token sees none must not pay for a roster nothing will use.
            // Propagated, not defaulted away: a roster this cannot fetch is a missing
            // `users:read`, and swallowing it serves every DM under a fallback name with no
            // error anywhere — the one place in this source where a missing grant would be
            // quiet.
            let members = if dms.iter().any(|d| d.get("user").is_some()) {
                self.api.list_users().await?
            } else {
                Vec::new()
            };
            out.extend(dms.iter().filter_map(|c| conversation(c, &members)));
        }
            Ok(out)
        })
    }

    fn history<'a>(
        &'a self,
        conv: &'a ConvId,
        window: Window,
    ) -> BoxFuture<'a, SourceResult<Vec<Message>>> {
        Box::pin(async move {
        let msgs = self
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
            Ok(msgs)
        })
    }

    fn thread<'a>(
        &'a self,
        conv: &'a ConvId,
        root: &'a MsgId,
    ) -> BoxFuture<'a, SourceResult<Vec<Message>>> {
        Box::pin(async move {
            let msgs = self.api.conversation_replies(&conv.0, &root.0).await?;
            Ok(msgs.iter().filter_map(message).collect())
        })
    }

    fn users<'a>(&'a self) -> BoxFuture<'a, SourceResult<Vec<User>>> {
        Box::pin(async move {
            let members = self.api.list_users().await?;
            Ok(members.iter().filter_map(user).collect())
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

    /// Slack keeps a message index, and `search.messages` is **user token only** — a bot token
    /// is refused it whatever scopes the install was granted.
    ///
    /// So this is `None` for a bot-token mount, and the difference is worth having: a store
    /// that says it has no index is reported as such, where one that offered an index and then
    /// failed every call would spend a request to say the same thing and read as a fault.
    fn as_index(&self) -> Option<&dyn MessengerIndex> {
        self.reads_as_user.then_some(self as &dyn MessengerIndex)
    }
}

impl MessengerIndex for SlackSource {
    fn search<'a>(
        &'a self,
        query: &'a str,
        count: usize,
    ) -> BoxFuture<'a, SourceResult<Vec<SearchHit>>> {
        Box::pin(async move { SlackSource::search(self, query, count).await })
    }
}

// ---- Slack's vocabulary ---------------------------------------------------------------

/// The `thread_ts` a permalink carries, which is where a search match keeps it.
///
/// `.../p1785737892782639?thread_ts=1785737875.341929` — present on a reply and on a root
/// (pointing at itself), absent on a message that is not in a thread at all.
fn permalink_thread_ts(link: Option<&Value>) -> Option<String> {
    let query = link?.as_str()?.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("thread_ts="))
        .filter(|ts| !ts.is_empty())
        .map(str::to_string)
}

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
        // Seconds, unlike the `updated` beside it in the same object, which is milliseconds.
        // Absent for a conversation Slack did not report one for, and then the epoch — which
        // makes the calendar start further back than it needs to rather than cut a day off it.
        created: UNIX_EPOCH
            + Duration::from_secs(c.get("created").and_then(Value::as_u64).unwrap_or(0)),
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
        size: Some(f.get("size").and_then(Value::as_u64)?),
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
#[path = "slack_tests.rs"]
mod slack_tests;
