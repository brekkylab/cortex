//! Unit tests for the Slack Web API client.
//!
//! Kept in a sibling file for the reason the other backends' are: at the foot of the
//! implementation a reader had to scroll past them to find the code. Still a child module,
//! so private items stay reachable.
//!
//! Nothing here makes a request. Every test asserts on a decision — which host is allowed,
//! which delay comes next, which code is soft — because the alternative for most of them is
//! a unit test that sends a real workspace's token somewhere.

use super::*;

fn config(user: Option<&str>, bot: Option<&str>) -> SlackConfig {
    SlackConfig {
        user_token: user.map(String::from),
        bot_token: bot.map(String::from),
        base_url: None,
    }
}

/// What `call` would have built for this code.
fn api_err(code: &str) -> SourceError {
    SourceError::Api(ApiError {
        op: "conversations.history".into(),
        code: code.into(),
        detail: None,
        class: class_of(code),
    })
}

#[test]
fn api_base_defaults_to_slack_and_honors_the_override() {
    assert_eq!(api_base(None), "https://slack.com/api");
    // A mock or gateway serves Slack under /slack/api, and a trailing slash in the
    // configured origin must not double up.
    assert_eq!(
        api_base(Some("http://localhost:8000")),
        "http://localhost:8000/slack/api"
    );
    assert_eq!(
        api_base(Some("http://localhost:8000/")),
        "http://localhost:8000/slack/api"
    );
}

/// The user token wins when both are present. It is one token for every call rather than a
/// per-method choice: reading as the person is the point, and a bot token would narrow the
/// tree to the bot's own memberships and drop DMs.
#[test]
fn the_user_token_wins_when_both_are_present() {
    let a = SlackAccessor::new(&config(Some("xoxp-user"), Some("xoxb-bot"))).unwrap();
    assert_eq!(a.token(), "xoxp-user");
    assert!(a.search_available());
}

/// A bot-only install still serves a tree (the bot's own conversations), so the bot token is
/// a fallback rather than an error — but search is genuinely impossible, and must be
/// reported rather than attempted.
#[tokio::test]
async fn a_bot_only_install_falls_back_and_cannot_search() {
    let a = SlackAccessor::new(&config(None, Some("xoxb-bot"))).unwrap();
    assert_eq!(a.token(), "xoxb-bot");
    assert!(!a.search_available());
    // No network: the guard rejects before a request is built.
    assert!(a.search_messages("anything", 20).await.is_err());
}

/// A user-only install is the intended shape: no bot scopes needed at all.
#[test]
fn a_user_only_install_is_enough() {
    let a = SlackAccessor::new(&config(Some("xoxp-user"), None)).unwrap();
    assert_eq!(a.token(), "xoxp-user");
    assert!(a.search_available());
}

/// With no credential every call would 401, and the mount would show an empty workspace as a
/// complete one — so it must fail at construction instead. An empty string counts as absent
/// (a round-tripped config can carry `""`).
#[test]
fn a_config_with_no_token_is_rejected() {
    assert!(SlackAccessor::new(&config(None, None)).is_err());
    assert!(SlackAccessor::new(&config(Some(""), Some(""))).is_err());
    // And an empty user token falls through to a real bot token.
    let a = SlackAccessor::new(&config(Some(""), Some("xoxb-bot"))).unwrap();
    assert_eq!(a.token(), "xoxb-bot");
    assert!(!a.search_available());
}

/// Slack's codes in the lane's terms. This table is the only Slack-specific part of failing;
/// what each class then *means* — which is absorbed, which errno it answers with — belongs to
/// every source alike and is tested in `messenger/error_tests.rs`.
#[test]
fn slack_codes_land_in_the_right_class() {
    for code in [
        "not_in_channel",
        "channel_not_found",
        "is_archived",
        "restricted_action",
        "no_permission",
    ] {
        assert_eq!(class_of(code), ErrorClass::ConversationDenied, "{code}");
    }
    // A scope governs a whole kind of conversation, so it is never a per-conversation denial:
    // absorbed there, a section renders as "all of these exist and none has any history".
    assert_eq!(class_of("missing_scope"), ErrorClass::ScopeMissing);
    // About the token itself — every conversation would fail the same way, so serving them
    // empty would present an empty workspace as a complete one.
    for code in [
        "not_authed",
        "invalid_auth",
        "account_inactive",
        "token_revoked",
        "token_expired",
        "ekm_access_denied",
    ] {
        assert_eq!(class_of(code), ErrorClass::Unauthenticated, "{code}");
    }
    // A code this crate has not seen propagates rather than having its reach guessed at.
    // `ratelimited` is here because `call` retries it and only surfaces one it gave up on.
    assert_eq!(class_of("fatal_error"), ErrorClass::Other);
    assert_eq!(class_of("ratelimited"), ErrorClass::Other);
}

/// The class is decided where the error is built, so nothing downstream re-derives it from
/// the code — which is what stops the table above from being read in two places.
#[test]
fn an_error_carries_its_class() {
    assert!(api_err("not_in_channel").is_conversation_denied());
    assert!(!api_err("missing_scope").is_conversation_denied());
    assert!(!SourceError::io("connection reset").is_conversation_denied());
}

#[test]
fn messages_order_by_ts() {
    let m = |ts: &str| serde_json::json!({"ts": ts});
    let mut v = [m("1754210000.000200"), m("1754209000.000100")];
    v.sort_by(|a, b| ts_of(a).total_cmp(&ts_of(b)));
    assert_eq!(ts_of(&v[0]), 1_754_209_000.000_1);
    // A message without a ts sorts first rather than panicking.
    assert_eq!(ts_of(&serde_json::json!({})), 0.0);
}

/// `8..8` asks for nothing, but the inclusive-end conversion has no way to say so: it
/// produced `bytes=8-8`, one byte, which a 206 then handed back unsliced while an in-process
/// slice returned none for the same range. Answering before the request settles both — and
/// this can be tested at all because the answer comes before the host check too, so a host
/// that would be refused proves nothing was sent.
#[tokio::test]
async fn a_range_of_no_bytes_needs_no_request() {
    let a = SlackAccessor::new(&config(Some("xoxp-user"), None)).unwrap();
    // Built from values: an empty or reversed range literal is a lint, and these are exactly
    // the shapes under test.
    for (start, end) in [(8u64, 8u64), (8, 2), (0, 0)] {
        let (bytes, _) = a
            .download_file("https://evil.example.com/f.pdf", Some(start..end), 100)
            .await
            .unwrap_or_else(|e| panic!("{start}..{end} must answer without a request: {e}"));
        assert!(bytes.is_empty(), "{start}..{end} asked for no bytes");
    }
}

/// A file download must refuse to send the mount's token to a host that isn't Slack's —
/// `url_private_download` is API-supplied data, and the token it would leak is the person's
/// own Slack access.
///
/// Asserted on the check rather than through `download_file`, which would send the accepted
/// urls: a passing host is exactly the case that leaves the machine, so testing it that way
/// means a unit test making real requests to Slack and calling the connect failure a result.
#[test]
fn a_file_download_only_ever_reaches_slack() {
    let check = |u: &str, base: Option<&str>| {
        check_file_host(&reqwest::Url::parse(u).expect("a url"), base)
    };
    let refused = |u: &str, base: Option<&str>| match check(u, base) {
        Ok(()) => panic!("{u} was accepted"),
        Err(e) => assert!(e.to_string().contains("not a Slack host"), "{e}"),
    };

    assert!(check("https://files.slack.com/f.pdf", None).is_ok());
    assert!(check("https://slack.com/f.pdf", None).is_ok());
    refused("https://evil.example.com/f.pdf", None);
    // A suffix match on the wrong side of the dot, and the `slack.com` name as someone
    // else's subdomain — the two ways a bare `contains` would let a lookalike through.
    refused("https://slack.com.evil.example/f.pdf", None);
    refused("https://notslack.com/f.pdf", None);

    // A base_url deployment narrows it to that one origin: Slack's own hosts are no longer
    // where this mount's files come from.
    let mock = Some("http://localhost:8000");
    assert!(check("http://localhost:8000/files/f.pdf", mock).is_ok());
    refused("https://files.slack.com/f.pdf", mock);
}
