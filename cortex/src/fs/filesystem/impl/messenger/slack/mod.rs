//! Slack, as a [`MessengerSource`](super::MessengerSource).
//!
//! Two files, because the halves fail differently and are tested differently:
//!
//! * [`accessor`] — the Web API client. One gate on every call, because Slack reports
//!   application errors *inside* a 200; cursor pagination; the one host check an attachment
//!   download is allowed to send the token to.
//! * [`source`] — which of those calls answers which of the lane's questions, and how a Slack
//!   message becomes a [`Message`](super::Message).
//!
//! # Where the credentials come from
//!
//! [`SlackConfig`] carries tokens that were already obtained. Getting them — the
//! `oauth.v2.access` code exchange — needs a client secret and belongs to whatever ran the
//! consent flow, which is a server with an HTTP handler and not a filesystem. `S3Config` and
//! `NotionConfig` are the same shape for the same reason, and like them nothing in this crate
//! constructs one: a consumer builds a [`SlackSource`] and mounts the
//! [`MessengerFs`](super::MessengerFs) around it.
//!
//! # What the install has to grant
//!
//! Every scope below is the one Slack's own method reference names for a call this client
//! makes. A user token is what a mount wants (see [`SlackConfig::user_token`]); the scope names
//! are the same either way, and it is the *reach* that differs.
//!
//! | what this calls | scope |
//! | --- | --- |
//! | `conversations.list` | `channels:read` `groups:read` `im:read` `mpim:read` |
//! | `conversations.history`, `conversations.replies` | `channels:history` `groups:history` `im:history` `mpim:history` |
//! | `users.list`, `users.info`, `bots.info` | `users:read` |
//! | attachment download | `files:read` |
//! | `search.messages` | `search:read` — **user token only** |
//!
//! Two of those rows are softer than they look, and one is harder.
//!
//! **`groups:*` is optional.** Slack wants a scope per conversation *type* and fails the whole
//! `conversations.list` call when one is missing, so a token with `channels:read` and no
//! `groups:read` would see no channels at all — which is why
//! [`SlackSource::section`](source) asks again for fewer types. Without the `groups:*` pair the
//! tree simply has no private channels in it.
//!
//! **`search:read` is unused by the tree.** There is no search in
//! [`MessengerSource`](super::MessengerSource) — the point of the layout is that `grep` runs
//! locally — so this is only reachable through [`SlackAccessor::search_messages`] by a caller
//! holding the client directly.
//!
//! **The DM scopes are not optional on a user token.** `dms/` exists whenever there is one (see
//! [`SlackSource::capabilities`](source)), so the tree *will* ask, and a token missing
//! `im:read` fails that listing rather than serving a tree without the section. Narrowing
//! cannot save it the way it saves `groups:read`, because dropping `mpim` still leaves `im`.
//! Granting the four DM scopes with a user token, or using a bot token — which has no DMs and
//! is reported as such — are the two arrangements that work.
//!
//! Only `files:read` is inferred rather than quoted: Slack documents it for `files.info`, and an
//! attachment here is fetched from the `url_private_download` in a message rather than through
//! that method, which the reference does not separately state a scope for.

mod accessor;
mod source;

pub use accessor::*;
pub use source::*;
