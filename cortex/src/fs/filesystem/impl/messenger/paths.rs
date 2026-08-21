//! The tree's names, in one place.
//!
//! Which section a conversation is listed under, what its directory is called, which day
//! directory a message falls in, and the path of the file its line is in.
//!
//! Split out because there are now two callers. The tree builds these names when it lists a
//! directory; anything that points *back* into the tree — a search hit naming where its
//! message can be read — has to build the same string. Two implementations of one rule is two
//! layouts, which is the failure this lane exists to prevent, so the arithmetic lives here and
//! neither caller has a copy.

use std::time::SystemTime;

use chrono::{DateTime, NaiveDate, Utc};

use super::{ConvKind, Conversation, Message};

/// The three sections at the mount root.
pub(super) const CHANNELS: &str = "channels";
pub(super) const DMS: &str = "dms";
pub(super) const USERS: &str = "users";

/// The file a day's (or a thread's) messages are served as.
pub(super) const CHAT: &str = "chat.jsonl";
/// The directories inside a day.
pub(super) const THREADS: &str = "threads";
pub(super) const FILES: &str = "files";

/// How a day directory is spelled. One constant, because a listing writes these names and a
/// path parser reads them back.
pub(super) const DAY_FMT: &str = "%Y-%m-%d";

/// The section a conversation of this kind is listed under.
pub fn section_of(kind: ConvKind) -> &'static str {
    match kind {
        ConvKind::Channel => CHANNELS,
        ConvKind::Dm => DMS,
    }
}

/// The `<name>__<id>` directory a conversation is listed as.
///
/// The human half is for reading and the id half is the address: a path is resolved by the id
/// alone, so a renamed channel keeps every path anyone wrote down.
pub fn conv_dir(conv: &Conversation) -> String {
    format!("{}__{}", sanitize(&conv.name), conv.id.0)
}

/// The day directory an instant falls in.
///
/// **UTC**, and not the workspace's own zone: nothing here has a timezone to read one out of,
/// and a bucketing rule that moved with the reader's clock would file the same message under
/// different days for two readers of one mount.
pub fn day_dir(at: SystemTime) -> String {
    day_of(at).format(DAY_FMT).to_string()
}

/// The tree-relative path of the file this message's line is in.
///
/// `day` is the instant whose UTC day names the directory — a message's own for one posted to
/// the conversation, and its **thread root's** for a reply, because a thread is served under
/// the day it was started on. A caller has to pass it: a root is named by a platform id, which
/// is opaque here, so this cannot recover the root's instant from the message alone.
pub fn chat_path(conv: &Conversation, msg: &Message, day: SystemTime) -> String {
    let dir = format!(
        "{}/{}/{}",
        section_of(conv.kind),
        conv_dir(conv),
        day_dir(day)
    );
    match msg.thread.as_ref().and_then(|t| t.root.as_ref()) {
        Some(root) => format!("{dir}/{THREADS}/{}/{CHAT}", root.0),
        None => format!("{dir}/{CHAT}"),
    }
}

/// A display name made safe to be one path component.
///
/// `/` would open a directory nobody created and a leading `.` hides the entry from an `ls`
/// that did not ask; a name that is entirely separators still has to leave something behind,
/// or two conversations collapse onto one path.
pub(super) fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// The UTC day a timestamp falls in — the tree's bucketing rule, in one place.
pub(super) fn day_of(ts: SystemTime) -> NaiveDate {
    DateTime::<Utc>::from(ts).date_naive()
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;
