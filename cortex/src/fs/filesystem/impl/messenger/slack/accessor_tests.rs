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
            .download_file("https://evil.example.com/f.pdf", Some(start..end), Some(100))
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
        Err(e) => assert!(e.to_string().contains("may send the token to"), "{e}"),
    };

    assert!(check("https://files.slack.com/f.pdf", None).is_ok());
    assert!(check("https://slack.com/f.pdf", None).is_ok());
    refused("https://evil.example.com/f.pdf", None);
    // A suffix match on the wrong side of the dot, and the `slack.com` name as someone
    // else's subdomain — the two ways a bare `contains` would let a lookalike through.
    refused("https://slack.com.evil.example/f.pdf", None);
    refused("https://notslack.com/f.pdf", None);
    // The host is right and the origin is not. Plain HTTP would put the token on the wire in
    // clear, and a port Slack does not serve is not Slack.
    refused("http://files.slack.com/f.pdf", None);
    refused("https://files.slack.com:8443/f.pdf", None);
    // Userinfo is not the host, whatever it is spelled to look like.
    refused("https://files.slack.com@evil.example/f.pdf", None);

    // A base_url deployment narrows it to that one origin: Slack's own hosts are no longer
    // where this mount's files come from, and neither is any other port or scheme on the
    // gateway's own host — one file URL must not become an authenticated GET against whatever
    // else is listening there.
    let mock = Some("http://localhost:8000");
    assert!(check("http://localhost:8000/files/f.pdf", mock).is_ok());
    refused("https://files.slack.com/f.pdf", mock);
    refused("http://localhost:22/f.pdf", mock);
    refused("http://localhost:9200/_search", mock);
    refused("https://localhost:8000/f.pdf", mock);
}

/// The two halves of a page, which is everything Slack-specific about pagination. `walk` is
/// tested beside this and knows none of it; between them they cover the whole loop.
#[test]
fn a_page_yields_its_items_and_where_the_next_one_is() {
    let page = serde_json::json!({
        "ok": true,
        "channels": [{"id": "C1"}, {"id": "C2"}],
        "response_metadata": {"next_cursor": "dGVhbTpDMg=="}
    });
    assert_eq!(page_items(&page, "channels").len(), 2);
    assert_eq!(next_cursor(&page).as_deref(), Some("dGVhbTpDMg=="));
    // And the key is per method, so asking with the wrong one must not silently borrow another.
    assert!(page_items(&page, "members").is_empty());
}

/// Slack ends a walk two ways and both have to read as the end. An empty string read as a
/// cursor asks for page one again, forever.
#[test]
fn the_end_of_a_walk_is_an_absent_or_an_empty_cursor() {
    for meta in [
        serde_json::json!({"ok": true}),
        serde_json::json!({"ok": true, "response_metadata": {}}),
        serde_json::json!({"ok": true, "response_metadata": {"next_cursor": ""}}),
    ] {
        assert_eq!(next_cursor(&meta), None, "{meta}");
    }
}

/// A quiet conversation answers `ok: true` with the key missing, which is not a failure and not
/// a fault — it is a channel where nothing was said.
#[test]
fn a_page_missing_its_key_is_empty_rather_than_an_error() {
    assert!(page_items(&serde_json::json!({"ok": true}), "messages").is_empty());
    // Present but not an array is the same: nothing to read, and nothing to panic about.
    assert!(page_items(&serde_json::json!({"messages": 3}), "messages").is_empty());
}

/// A page ceiling is what the flag this replaced reported, and reporting it was all anyone did
/// with it. Sixty pages is past the fifty the ceiling used to stand at, so this is the case
/// that used to come back short and call itself whole.
#[tokio::test]
async fn a_walk_follows_its_cursor_past_where_the_old_ceiling_stood() {
    let pages = 60;
    let got = walk(PAGE_GUARD, "test.method", |cursor| async move {
        let n: usize = cursor.as_deref().unwrap_or("0").parse().expect("a number");
        let items = vec![Value::from(n)];
        let next = (n + 1 < pages).then(|| (n + 1).to_string());
        Ok((items, next))
    })
    .await
    .expect("a walk that ends");
    assert_eq!(got.len(), pages, "every page arrived");
}

/// The last page carries items *and* no cursor, so the two have to be read in that order. Read
/// the other way round, every listing loses its final page.
#[tokio::test]
async fn the_page_that_ends_the_walk_keeps_its_items() {
    let got = walk(PAGE_GUARD, "test.method", |cursor| async move {
        match cursor.as_deref() {
            None => Ok((vec![Value::from(1)], Some("2".to_string()))),
            _ => Ok((vec![Value::from(2)], None)),
        }
    })
    .await
    .expect("a walk that ends");
    assert_eq!(got, vec![Value::from(1), Value::from(2)]);
}

/// A walk broken off partway fails, and the pages it had are dropped with it.
///
/// Deliberately, and it is the expensive choice: those requests are spent for nothing. But a
/// short listing does not read as short in this tree — a day it does not name reads as a day
/// that held nothing, and a conversation it does not name is unreachable by path — so serving
/// one would teach the caller something untrue. An error teaches it to ask again.
#[tokio::test]
async fn a_walk_broken_off_partway_fails_rather_than_answering_short() {
    let r = walk(PAGE_GUARD, "test.method", |cursor| async move {
        match cursor.as_deref() {
            None => Ok((vec![Value::from(1)], Some("2".to_string()))),
            _ => Err(api_err("ratelimited")),
        }
    })
    .await;
    assert!(
        matches!(&r, Err(SourceError::Api(e)) if e.code == "ratelimited"),
        "the pages before it are not an answer: {r:?}"
    );
}

/// The guard is for a cursor that never ends, which is a fault rather than a long listing —
/// so it answers as one instead of handing back a prefix.
#[tokio::test]
async fn a_cursor_that_never_ends_is_a_fault() {
    let r = walk(3, "test.method", |_| async move {
        Ok((vec![Value::from(0)], Some("more".to_string())))
    })
    .await;
    assert!(matches!(r, Err(SourceError::Io(_))), "{r:?}");
}

// --- a loopback Slack ----------------------------------------------------------------------
//
// Everything above this line asserts a decision without a socket, which is right for a
// decision. What none of it can reach is `paginate`: stub its body to return no items and no
// cursor and the whole suite stays green, because the cursor member, the `limit` it asks for and
// the ordering the two history methods apply are all *between* the free functions and the tree.
// `SlackConfig::base_url` exists for a mock, so this is one.

/// A Slack that answers with canned bodies, in order, and remembers what it was asked.
struct Loopback {
    base: String,
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Loopback {
    /// One body per request. A request past the last is not answered, which is how a test that
    /// expected fewer pages than the client asked for fails rather than hangs — the client's
    /// own connect timeout ends it.
    async fn serving(bodies: Vec<String>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = asked.clone();

        tokio::spawn(async move {
            for body in bodies {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // Enough of the head to see the request line. Read to the blank line rather
                // than to EOF: the client keeps the connection open waiting for a response.
                let mut head = Vec::new();
                loop {
                    let mut chunk = [0u8; 512];
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&chunk[..n]),
                    }
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                log.lock().expect("a lock").push(line);

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        Loopback { base, asked }
    }

    fn accessor(&self) -> SlackAccessor {
        SlackAccessor::new(&SlackConfig {
            base_url: Some(self.base.clone()),
            ..config(Some("xoxp-loopback"), None)
        })
        .expect("a client")
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("a lock").clone()
    }
}

/// `paginate` end to end: the cursor comes out of `response_metadata`, goes back in as a
/// parameter, and every page asks for `PAGE_LIMIT`.
#[tokio::test]
async fn paginate_threads_the_cursor_and_asks_for_a_full_page() {
    let fake = Loopback::serving(vec![
        r#"{"ok":true,"channels":[{"id":"C1"}],"response_metadata":{"next_cursor":"NEXT"}}"#
            .to_string(),
        r#"{"ok":true,"channels":[{"id":"C2"}],"response_metadata":{"next_cursor":""}}"#
            .to_string(),
    ])
    .await;

    let got = fake
        .accessor()
        .list_conversations("public_channel")
        .await
        .expect("two pages");

    assert_eq!(got.len(), 2, "both pages arrived: {got:?}");
    let asked = fake.asked();
    assert_eq!(asked.len(), 2, "one request per page: {asked:?}");
    assert!(
        asked.iter().all(|q| q.contains(&format!("limit={PAGE_LIMIT}"))),
        "every page asks for a full one: {asked:?}"
    );
    assert!(
        !asked[0].contains("cursor="),
        "the first page has no cursor to send: {}",
        asked[0]
    );
    assert!(
        asked[1].contains("cursor=NEXT"),
        "the second page sends what the first returned: {}",
        asked[1]
    );
}

/// Slack answers newest-first and the tree partitions by day in the order it is handed, so this
/// sort is the only thing making a day file readable top to bottom. Deleting it passed every
/// other test in this crate.
#[tokio::test]
async fn history_answers_oldest_first_however_slack_ordered_it() {
    let fake = Loopback::serving(vec![
        r#"{"ok":true,"messages":[
             {"ts":"300.000000","text":"third"},
             {"ts":"100.000000","text":"first"},
             {"ts":"200.000000","text":"second"}
           ]}"#
        .to_string(),
    ])
    .await;

    let got = fake
        .accessor()
        .conversation_history("C1", "0", "999")
        .await
        .expect("one page");

    let order: Vec<&str> = got.iter().map(|m| m["ts"].as_str().unwrap_or("")).collect();
    assert_eq!(
        order,
        ["100.000000", "200.000000", "300.000000"],
        "oldest first, whatever order it arrived in"
    );
}

/// A thread's root has to come first or its file cannot be read top to bottom, and a root's ts
/// is the smallest in the thread — so the same sort carries it.
#[tokio::test]
async fn a_thread_answers_with_its_root_first() {
    let fake = Loopback::serving(vec![
        r#"{"ok":true,"messages":[
             {"ts":"100.000200","thread_ts":"100.000000","text":"reply"},
             {"ts":"100.000000","thread_ts":"100.000000","text":"root"}
           ]}"#
        .to_string(),
    ])
    .await;

    let got = fake
        .accessor()
        .conversation_replies("C1", "100.000000")
        .await
        .expect("one page");

    assert_eq!(
        got.first().and_then(|m| m["text"].as_str()),
        Some("root"),
        "the root leads: {got:?}"
    );
}
