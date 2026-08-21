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

use chrono::{DateTime, Datelike, NaiveDate, Utc};

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

/// A day is three directories and not one name.
///
/// `2026/08/10` rather than `2026-08-10`, because the directories are a *calendar* — every day
/// between a conversation's creation and today is there whether or not it holds anything, so a
/// flat axis puts a thousand entries in one listing for a channel a few years old. Split, no
/// listing is longer than the months in a year or the days in a month.
///
/// Zero-padded and fixed width, so a lexical sort is a chronological one — which is what `ls`
/// does without being asked.
pub(super) fn year_dir(year: i32) -> String {
    format!("{year:04}")
}

/// A month or a day, as the two-digit name it is listed under.
pub(super) fn pad2(n: u32) -> String {
    format!("{n:02}")
}

/// The three components a date is spelled as, joined.
pub(super) fn day_path(date: NaiveDate) -> String {
    format!(
        "{}/{}/{}",
        year_dir(date.year()),
        pad2(date.month()),
        pad2(date.day())
    )
}

/// The date `y/m/d` names, or `None` when any component is not the name this writes.
///
/// Strict about width as well as value: `2026/8/10` is not a name any listing produced, and
/// accepting it would give one day two paths — and two cache entries that never see each
/// other.
pub(super) fn date_of(y: &str, m: &str, d: &str) -> Option<NaiveDate> {
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
}

/// The years a conversation created at `created` has, up to and including today.
pub(super) fn years(created: NaiveDate, today: NaiveDate) -> Vec<i32> {
    (created.year()..=today.year()).collect()
}

/// The months of `year` that fall within `[created, today]`.
pub(super) fn months(year: i32, created: NaiveDate, today: NaiveDate) -> Vec<u32> {
    let first = if year == created.year() { created.month() } else { 1 };
    let last = if year == today.year() { today.month() } else { 12 };
    (first..=last).collect()
}

/// The days of `year`-`month` that fall within `[created, today]`.
///
/// The month's own length comes from the calendar rather than a table: the first of the next
/// month, less one day, is right in February of every year without anybody writing the rule
/// down.
pub(super) fn days(year: i32, month: u32, created: NaiveDate, today: NaiveDate) -> Vec<u32> {
    let Some(first_of) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return Vec::new();
    };
    let in_month = match month {
        12 => NaiveDate::from_ymd_opt(year + 1, 1, 1),
        _ => NaiveDate::from_ymd_opt(year, month + 1, 1),
    }
    .map(|next| next.signed_duration_since(first_of).num_days() as u32)
    .unwrap_or(0);

    (1..=in_month)
        .filter(|d| {
            NaiveDate::from_ymd_opt(year, month, *d)
                .is_some_and(|date| date >= created && date <= today)
        })
        .collect()
}

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
    entry(&conv.name, &conv.id.0, "")
}

/// The day directory an instant falls in.
///
/// **UTC**, and not the workspace's own zone: nothing here has a timezone to read one out of,
/// and a bucketing rule that moved with the reader's clock would file the same message under
/// different days for two readers of one mount.
pub fn day_dir(at: SystemTime) -> String {
    day_path(day_of(at))
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

/// The longest one path component may be, in **bytes**.
///
/// 255 is the floor across what this mounts on — the Linux VFS limit, and APFS's — and a name
/// over it is not served at all. Slack alone never reaches it, because a channel name is capped
/// at 80 characters; an attachment's name is whoever uploaded it's, and a Discord group DM or a
/// Teams chat topic is not capped at anything this can count on.
pub(super) const NAME_MAX: usize = 255;

/// `<name>__<id><suffix>` as one path component, cut to fit.
///
/// `suffix` is a parameter rather than the caller's own `format!` because it spends the same
/// budget: `__{id}.json` leaves five fewer bytes for the name than `__{id}` does, and a caller
/// concatenating afterwards would put the component back over the limit having just fitted it.
///
/// The **id is never cut** — it is the address, and a path is resolved by it alone, so what
/// gives way is the half that is only for reading. Two long names can therefore shorten to the
/// same text; their components still differ, because the id follows.
pub(super) fn entry(name: &str, id: &str, suffix: &str) -> String {
    let tail = format!("__{id}{suffix}");
    let budget = NAME_MAX.saturating_sub(tail.len());
    let mut head = sanitize(name);
    if head.len() > budget {
        // Back off to a character boundary: `truncate` panics inside one, and a name cut
        // mid-character is bytes no reader can render. `is_char_boundary(0)` is always true,
        // so this ends.
        let mut cut = budget;
        while cut > 0 && !head.is_char_boundary(cut) {
            cut -= 1;
        }
        head.truncate(cut);
        // A cut lands wherever the budget ran out, which can be mid-word and after a space.
        // Trailing blanks in a directory name are legal and invisible, which is worse than
        // either having them or not.
        while head.ends_with(' ') {
            head.pop();
        }
    }
    format!("{head}{tail}")
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
