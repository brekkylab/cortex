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
    assert_eq!(day_dir(at(1_785_798_000)), "2026-08-03");
    // One hour later crosses midnight UTC.
    assert_eq!(day_dir(at(1_785_801_600)), "2026-08-04");
}

#[test]
fn a_message_posted_to_the_conversation_is_in_the_days_own_file() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    assert_eq!(
        chat_path(&c, &msg(1_785_737_875, None), at(1_785_737_875)),
        "channels/pricing__C1/2026-08-03/chat.jsonl"
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
        "channels/pricing__C1/2026-08-03/chat.jsonl"
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
        "channels/pricing__C1/2026-08-03/threads/1785737875.341929/chat.jsonl",
        "the thread's day, not the reply's"
    );
}

#[test]
fn a_dm_lands_in_the_dms_section() {
    let c = conv("김철수", "D1", ConvKind::Dm);
    assert_eq!(
        chat_path(&c, &msg(1_785_737_875, None), at(1_785_737_875)),
        "dms/김철수__D1/2026-08-03/chat.jsonl"
    );
}
