//! Tests for the one line format.
//!
//! These are the lane's contract tests in the strictest sense: [`render_line`] is the single
//! place a message becomes bytes, so what is asserted here is true of every source that will
//! ever be added — which is the whole reason the function is shared rather than implemented
//! per platform.

use std::time::Duration;

use super::*;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

fn msg(text: &str) -> Message {
    Message {
        id: MsgId("1754209875.000100".into()),
        ts: at(1_754_209_875),
        from: Author {
            id: "U0BM3F1".into(),
            name: Some("김철수".into()),
            claimed: None,
        },
        text: text.into(),
        thread: None,
        files: Vec::new(),
        raw: serde_json::json!({"platform": "only"}),
    }
}

fn parsed(m: &Message) -> Value {
    let bytes = render_line(m);
    assert_eq!(
        bytes.iter().filter(|b| **b == b'\n').count(),
        1,
        "a message is exactly one line"
    );
    assert_eq!(bytes.last(), Some(&b'\n'), "the line is newline-terminated");
    serde_json::from_slice(&bytes).expect("a line is one JSON object")
}

/// The shape §4.3 of the design memo promises, and the one a `jq` filter is written against.
#[test]
fn a_line_carries_the_agreed_fields() {
    let v = parsed(&msg("가격 정책은 다음 주에 하죠"));
    assert_eq!(v["ts"], "2025-08-03T08:31:15Z");
    assert_eq!(v["id"], "1754209875.000100");
    assert_eq!(v["from"]["id"], "U0BM3F1");
    assert_eq!(v["from"]["name"], "김철수");
    assert_eq!(v["text"], "가격 정책은 다음 주에 하죠");
    // Absent rather than null: a reader filtering on `.thread` must not see every standalone
    // message as a thread with no root.
    assert!(v.get("thread").is_none(), "{v}");
    assert!(v.get("files").is_none(), "{v}");
}

/// The invariant `grep` depends on. A body with newlines in it must not become several lines,
/// or a hit is half a message with somebody else's name attached to it.
#[test]
fn a_body_with_newlines_is_still_one_line() {
    let v = parsed(&msg("first\nsecond\r\nthird\n"));
    assert_eq!(v["text"], "first\nsecond\r\nthird\n");
}

/// A control character, a lone surrogate's worth of escaping, and a quote — the shapes that
/// break a hand-rolled writer. Asserted by round-trip, because that is what a reader does.
#[test]
fn a_body_survives_whatever_is_in_it() {
    for body in [
        "\"quoted\"",
        "tab\there",
        "nul\u{0}byte",
        "back\\slash",
        "]}{[",
    ] {
        let v = parsed(&msg(body));
        assert_eq!(v["text"], body, "{body:?} did not round-trip");
    }
}

/// A claimed name never becomes the resolved one. A webhook can assert any `username` it
/// likes, and merging the two would let anything that can post look like a colleague — to a
/// reader that has no way to tell unless the line says so.
#[test]
fn a_claimed_name_stays_apart_from_the_resolved_one() {
    let mut m = msg("deploy finished");
    m.from.name = None;
    m.from.claimed = Some("김철수".into());
    let v = parsed(&m);
    assert_eq!(v["from"]["claimed"], "김철수");
    assert!(
        v["from"].get("name").is_none(),
        "an unresolved id must not borrow the claimed name: {v}"
    );

    // And with both, both are present and distinguishable.
    let mut m = msg("deploy finished");
    m.from.claimed = Some("Deploy Bot".into());
    let v = parsed(&m);
    assert_eq!(v["from"]["name"], "김철수");
    assert_eq!(v["from"]["claimed"], "Deploy Bot");
}

/// A root counts its replies; a reply names its root. Keyed by the root's *id* so a deleted
/// root does not take its thread's lines with it.
#[test]
fn a_thread_is_addressed_by_its_root_id() {
    let mut m = msg("가격 이야기");
    m.thread = Some(Thread {
        root: None,
        replies: 3,
    });
    let v = parsed(&m);
    assert_eq!(v["thread"]["replies"], 3);
    assert_eq!(v["thread"]["root"], Value::Null, "a root has no root");

    m.thread = Some(Thread {
        root: Some(MsgId("1754209000.000100".into())),
        replies: 0,
    });
    let v = parsed(&m);
    assert_eq!(v["thread"]["root"], "1754209000.000100");
}

/// Attachments are listed with the exact size a download will be checked against.
#[test]
fn files_carry_the_length_a_download_is_verified_by() {
    let mut m = msg("보고서 첨부");
    m.files = vec![FileRef {
        id: "F1".into(),
        name: "report.pdf".into(),
        size: 966_750,
        // Never rendered: a url is a credential-bearing fetch the source performs, not
        // something a reader should be handed.
        url: "https://files.slack.com/secret".into(),
    }];
    let v = parsed(&m);
    assert_eq!(v["files"][0]["name"], "report.pdf");
    assert_eq!(v["files"][0]["size"], 966_750);
    assert_eq!(v["files"][0]["id"], "F1");
    let line = String::from_utf8(render_line(&m)).unwrap();
    assert!(
        !line.contains("files.slack.com"),
        "a line must not carry a download url: {line}"
    );
}

/// Second resolution, and fixed width. A sortable prefix is what lets a reader order lines
/// from two files without parsing them.
#[test]
fn a_timestamp_is_fixed_width_utc_seconds() {
    let mut m = msg("x");
    m.ts = at(0);
    assert_eq!(parsed(&m)["ts"], "1970-01-01T00:00:00Z");
    // A sub-second component is dropped rather than rendered: it means something different
    // on each platform, and a reader sorting on it would be sorting on that difference.
    m.ts = at(1_754_209_875) + Duration::from_micros(999_999);
    assert_eq!(parsed(&m)["ts"], "2025-08-03T08:31:15Z");
}
