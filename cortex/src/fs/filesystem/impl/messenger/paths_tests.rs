//! Unit tests for the tree's names.
//!
//! Nothing here makes a request: a path is arithmetic over a conversation and an instant, and
//! the point of the file is that one rule serves both the listing and anything pointing back
//! at it.

use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::fs::{Author, ConvId, MsgId, Thread};

fn conv(name: &str, id: &str, kind: ConvKind) -> Conversation {
    Conversation {
        id: ConvId(id.into()),
        name: name.into(),
        kind,
        created: at(0),
    }
}

/// `2026-08-03T06:17:55Z`, the instant a real workspace's thread was started on.
fn at(secs: u64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn msg(ts: u64, thread: Option<Thread>) -> Message {
    Message {
        id: MsgId(format!("{ts}.000000")),
        ts: at(ts),
        from: Author {
            id: "U1".into(),
            name: None,
            claimed: None,
        },
        text: String::new(),
        thread,
        files: Vec::new(),
        raw: serde_json::Value::Null,
    }
}

#[test]
fn a_conversation_directory_carries_both_halves() {
    assert_eq!(
        conv_dir(&conv("pricing", "C1", ConvKind::Channel)),
        "pricing__C1"
    );
}

#[test]
fn a_name_that_would_leave_its_component_is_sanitized() {
    // A channel named with a separator must not open a directory nobody created, and the id
    // half still has to be findable at the end of the name.
    assert_eq!(
        conv_dir(&conv("a/b", "C1", ConvKind::Channel)),
        "a-b__C1",
        "a slash becomes a dash rather than a path component"
    );
    assert_eq!(
        conv_dir(&conv("  ", "C1", ConvKind::Channel)),
        "unnamed__C1"
    );
    assert_eq!(
        conv_dir(&conv(".hidden", "C1", ConvKind::Channel)),
        "hidden__C1"
    );
}

/// A control character is legal in a POSIX name and breaks every format built on lines.
///
/// A newline splits a listing into two entries, a tab splits a search hit's `<path>\t<record>`
/// in the wrong place, and an escape sequence rewrites what a terminal already drew. Slack's own
/// channel names cannot carry one; a display name and an uploaded filename are whoever typed
/// them, which is where this comes from.
#[test]
fn a_control_character_never_reaches_a_path() {
    for (raw, want) in [
        ("two\nlines", "two-lines__C1"),
        ("a\tb", "a-b__C1"),
        ("re\rwrite", "re-write__C1"),
        ("esc\u{1b}[2Kape", "esc-[2Kape__C1"),
    ] {
        let got = conv_dir(&conv(raw, "C1", ConvKind::Channel));
        assert_eq!(got, want, "{raw:?}");
        assert!(
            !got.chars().any(char::is_control),
            "{got:?} still carries one"
        );
    }
}

#[test]
fn a_kind_picks_its_section() {
    assert_eq!(section_of(ConvKind::Channel), "channels");
    assert_eq!(section_of(ConvKind::Dm), "dms");
}

/// The bucketing rule is UTC, which is a decision and not an accident — the same mount read
/// from two zones has to name a message's day the same way.
#[test]
fn a_day_directory_is_the_utc_day() {
    // 2026-08-03T23:00:00Z — still the 3rd in UTC, already the 4th in Seoul.
    assert_eq!(day_dir(at(1_785_798_000)), "2026/08");
    // One hour later crosses midnight UTC.
    assert_eq!(day_dir(at(1_785_801_600)), "2026/08");
}

#[test]
fn a_message_posted_to_the_conversation_is_in_the_days_own_file() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    assert_eq!(
        chat_path(&c, &msg(1_785_737_875, None), at(1_785_737_875)),
        "channels/pricing__C1/2026/08/2026-08-03.jsonl"
    );
}

/// A root is a conversation message: it has a thread, but no root above it, so its line is in
/// the day's own file — the same place the tree lists it.
#[test]
fn a_thread_root_is_still_in_the_days_own_file() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    let root = msg(
        1_785_737_875,
        Some(Thread {
            root: None,
            replies: 4,
        }),
    );
    assert_eq!(
        chat_path(&c, &root, at(1_785_737_875)),
        "channels/pricing__C1/2026/08/2026-08-03.jsonl"
    );
}

/// The one the caller has to help with: a reply is served under the day its *root* was
/// started, which is why `day` is an argument and not `msg.ts`.
#[test]
fn a_reply_is_under_its_roots_day_and_not_its_own() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    let reply = msg(
        1_786_000_000, // days later
        Some(Thread {
            root: Some(MsgId("1785737875.341929".into())),
            replies: 0,
        }),
    );
    assert_eq!(
        chat_path(&c, &reply, at(1_785_737_875)),
        "channels/pricing__C1/2026/08/threads/1785737875.341929.jsonl",
        "the thread's day, not the reply's"
    );
}

#[test]
fn a_dm_lands_in_the_dms_section() {
    let c = conv("김철수", "D1", ConvKind::Dm);
    assert_eq!(
        chat_path(&c, &msg(1_785_737_875, None), at(1_785_737_875)),
        "dms/김철수__D1/2026/08/2026-08-03.jsonl"
    );
}

/// A name longer than a path component may be is cut to fit, and the id survives whole.
///
/// `NAME_MAX` is a hard limit: over it the entry is not served at all, so the conversation
/// disappears from the tree with nothing saying why. What gives way is the readable half.
#[test]
fn a_name_too_long_for_a_component_is_cut_and_the_id_kept() {
    let long = "a".repeat(400);
    let dir = conv_dir(&conv(&long, "C1", ConvKind::Channel));
    assert_eq!(dir.len(), NAME_MAX, "the component fills the limit exactly");
    assert!(dir.ends_with("__C1"), "the id is the address and is never cut");
}

/// The `.json` a user file ends in spends the same budget, which is why the suffix is passed
/// in rather than concatenated after: a caller appending it would put the component back over
/// the limit having just fitted it.
#[test]
fn a_suffix_is_counted_against_the_same_budget() {
    let long = "a".repeat(400);
    let with = entry(&long, "U1", ".json");
    assert_eq!(with.len(), NAME_MAX);
    assert!(with.ends_with("__U1.json"));
    assert_eq!(
        with.trim_end_matches(".json").len(),
        NAME_MAX - ".json".len()
    );
}

/// Cutting by bytes must land on a character boundary. A Korean name is three bytes a
/// character, so a budget that is not a multiple of three falls inside one — and `truncate`
/// panics there rather than producing the broken string, which is the only reason this is
/// visible at all.
#[test]
fn a_multibyte_name_is_cut_between_characters() {
    let long = "가".repeat(200); // 600 bytes
    let dir = conv_dir(&conv(&long, "C1", ConvKind::Channel));
    assert!(dir.len() <= NAME_MAX);
    // 255 - 4 = 251 bytes of budget, which is 83 whole characters and two bytes over.
    let head = dir.trim_end_matches("__C1");
    assert_eq!(head.chars().count(), 83);
    assert!(head.chars().all(|c| c == '가'), "no partial character");
}

/// Two different long names shorten to the same text. Their components still differ, because
/// the id follows — which is what makes cutting the readable half safe.
#[test]
fn two_long_names_that_shorten_alike_are_still_two_directories() {
    let a = conv_dir(&conv(&"x".repeat(400), "C1", ConvKind::Channel));
    let b = conv_dir(&conv(&"x".repeat(500), "C2", ConvKind::Channel));
    assert_ne!(a, b);
    assert_eq!(a.trim_end_matches("__C1"), b.trim_end_matches("__C2"));
}

/// A cut lands wherever the budget ran out, which can be just after a space. A trailing blank
/// in a directory name is legal and invisible, which is worse than either having it or not.
#[test]
fn a_cut_does_not_leave_a_trailing_blank() {
    let name = format!("{} tail", "a".repeat(246));
    let dir = conv_dir(&conv(&name, "C1", ConvKind::Channel));
    assert!(!dir.trim_end_matches("__C1").ends_with(' '), "{dir:?}");
}

/// Short names are untouched — the limit is a ceiling, not a width.
#[test]
fn a_name_that_fits_is_left_alone() {
    assert_eq!(conv_dir(&conv("pricing", "C1", ConvKind::Channel)), "pricing__C1");
}
