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

/// The directories inside a day.
pub(super) const THREADS: &str = "threads";
pub(super) const FILES: &str = "files";

/// The two directories a month is listed under, and the file a day is served as.
///
/// A month is the *fetch* unit and a day is the *file* unit, which is the whole of why they are
/// spelled separately. One request buys a month, and what it learned is which days in it have
/// anything — so the month's listing names exactly those, and no day directory has to exist for
/// a day nobody spoke on. Splitting the fetch any finer would cost a request per day to find
/// out the same thing.
///
/// The year is its own directory so that a year is a *path*: `grep -r .../2025/` is the
/// question "what happened last year" asked without knowing how months are spelled, where one
/// flat `2025-03/` level needs a glob and the convention behind it. It also keeps a listing
/// under twelve entries however old the conversation is.
///
/// The day's name repeats both, deliberately. `2026-08-03.jsonl` says what it is when a path is
/// truncated, a basename is passed on, or a file is copied somewhere else — the same trade
/// `<name>__<id>` makes, where the id alone would have addressed it.
pub(super) const DAY_FMT: &str = "%Y-%m-%d";
/// What a rendered run of messages is served as, whichever scope it came from.
pub(super) const JSONL: &str = ".jsonl";

/// The year directory `at` falls in.
pub(super) fn year_dir(year: i32) -> String {
    format!("{year:04}")
}

/// The month directory inside its year.
pub(super) fn month_dir(month: u32) -> String {
    format!("{month:02}")
}

/// `<year>/<month>`, which is where a day file lives.
pub(super) fn month_path(at: NaiveDate) -> String {
    format!("{}/{}", year_dir(at.year()), month_dir(at.month()))
}

/// The file a day's messages are served as, inside its month.
pub(super) fn day_file(at: NaiveDate) -> String {
    format!("{}{JSONL}", at.format(DAY_FMT))
}

/// The `(year, month)` a `<year>/<month>` pair means, or `None` when either is not a name this
/// writes.
///
/// Strict about width as well as value: `8` is not a name any listing produced, and taking it
/// would give one month two paths — and two cache entries that never see each other.
pub(super) fn month_of(year: &str, month: &str) -> Option<(i32, u32)> {
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    let (y, m) = (year.parse().ok()?, month.parse::<u32>().ok()?);
    (1..=12).contains(&m).then_some((y, m))
}


/// Every month between `created` and `today`, oldest first.
///
/// The whole date axis of a conversation, and arithmetic — so listing it costs nothing and is
/// never a window. The count is the conversation's *lifetime* in months and has nothing to do
/// with how much was said: a channel carrying fifty thousand messages a day has no more entries
/// than a silent one of the same age.
pub(super) fn months(created: NaiveDate, today: NaiveDate) -> Vec<(i32, u32)> {
    let mut out = Vec::new();
    let (mut y, mut m) = (created.year(), created.month());
    while (y, m) <= (today.year(), today.month()) {
        out.push((y, m));
        (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    }
    out
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
    month_path(day_of(at))
}

/// The tree-relative path of the file this message's line is in.
///
/// `day` is the instant whose UTC day names the directory — a message's own for one posted to
/// the conversation, and its **thread root's** for a reply, because a thread is served under
/// the day it was started on. A caller has to pass it: a root is named by a platform id, which
/// is opaque here, so this cannot recover the root's instant from the message alone.
pub fn chat_path(conv: &Conversation, msg: &Message, day: SystemTime) -> String {
    let date = day_of(day);
    let dir = format!(
        "{}/{}/{}",
        section_of(conv.kind),
        conv_dir(conv),
        month_path(date)
    );
    match msg.thread.as_ref().and_then(|t| t.root.as_ref()) {
        Some(root) => format!("{dir}/{THREADS}/{}{JSONL}", root.0),
        None => format!("{dir}/{}", day_file(date)),
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
        // A control character is legal in a POSIX name and hostile in every other way. A
        // newline breaks anything that reads a listing a line at a time, a tab breaks the
        // `<path>\t<record>` a search hit is printed as, and an escape sequence rewrites
        // whatever a terminal already drew. Slack's own channel names cannot carry one, but a
        // display name and an uploaded filename are whoever typed them.
        .map(|c| if c == '/' || c.is_control() { '-' } else { c })
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
