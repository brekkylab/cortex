//! [`MessengerSource`] — what a messenger has to answer for its conversations to become a
//! tree, and the normalized shapes it answers in.
//!
//! # What is normalized, and what deliberately is not
//!
//! Normalized: a timestamp is a [`SystemTime`] whichever way the platform spells one (a
//! Slack float-seconds string, a Discord snowflake, an RFC 3339 instant), an author is an
//! [`Author`], a thread is [`Thread`]. That is what makes one `jq` filter work across every
//! source — the point of the lane, and the reason the tree-building code
//! ([`MessengerFs`](super::MessengerFs)) is written once rather than per platform.
//!
//! Not normalized: [`Message::raw`] keeps the platform's own object. A normalization that
//! cannot be escaped is one that decides in advance what nobody will need, and the
//! alternative has a name — every unified messenger project that flattened its sources to a
//! common struct lost the platform-specific half with it.
//!
//! # Why there is no method for "which days are there"
//!
//! No messenger API has an endpoint for it, and the two ways to answer it without one both
//! cost something. Walking the newest messages backwards spends a request per conversation and
//! *cannot finish*: two hundred messages is a month of a quiet channel and four hours of a busy
//! one, so the listing it produces is a window that reads like a whole — and the reader is an
//! agent, which has no way to tell the difference and stops looking.
//!
//! So the tree generates the axis instead, from [`Conversation::created`] to today. The
//! `<year>/` and `<month>/` *names* are arithmetic over those two dates: always complete, never
//! a window, and spelling them costs nothing past the walk that listed the conversations. An
//! agent sees the whole span before asking for any of it, and a channel carrying fifty thousand
//! messages a day has no more of them than a silent one of the same age.
//!
//! A month is where that stops being free, and it is the unit a window buys. Inside one, only a
//! day that *has* messages is a name — a day file, not a directory level — because naming every
//! silent day is what the walk was rejected for, and an agent cannot tell an empty directory
//! from a failed request without paying a round trip to find out.

use std::ops::Range;
use std::time::SystemTime;

use serde_json::Value;

use crate::BoxFuture;

use super::error::SourceResult;

/// A conversation's platform id, opaque to the tree.
///
/// A newtype and not a `String`, because a conversation id and a message id are both strings
/// and the tree passes them next to each other — see [`MsgId`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConvId(pub String);

/// A message's platform id. Also the name of its thread directory, when it has one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MsgId(pub String);

/// Which section of the tree a conversation belongs under.
///
/// Only two, because only two survive on every platform in the lane. A finer split (private
/// vs public channels, group vs one-to-one DMs) is a property of one platform's model, and
/// the tree would then have sections some sources always leave empty — which is the empty
/// directory this lane refuses to synthesize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvKind {
    /// A named, many-participant conversation. `channels/`.
    Channel,
    /// A direct or small-group conversation, named by its participants. `dms/`.
    Dm,
}

/// One conversation the tree will give a directory to.
#[derive(Clone, Debug)]
pub struct Conversation {
    pub id: ConvId,
    /// Display name, used as the human half of the `<name>__<id>` directory. Empty is
    /// allowed — the id half still identifies it.
    pub name: String,
    pub kind: ConvKind,
    /// When the conversation was created.
    ///
    /// The tree's date axis is a calendar from here to today, so this is not decoration: it is
    /// the *bottom* of what a reader may address, and it is why no request is spent finding out
    /// which days a conversation has. Every platform reports it in the listing this came from —
    /// Slack's `created`, a Discord channel's snowflake, a Teams chat's `createdDateTime` — so
    /// it costs nothing to carry.
    pub created: SystemTime,
}

/// Who sent a message.
///
/// The two names are separate fields on purpose. `name` is what the platform's own directory
/// says the account is called; `claimed` is a name the *message* asserted for itself — a
/// webhook's `username`, a bot posting under a display name. Merging them lets anything that
/// can post choose to look like a colleague, and the tree is read by an agent that cannot
/// tell the difference unless the file does.
#[derive(Clone, Debug, Default)]
pub struct Author {
    /// The platform's id for the account. The only field the platform vouches for.
    pub id: String,
    /// The name that id resolves to in the source's own user directory, when it does.
    pub name: Option<String>,
    /// A name this one message asserted for itself, if any. Never merged into `name`.
    pub claimed: Option<String>,
}

/// A message's place in a thread, when it is in one.
#[derive(Clone, Debug)]
pub struct Thread {
    /// The thread's root, or `None` when this message *is* the root.
    ///
    /// A reply keyed by its root's id and not by the root message itself, because a root can
    /// be deleted while its replies live on: hanging a thread off the root object loses the
    /// whole thread when that happens.
    pub root: Option<MsgId>,
    /// Replies below the root. Zero on a reply itself — only a root counts them.
    pub replies: usize,
}

/// An attachment, as a listing knows it.
#[derive(Clone, Debug)]
pub struct FileRef {
    pub id: String,
    /// Name as posted. The tree makes a filename out of it and its id.
    pub name: String,
    /// Exact length, when the listing said one.
    ///
    /// `None` is a platform that does not: Slack's file object carries `size`, a Teams
    /// `chatMessageAttachment` carries `content`, `contentType`, `contentUrl`, `id`, `name`,
    /// `teamsAppId` and `thumbnailUrl` — and no length anywhere.
    ///
    /// It forfeits two things, and both for the same reason rather than by choice:
    ///
    /// * **Windowing.** A window ends at `min(offset + window, size)`, so without a size there
    ///   is no end to stop at. An unsized attachment is fetched whole however large it is,
    ///   which is why the ceiling a mount sets does not bind it.
    /// * **The length check.** A body that is not as long as the listing promised is a login
    ///   page or a truncation rather than the file — the check that catches a refused token
    ///   answering `200`. With nothing promised there is nothing to compare.
    ///
    /// A source that can cheaply learn the length should, rather than leave this `None`: one
    /// `Range: bytes=0-0` returns the total in `Content-Range`, and a listing that spends that
    /// per attachment buys back both of the above.
    pub size: Option<u64>,
    /// Where the bytes are, in whatever form the source will hand back to itself.
    pub url: String,
}

/// One message, normalized.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: MsgId,
    /// When it was created — not when it was last edited. The tree buckets by this, so a
    /// reaction arriving a year later must not move a message to another day.
    pub ts: SystemTime,
    pub from: Author,
    /// The body as text. May be empty (an attachment-only post, a system event).
    pub text: String,
    pub thread: Option<Thread>,
    pub files: Vec<FileRef>,
    /// The platform's own object for this message, kept whole. See the module docs.
    pub raw: Value,
}

/// One hit from a platform's own message index, and where in the tree it can be read.
///
/// Search is not part of [`MessengerSource`]: Slack refuses it to a bot token, Discord does
/// not offer it to a bot at all, and a trait method some sources could only ever fail is the
/// empty directory of a trait. So a source that *can* search says so with an inherent method
/// handing these back, and a caller that has one uses it — while the tree, which needs every
/// source to answer the same questions, never asks.
///
/// The message is the same [`Message`] a day's file is rendered from, so a hit and the line it
/// points at cannot disagree about what the message is.
#[derive(Clone, Debug)]
pub struct SearchHit {
    /// The conversation it was posted in, as the search answer describes it.
    ///
    /// A whole record and not an id, so a caller can name the directory without a second
    /// request. It is not necessarily what a *listing* would say, though: a platform may hand
    /// back an index's own idea of a conversation — a one-to-one DM with no display name, for
    /// one — so a caller that has the listing should prefer it and keep this as the fallback.
    /// Either way the id half is the address, so a path built from this still resolves.
    pub conv: Conversation,
    pub msg: Message,
    /// When the thread this message is in was started, for a reply.
    ///
    /// A thread is served under its **root's** day, and a root is named by a platform id this
    /// crate treats as opaque — so a source that can recover the root's instant says it here
    /// rather than leaving a caller to parse an id it is not allowed to understand. `None` for
    /// a message that is not a reply, whose own instant names its day.
    pub thread_started: Option<SystemTime>,
}

/// A half-open span of time — one day of the tree, as the source should ask for it.
///
/// `SystemTime` and not a date, so the trait needs no calendar: turning `2026-08-10/` into
/// this pair is the tree's arithmetic, and turning this pair into `oldest`/`latest` params is
/// the source's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: SystemTime,
    /// Exclusive, so consecutive days neither overlap nor leave a gap.
    pub end: SystemTime,
}

/// One member of the workspace, for resolving ids to names.
#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub name: String,
    /// The platform's own record, for the `users/<name>__<id>.json` the tree serves.
    pub record: Value,
}

/// What a source can and cannot do, where the difference changes the tree's *shape*.
///
/// Deliberately tiny: a field earns its place only when a directory exists or does not
/// because of it. Anything else is an error the caller reads.
///
/// Both ask about **enumeration**, which an empty answer cannot settle — "none today" and
/// "never, by this credential" are the same empty list and opposite claims, and only one of
/// them may be served as an empty directory. The source knows which it is; nothing else can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether [`MessengerSource::conversations`] can return [`ConvKind::Channel`] entries.
    /// False for a source whose platform keeps its channels in another shape.
    pub channels: bool,
    /// Whether it can return [`ConvKind::Dm`] entries *and read them*. False where either
    /// half is missing: a listing whose conversations all serve empty is worse than an absent
    /// `dms/`, because it looks like it works.
    pub dms: bool,
}

/// A messenger the tree can be built over.
///
/// Read-only by construction: there is no method here that writes. A conversation this
/// credential cannot read is the source's own business to absorb — it returns what it could
/// see, because one channel a bot was never invited to must not fail the whole tree — while a
/// failure that applies to *every* conversation (a dead token, a scope never granted)
/// propagates, because serving that as an empty workspace presents it as a complete one.
/// # Why the futures are boxed
///
/// The same reason [`FileSystem`](crate::fs::FileSystem)'s are: a native `async fn` in a trait
/// is not object-safe, and one boxed future per call sits next to a round trip to a messenger.
/// Writing it out rather than reaching for a macro keeps this crate's one convention.
pub trait MessengerSource: Send + Sync {
    /// Conversations this credential can see.
    ///
    /// Every one of them, or a failure — never part of them, and there is no exception. A
    /// source that pages walks to the end of its cursor rather than stopping at a ceiling, and
    /// one the service breaks off partway fails, dropping the pages it had. Expensive, and the
    /// alternative is worse: a listing short by two thousand channels is indistinguishable from
    /// a workspace that has two thousand fewer, nothing in a directory can say which it is, and
    /// the tree makes those channels unreachable by path as well.
    ///
    /// The same holds for [`history`](Self::history), [`thread`](Self::thread) and
    /// [`users`](Self::users): each answers with all of what was asked for, or fails. A short
    /// month is a day this tree does not name, and an unnamed day reads as a day that held
    /// nothing.
    fn conversations<'a>(&'a self) -> BoxFuture<'a, SourceResult<Vec<Conversation>>>;

    /// Messages created within `window`, oldest-first, thread roots and standalone messages
    /// only — a root's replies come from [`thread`](Self::thread).
    fn history<'a>(
        &'a self,
        conv: &'a ConvId,
        window: Window,
    ) -> BoxFuture<'a, SourceResult<Vec<Message>>>;

    /// A thread: its root followed by every reply, oldest-first.
    fn thread<'a>(
        &'a self,
        conv: &'a ConvId,
        root: &'a MsgId,
    ) -> BoxFuture<'a, SourceResult<Vec<Message>>>;

    /// Workspace members, for resolving author ids to names.
    fn users<'a>(&'a self) -> BoxFuture<'a, SourceResult<Vec<User>>>;

    /// An attachment's bytes. `range` is a request, not a promise: the second return value
    /// is false when the source served the whole thing and the caller must slice it itself.
    fn fetch_file<'a>(
        &'a self,
        file: &'a FileRef,
        range: Option<Range<u64>>,
    ) -> BoxFuture<'a, SourceResult<(Vec<u8>, bool)>>;

    /// See [`Capabilities`].
    fn capabilities(&self) -> Capabilities;

    /// This source's own message index, if its service keeps one this credential may ask.
    ///
    /// `None` by default, which is the honest answer for most of the lane on most credentials.
    /// Slack keeps a message index and refuses it to a bot token; Discord gave bots none at all
    /// until March 2026. A method every source had to write would be one most of them could
    /// only ever fail, which is the mistake [`FileSystem::searchable`] is shaped to avoid one level
    /// up — and this is how a source tells the tree above it which of the two it is.
    ///
    /// Separate from the rest of the trait because the rest is the set of questions *every*
    /// source answers, and this is the one only some can. What reads it is
    /// [`MessengerFs::index`](super::MessengerFs), which is what turns "this source can be
    /// asked" into "this mount can be searched".
    ///
    /// [`FileSystem::searchable`]: crate::fs::FileSystem::searchable
    fn searchable(&self) -> Option<&dyn MessengerSearch> {
        None
    }
}

/// A source whose service keeps a message index this credential may ask.
///
/// One method, and it answers in the lane's own [`SearchHit`] rather than in the tree's
/// vocabulary: a source knows how to ask its service and how to normalize what came back, and
/// nothing about which file a message's line ends up in. Turning a hit into a path is the
/// tree's, because the tree is what spells paths — see [`MessengerFs`](super::MessengerFs).
///
/// Reached only through [`MessengerSource::searchable`]. A source that implements this and
/// forgets to override that is a source nothing will ever ask, which is why the override is one
/// line and sits next to the impl.
pub trait MessengerSearch: Send + Sync {
    /// Matches for `query`, most relevant or most recent first, at most `count` of them.
    ///
    /// The query is the service's own syntax, untouched. See
    /// [`Searchable::search`](crate::fs::Searchable::search) for why nothing translates it.
    fn search<'a>(
        &'a self,
        query: &'a str,
        count: usize,
    ) -> BoxFuture<'a, SourceResult<Vec<SearchHit>>>;
}

/// The one line format, shared by every source.
///
/// This function is the lane's actual product. A layout is a convention two implementations
/// can agree to by accident and drift from by accident; a single renderer cannot drift,
/// which is what makes "the same `jq` filter works on all of them" a property of the code
/// rather than a claim in a document.
///
/// One message is always one line: `serde_json` escapes a newline in the body as `\n`, so a
/// `grep` hit is a whole message and the name attached to it is the right one. Nothing here
/// may switch to a pretty writer without breaking that.
pub fn render_line(msg: &Message) -> Vec<u8> {
    let mut from = serde_json::Map::new();
    from.insert("id".into(), Value::String(msg.from.id.clone()));
    if let Some(n) = &msg.from.name {
        from.insert("name".into(), Value::String(n.clone()));
    }
    // Kept apart from `name` for the reason `Author::claimed` exists.
    if let Some(c) = &msg.from.claimed {
        from.insert("claimed".into(), Value::String(c.clone()));
    }

    let mut out = serde_json::Map::new();
    out.insert("ts".into(), Value::String(rfc3339(msg.ts)));
    out.insert("id".into(), Value::String(msg.id.0.clone()));
    out.insert("from".into(), Value::Object(from));
    out.insert("text".into(), Value::String(msg.text.clone()));
    if let Some(t) = &msg.thread {
        out.insert(
            "thread".into(),
            serde_json::json!({
                "root": t.root.as_ref().map(|r| r.0.clone()),
                "replies": t.replies,
            }),
        );
    }
    if !msg.files.is_empty() {
        out.insert(
            "files".into(),
            Value::Array(
                msg.files
                    .iter()
                    .map(|f| serde_json::json!({"name": f.name, "size": f.size, "id": f.id}))
                    .collect(),
            ),
        );
    }

    let mut bytes = serde_json::to_vec(&Value::Object(out)).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    bytes
}

/// A `SystemTime` as the fixed-width UTC instant the line format uses.
///
/// Second resolution, deliberately: the sub-second part of a messenger timestamp is a
/// platform's own ordering key (Slack's `ts` carries microseconds and *is* the message's id),
/// and a line that printed it would invite sorting on a field that means something different
/// on each source. Ordering within a day comes from the file's order, which the source
/// guarantees.
fn rfc3339(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod source_tests;
