//! A real Slack workspace, driven through [`SlackAccessor`].
//!
//! The unit tests beside the client assert *decisions* — which host may receive the token,
//! which delay comes next, which code lands in which class — because the alternative for most
//! of them is a test that sends a real workspace's token somewhere. What none of them can
//! show is that the requests are right: the parameter names, the cursor member, where the
//! messages actually live in a response, and Slack's habit of reporting failure inside a 200.
//! Those are upstream's to produce and nothing local stands in for them.
//!
//! Skipped, not failed, when there is no token: a suite with no credentials should stay
//! green.
//!
//! ```sh
//! set -a; . ./.env; set +a
//! cargo test --features slack --test slack_endpoint -- --ignored --nocapture
//! ```
//!
//! `SLACK_USER_TOKEN` is the workspace this reads, as the person who installed the app — see
//! [`SlackConfig::user_token`] for why it is the user's token and not a bot's.
//! `SLACK_BASE_URL` points the same tests at a mock or gateway instead.
//!
//! Read-only, and nothing here is named: the workspace is discovered from
//! `conversations.list` down, so this survives a workspace it has never seen. It reads
//! whatever the token can already read and writes nothing.

#![cfg(feature = "slack")]

use std::path::Path;

use cortex::fs::{FileSystem, MessengerFs, SlackAccessor, SlackConfig, SlackSource};

/// The workspace these read, or `None` when there is no credential to name one with.
fn config() -> Option<SlackConfig> {
    let token = std::env::var("SLACK_USER_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        eprintln!("skipping: SLACK_USER_TOKEN is not set");
        return None;
    };
    Some(SlackConfig {
        user_token: Some(token),
        bot_token: None,
        base_url: std::env::var("SLACK_BASE_URL")
            .ok()
            .filter(|u| !u.is_empty()),
    })
}

/// The client under test.
fn accessor() -> Option<SlackAccessor> {
    Some(SlackAccessor::new(&config()?).expect("a config with a token builds"))
}

/// A message's `ts`, as the string Slack keys everything by.
fn ts_of(m: &serde_json::Value) -> &str {
    m.get("ts")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// The bytes a file of this name has to begin with, for the formats worth being sure about.
///
/// Not a content-type library — a handful of unambiguous signatures is all this needs. The
/// question being asked is only "are these the file's bytes or somebody's login page", and
/// `None` (an extension not listed) still gets the HTML check, which is the one that matters.
fn magic_for(name: &str) -> Option<&'static [u8]> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => b"%PDF-",
        "png" => b"\x89PNG",
        "gif" => b"GIF8",
        "jpg" | "jpeg" => b"\xff\xd8\xff",
        // Office formats and every other zip container.
        "zip" | "docx" | "xlsx" | "pptx" => b"PK\x03\x04",
        _ => return None,
    })
}

/// The UTC day containing `ts`, as the `(oldest, latest)` pair a history window takes.
///
/// Computed from the ts rather than from today, because a workspace this has never seen may
/// have gone quiet months ago — asking for today would prove only that an empty window comes
/// back empty.
fn day_around(ts: &str) -> (String, String) {
    let secs = ts
        .split('.')
        .next()
        .unwrap_or("0")
        .parse::<i64>()
        .unwrap_or(0);
    let start = secs - secs.rem_euclid(86_400);
    (start.to_string(), (start + 86_400).to_string())
}

/// The whole read surface, over whatever the token can see.
///
/// One test and not five, because each step needs what the one before it discovered: there is
/// no channel id to ask history for until `conversations.list` has answered, and no thread to
/// expand until a day's history has turned one up.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real Slack token; see this file's docs"]
async fn a_real_workspace_answers_the_read_surface() {
    let Some(api) = accessor() else { return };

    // --- conversations -------------------------------------------------------------
    let (channels, truncated) = api
        .list_conversations("public_channel,private_channel")
        .await
        .expect("conversations.list");
    eprintln!(
        "channels: {} (walk {})",
        channels.len(),
        if truncated { "truncated" } else { "complete" }
    );
    assert!(
        !channels.is_empty(),
        "a token that can list nothing cannot exercise anything below"
    );
    for c in channels.iter().take(3) {
        eprintln!(
            "  {} {}",
            c["id"].as_str().unwrap_or("?"),
            c["name"].as_str().unwrap_or("?")
        );
    }

    // --- members -------------------------------------------------------------------
    let (users, _) = api.list_users().await.expect("users.list");
    eprintln!("users: {}", users.len());
    assert!(!users.is_empty(), "a workspace has at least the installer");

    // --- which days a conversation has ----------------------------------------------
    // The bounded walk the tree's date listing is built on. Whichever conversation the token
    // can actually read: a bot in none of them lists channels fine and reads none of them,
    // which is the soft-fail path rather than a failure.
    let mut scanned = None;
    for c in &channels {
        let id = c["id"].as_str().unwrap_or_default();
        match api.scan_history(id, 2).await {
            Ok((msgs, trunc)) if !msgs.is_empty() => {
                scanned = Some((id.to_string(), msgs, trunc));
                break;
            }
            Ok(_) => continue,
            // One conversation being unreadable must not fail the tree — the whole point of
            // the class. Anything wider propagates and fails this test, which is correct.
            Err(e) if e.is_conversation_denied() => continue,
            Err(e) => panic!("scan_history: {e}"),
        }
    }
    let Some((channel, msgs, truncated)) = scanned else {
        eprintln!("no readable conversation had messages; nothing further to exercise");
        return;
    };
    eprintln!(
        "scanned {channel}: {} messages, walk {}",
        msgs.len(),
        if truncated { "truncated" } else { "complete" }
    );

    // Oldest-first is what the tree's date bucketing assumes.
    let order: Vec<&str> = msgs.iter().map(ts_of).collect();
    assert!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "scan_history must answer oldest-first"
    );

    // --- one day's history ----------------------------------------------------------
    let newest = ts_of(msgs.last().expect("non-empty"));
    let (oldest, latest) = day_around(newest);
    let (day, _) = api
        .conversation_history(&channel, &oldest, &latest)
        .await
        .expect("conversations.history");
    eprintln!("day {oldest}..{latest}: {} messages", day.len());
    assert!(
        !day.is_empty(),
        "the day holding the newest message it just returned cannot be empty"
    );
    // The window is the tree's whole addressing scheme: a day directory that served messages
    // from another day would be a path that names the wrong thing.
    for m in &day {
        let t = ts_of(m)
            .split('.')
            .next()
            .unwrap_or("0")
            .parse::<i64>()
            .unwrap_or(0);
        assert!(
            (oldest.parse::<i64>().unwrap()..=latest.parse::<i64>().unwrap()).contains(&t),
            "conversations.history returned {t}, outside {oldest}..{latest}"
        );
    }

    // --- a thread -------------------------------------------------------------------
    let root = day.iter().find(|m| {
        m.get("reply_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0
    });
    match root {
        Some(r) => {
            let (thread, _) = api
                .conversation_replies(&channel, ts_of(r))
                .await
                .expect("conversations.replies");
            eprintln!("thread {}: {} messages", ts_of(r), thread.len());
            // The root comes first, which is what lets a thread's file be read top to bottom.
            assert_eq!(
                ts_of(thread.first().expect("a thread has its root")),
                ts_of(r),
                "a thread must start with its root"
            );
            assert!(
                thread.len() > 1,
                "a root with replies returns more than itself"
            );
        }
        None => eprintln!("no threaded message in that day; replies not exercised"),
    }

    // --- an attachment --------------------------------------------------------------
    // The one path that is not the JSON API: a bearer download whose length is checked
    // against the listing, because an unaccepted token gets a login page answering 200.
    let file = day
        .iter()
        .filter_map(|m| m.get("files").and_then(serde_json::Value::as_array))
        .flatten()
        .find(|f| f.get("url_private_download").is_some() && f.get("size").is_some());
    match file {
        Some(f) => {
            let url = f["url_private_download"].as_str().unwrap();
            let size = f["size"].as_u64().unwrap();
            let name = f["name"].as_str().unwrap_or("?");
            let (bytes, _) = api.download_file(url, None, size).await.expect("download");
            eprintln!("file {name}: {} bytes", bytes.len());
            assert_eq!(bytes.len() as u64, size);

            // The length alone does not say these are the file's bytes. The failure this is
            // really about produced a *login page* where a file was expected — HTML answering
            // 200 after a redirect — and a reader that only counts bytes serves that as the
            // document. So the first bytes have to be what the name claims they are.
            let head = &bytes[..bytes.len().min(16)];
            eprintln!("  head: {:?}", String::from_utf8_lossy(head));
            assert!(
                !bytes.starts_with(b"<!DOCTYPE") && !bytes.starts_with(b"<html"),
                "{name} came back as HTML — the token was not accepted"
            );
            if let Some(magic) = magic_for(name) {
                assert!(
                    bytes.starts_with(magic),
                    "{name} does not begin with {:?}",
                    String::from_utf8_lossy(magic)
                );
                eprintln!("  magic: ok");
            }

            // And a ranged read must be the same bytes at the same offsets, since that is how
            // a kernel will actually read this file — never once, whole.
            let mid = (size / 2) as usize;
            let (chunk, served_range) = api
                .download_file(url, Some(mid as u64..mid as u64 + 64), size)
                .await
                .expect("ranged download");
            let want = &bytes[mid..(mid + 64).min(bytes.len())];
            let got = if served_range {
                &chunk[..]
            } else {
                &chunk[mid..(mid + 64).min(chunk.len())]
            };
            assert_eq!(got, want, "a ranged read disagreed with the whole file");
            eprintln!(
                "  range {mid}..{}: {} bytes, {}",
                mid + 64,
                got.len(),
                if served_range {
                    "served by the host"
                } else {
                    "sliced locally"
                }
            );
        }
        None => eprintln!("no attachment in that day; download not exercised"),
    }
}

/// The same workspace, as a filesystem.
///
/// Everything above drives the client. This drives the whole stack — spec, source, tree — the
/// way a mount will: walk the sections, enter a conversation, read a day, and open the file.
/// Nothing about the layout is Slack's, which is the claim worth checking against a live
/// account rather than a fixture: the days are the days that workspace *has*, and they came
/// from a bounded walk of real history.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real Slack token; see this file's docs"]
async fn the_workspace_opens_as_a_tree() {
    let Some(config) = config() else { return };
    let vol = MessengerFs::new(SlackSource::new(&config).expect("a source"));

    let sections: Vec<String> = vol
        .list(Path::new("/"))
        .await
        .expect("the root lists")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    eprintln!("sections: {sections:?}");
    assert!(sections.contains(&"channels".to_string()));
    assert!(sections.contains(&"users".to_string()));

    let channels = vol.list(Path::new("/channels")).await.expect("channels");
    eprintln!("channels: {}", channels.len());
    assert!(!channels.is_empty(), "a workspace with no channels proves nothing");

    // Whichever conversation actually has days. One a token cannot read is listed and empty,
    // which is the soft-fail path rather than a failure — so this looks for one with content.
    let mut found = None;
    for c in &channels {
        let at = format!("/channels/{}", c.name);
        let days = vol.list(Path::new(&at)).await.expect("a conversation lists");
        if let Some(d) = days.last() {
            found = Some((at, d.name.clone()));
            break;
        }
    }
    let Some((conv, day)) = found else {
        eprintln!("no conversation had any days; nothing further to exercise");
        return;
    };
    eprintln!("entering {conv}/{day}");

    // A date directory, and it parses as one — the tree's addressing scheme is a real date.
    assert!(
        chrono_like(&day),
        "a day directory must be YYYY-MM-DD, got {day:?}"
    );

    let entries: Vec<String> = vol
        .list(Path::new(&format!("{conv}/{day}")))
        .await
        .expect("a day lists")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    eprintln!("  {entries:?}");
    assert!(entries.contains(&"chat.jsonl".to_string()));

    // And the file reads, one JSON object per line, through a handle like any other file.
    let p = format!("{conv}/{day}/chat.jsonl");
    let stat = vol.stat(Path::new(&p)).await.expect("chat.jsonl stats");
    let mut buf = vec![0u8; stat.size as usize];
    let n = vol
        .read_at(Path::new(&p), &mut buf, 0)
        .await
        .expect("reads");
    buf.truncate(n);
    let text = String::from_utf8(buf).expect("utf-8");

    let lines: Vec<&str> = text.lines().collect();
    eprintln!("  chat.jsonl: {} bytes, {} lines", stat.size, lines.len());
    assert!(!lines.is_empty(), "a listed day is never empty");
    for l in &lines {
        let v: serde_json::Value = serde_json::from_str(l).expect("one object per line");
        assert!(v.get("ts").is_some(), "a line carries its instant: {l}");
        assert!(v.get("from").is_some(), "a line carries its author: {l}");
    }
    eprintln!("  first: {}", lines[0]);

    // A silent day inside the walked range is refused rather than served empty. The day
    // before the newest one is a guess, so this only asserts the shape of the answer.
    let missing = format!("{conv}/1970-01-02");
    assert!(
        matches!(
            vol.stat(Path::new(&missing)).await,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
        ),
        "a day the workspace does not have must not be a directory"
    );
}

/// `YYYY-MM-DD`, without taking a calendar dependency in a test whose subject is not dates.
fn chrono_like(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && s.chars().filter(char::is_ascii_digit).count() == 8
}

/// Slack answers a bad token with HTTP 200 and `{"ok": false, "error": "invalid_auth"}`. That
/// is the convention the client's single gate exists for, and the one thing no local test can
/// produce — a status-only check would read this as a successful empty response and the tree
/// would present an unusable workspace as an empty one.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network reachability to Slack; see this file's docs"]
async fn a_bad_token_is_refused_rather_than_read_as_empty() {
    // Not gated on `SLACK_USER_TOKEN`: the point is a token that does not work, and this one
    // is well-formed enough to be sent and certain to be refused.
    let config = SlackConfig {
        user_token: Some("xoxp-0000000000-0000000000-0000000000-0000000000000000".into()),
        bot_token: None,
        base_url: std::env::var("SLACK_BASE_URL")
            .ok()
            .filter(|u| !u.is_empty()),
    };
    let api = SlackAccessor::new(&config).expect("a config with a token builds");

    let err = api
        .list_conversations("public_channel")
        .await
        .expect_err("a bad token must not read as an empty workspace");
    eprintln!("bad token: {err}");

    // And it must not be absorbed as one quiet conversation: the failure is the credential's,
    // so it applies to every conversation and the tree has to say so.
    assert!(
        !err.is_conversation_denied(),
        "a token failure must propagate, not be served as an empty conversation: {err}"
    );
}
