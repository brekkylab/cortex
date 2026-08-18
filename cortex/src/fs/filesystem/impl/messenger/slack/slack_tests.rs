//! Tests for the translation from Slack's vocabulary into the lane's.
//!
//! Every case is a `Value` off the wire and the shape it should become. No requests: what the
//! client does with a request is tested beside it, and what the tree does with the result is
//! tested beside that — this file is only the middle, which is where a field read from the
//! wrong key produces a tree that is confidently wrong.

use super::*;

fn ts(s: &str) -> SystemTime {
    parse_ts(s).expect("a well-formed ts")
}

/// A `ts` is a timestamp *and* a message id, so it has to survive both readings exactly. A
/// float round trip loses the low digits and names a message that does not exist.
#[test]
fn a_ts_parses_exactly_and_round_trips() {
    for s in [
        "1785737875.341929",
        "1785737875.000100",
        "0.000001",
        "1785737875.000000",
    ] {
        assert_eq!(to_ts(ts(s)), s, "{s} did not round-trip");
    }
    // A fraction shorter than six digits is padded on the right: `.34` is 340,000µs.
    assert_eq!(to_ts(ts("1785737875.34")), "1785737875.340000");
    // And one with no fraction at all is still an instant.
    assert_eq!(to_ts(ts("1785737875")), "1785737875.000000");
    // Two ids one microsecond apart stay two instants.
    assert_ne!(ts("1785737875.341929"), ts("1785737875.341930"));
}

/// The root's `thread_ts` equals its own `ts`; a reply's points back. Reading that wrong puts
/// every root under itself as a reply, or hides every thread.
#[test]
fn a_thread_is_read_from_thread_ts() {
    let root = message(&serde_json::json!({
        "ts": "100.000000", "thread_ts": "100.000000", "reply_count": 3, "text": "start"
    }))
    .unwrap();
    let t = root.thread.expect("a root in a thread");
    assert_eq!(t.root, None, "a root has no root");
    assert_eq!(t.replies, 3);

    let reply = message(&serde_json::json!({
        "ts": "101.000000", "thread_ts": "100.000000", "text": "answer"
    }))
    .unwrap();
    let t = reply.thread.expect("a reply is in a thread");
    assert_eq!(t.root, Some(MsgId("100.000000".into())));
    assert_eq!(t.replies, 0, "only a root counts replies");

    // A standalone message is not in a thread at all — absent, not a thread with no root.
    let plain = message(&serde_json::json!({"ts": "102.000000", "text": "hi"})).unwrap();
    assert!(plain.thread.is_none());
}

/// A webhook message carries a `bot_id` and a `username` and nothing else. The id is what the
/// platform vouches for; the username is what the message claimed, and the two must not merge
/// — anything that can post could otherwise look like a colleague.
#[test]
fn a_webhook_message_keeps_its_claimed_name_apart() {
    let m = message(&serde_json::json!({
        "ts": "100.000000", "bot_id": "B01", "username": "김철수", "text": "deployed"
    }))
    .unwrap();
    assert_eq!(m.from.id, "B01");
    assert_eq!(m.from.claimed.as_deref(), Some("김철수"));
    assert!(m.from.name.is_none(), "the tree resolves the name, not this");
}

/// A message with no `ts` is not a message: the field is its id, its timestamp and its thread
/// key at once. Dropped rather than served with a made-up one.
#[test]
fn a_message_without_a_ts_is_dropped() {
    assert!(message(&serde_json::json!({"text": "orphan"})).is_none());
    assert!(message(&serde_json::json!({"ts": "not-a-number"})).is_none());
}

/// An attachment needs somewhere to fetch from and the length that fetch is checked against.
/// Without the length a download cannot be told from a login page, so the entry is dropped
/// rather than listed with a zero.
#[test]
fn an_attachment_without_a_size_or_a_url_is_dropped() {
    let with = serde_json::json!({
        "id": "F1", "name": "report.pdf", "size": 966_750,
        "url_private_download": "https://files.slack.com/f"
    });
    let f = file(&with).expect("a complete file");
    assert_eq!(f.id, "F1");
    assert_eq!(f.size, 966_750);

    assert!(file(&serde_json::json!({"id": "F1", "name": "x", "size": 1})).is_none());
    assert!(
        file(&serde_json::json!({"id": "F1", "url_private_download": "https://x/"})).is_none()
    );
    // `url_private` stands in when the download form is absent.
    assert!(
        file(&serde_json::json!({
            "id": "F1", "size": 1, "url_private": "https://files.slack.com/f"
        }))
        .is_some()
    );
}

/// A channel is named; a one-to-one DM has a partner instead, and is named from the roster.
#[test]
fn a_dm_is_named_after_its_partner() {
    let members = vec![serde_json::json!({
        "id": "U1", "name": "chulsoo", "profile": {"display_name": "김철수"}
    })];

    let im = conversation(
        &serde_json::json!({"id": "D1", "is_im": true, "user": "U1"}),
        &members,
    )
    .unwrap();
    assert_eq!(im.kind, ConvKind::Dm);
    assert_eq!(im.name, "김철수");

    // A partner the roster does not have stays addressable under their id.
    let stranger = conversation(
        &serde_json::json!({"id": "D2", "is_im": true, "user": "U9"}),
        &members,
    )
    .unwrap();
    assert_eq!(stranger.name, "U9");

    // A group DM carries a name of its own.
    let mpim = conversation(
        &serde_json::json!({"id": "D3", "is_mpim": true, "name": "mpdm-a--b--c-1"}),
        &members,
    )
    .unwrap();
    assert_eq!(mpim.kind, ConvKind::Dm);
    assert_eq!(mpim.name, "mpdm-a--b--c-1");

    // And a channel is a channel.
    let ch = conversation(&serde_json::json!({"id": "C1", "name": "pricing"}), &[]).unwrap();
    assert_eq!(ch.kind, ConvKind::Channel);
    assert_eq!(ch.name, "pricing");
}

/// Slack's own order of preference for what to call somebody, and a fallback to the id so
/// that a line is always attributable to something.
#[test]
fn a_user_is_named_the_way_slack_names_them() {
    let u = user(&serde_json::json!({
        "id": "U1", "name": "chulsoo",
        "profile": {"display_name": "김철수", "real_name": "Kim Chulsoo"}
    }))
    .unwrap();
    assert_eq!(u.name, "김철수");

    // No display name: the real name.
    let u = user(&serde_json::json!({
        "id": "U2", "name": "minji", "profile": {"real_name": "Park Minji"}
    }))
    .unwrap();
    assert_eq!(u.name, "Park Minji");

    // Neither: the handle.
    let u = user(&serde_json::json!({"id": "U3", "name": "bot"})).unwrap();
    assert_eq!(u.name, "bot");

    // Nothing at all: the id, so the entry is still addressable.
    let u = user(&serde_json::json!({"id": "U4"})).unwrap();
    assert_eq!(u.name, "U4");

    // A row with no id is not a member.
    assert!(user(&serde_json::json!({"name": "nobody"})).is_none());
}

/// A bot-token install has no DMs to enumerate, so the section must be absent rather than
/// empty — an empty one would say this person has no DMs.
///
/// Decided by the config alone — no workspace to ask, which is the point of `capabilities`
/// being sync.
#[test]
fn a_bot_only_install_declares_no_dms() {
    let bot = SlackSource::new(&SlackConfig {
        user_token: None,
        bot_token: Some("xoxb-bot".into()),
        base_url: None,
    })
    .unwrap();
    assert_eq!(
        bot.capabilities(),
        Capabilities {
            channels: true,
            dms: false
        }
    );

    let person = SlackSource::new(&SlackConfig {
        user_token: Some("xoxp-user".into()),
        bot_token: None,
        base_url: None,
    })
    .unwrap();
    assert!(person.capabilities().dms);

    // An empty string is not a token, the same as everywhere else a config is read.
    let empty = SlackSource::new(&SlackConfig {
        user_token: Some(String::new()),
        bot_token: Some("xoxb-bot".into()),
        base_url: None,
    })
    .unwrap();
    assert!(!empty.capabilities().dms);
}
