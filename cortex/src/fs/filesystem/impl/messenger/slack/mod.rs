//! Slack, as a [`MessengerSource`](super::MessengerSource).
//!
//! Two files, because the halves fail differently and are tested differently:
//!
//! * [`accessor`] — the Web API client. One gate on every call, because Slack reports
//!   application errors *inside* a 200; cursor pagination; the one host check an attachment
//!   download is allowed to send the token to.
//! * [`slack`] — which of those calls answers which of the lane's questions, and how a Slack
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
//! Nothing in this crate checks that list before mounting, and the table is here for whoever
//! grants the token rather than for code to enforce — see [`SlackSource::new`](slack). What
//! enforces it is Slack: `missing_scope` comes back on the first call the absent grant governs,
//! carrying `needed` and `provided`, which is what the accessor puts in the error a reader sees.
//!
//! Three of the rows are worth a note.
//!
//! **`groups:*` is not optional.** Slack wants a scope per conversation *type* and fails the
//! whole `conversations.list` call when one is missing, so a token with `channels:read` and no
//! `groups:read` sees no channels **at all** — not "public ones only". `channels/` asks for both
//! types in one call and does not retry with fewer, so a public-channels-only install does not
//! get a narrower tree; it gets `EACCES` on that listing.
//!
//! **`users:read` reaches further than the roster.** `dms/` names a one-to-one conversation
//! after its partner, which needs the member list, so a token without it fails that listing too
//! — rather than serving DMs under a fallback name, which would be the one grant whose absence
//! this source could be quiet about.
//!
//! **`search:read` is unused by the tree.** There is no search in
//! [`MessengerSource`](super::MessengerSource) — the point of the layout is that `grep` runs
//! locally — so this is only reachable through [`SlackAccessor::search_messages`] by a caller
//! holding the client directly. An install that never granted it still mounts and reads.
//!
//! Only `files:read` is inferred rather than quoted: Slack documents it for `files.info`, and an
//! attachment here is fetched from the `url_private_download` in a message rather than through
//! that method, which the reference does not separately state a scope for.

mod accessor;
mod slack;

pub use accessor::*;
pub use slack::*;
