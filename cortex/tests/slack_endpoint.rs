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

use cortex::fs::{FileSystem, MessengerFs, SlackAccessor, SlackConfig, SlackSource, WorkFs};

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
    let vol = MessengerFs::new(
        SlackSource::new(&config).expect("a config with a token builds"),
    );

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

    // Down the date axis to a day that has something. Every level lists without a request,
    // and the axis is a calendar — so the newest directories are usually empty and finding
    // content is a search. What no fixture can check is that the *live* tree agrees with
    // `paths.rs` about how these are spelled.
    let mut probes = 0usize;
    let mut found = None;
    'outer: for c in &channels {
        let conv = format!("/channels/{}", c.name);
        for y in newest(&vol, &conv).await {
            assert_eq!(y.len(), 4, "a year is four digits, got {y:?}");
            for m in newest(&vol, &format!("{conv}/{y}")).await {
                assert_eq!(m.len(), 2, "a month is two, got {m:?}");
                for d in newest(&vol, &format!("{conv}/{y}/{m}")).await {
                    if probes >= 60 {
                        eprintln!("gave up after {probes} probes");
                        break 'outer;
                    }
                    probes += 1;
                    let at = format!("{conv}/{y}/{m}/{d}");
                    let st = vol.stat(Path::new(&format!("{at}/chat.jsonl"))).await;
                    if st.is_ok_and(|st| st.size > 0) {
                        found = Some(at);
                        break 'outer;
                    }
                }
            }
        }
    }
    let Some(day_dir) = found else {
        eprintln!("no day in reach had messages; nothing further to exercise");
        return;
    };
    eprintln!("entering {day_dir} after {probes} probes");
    let conv = day_dir.rsplitn(4, '/').last().unwrap().to_string();

    let entries: Vec<String> = vol
        .list(Path::new(&day_dir))
        .await
        .expect("a day lists")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    eprintln!("  {entries:?}");
    assert!(entries.contains(&"chat.jsonl".to_string()));

    // And the file reads, one JSON object per line, through a handle like any other file.
    let p = format!("{day_dir}/chat.jsonl");
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

    // Outside the span there is nothing, and against a live workspace that is the half of the
    // rule a fixture cannot check: `created` came from Slack rather than from a test.
    for outside in [format!("{conv}/1970"), format!("{conv}/1970/01/02")] {
        assert!(
            matches!(
                vol.stat(Path::new(&outside)).await,
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
            ),
            "{outside} predates the conversation and must not be a directory"
        );
    }
}


/// One level of the date axis, newest first. Listing it costs no request.
async fn newest(vol: &MessengerFs<SlackSource>, at: &str) -> Vec<String> {
    let mut out: Vec<String> = vol
        .list(Path::new(at))
        .await
        .map(|es| es.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default();
    out.sort();
    out.reverse();
    out
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

/// A day that actually has messages, as `(conversation root, "YYYY/MM/DD")`.
///
/// A search and not a lookup, which is the date axis's cost showing up: the axis is a calendar,
/// so the newest directories are usually the emptiest — a workspace quiet for a fortnight has a
/// fortnight of empty days at the top of it. Walking newest-first and stopping at the first
/// non-empty day is what a reader has to do too, which is the point of exercising it here.
///
/// `budget` caps the `stat`s, because each one is a request against a live workspace.
async fn day_with_content(fs: &WorkFs, root: &str, budget: usize) -> Option<(String, String)> {
    let convs = fs
        .list(Path::new(&format!("{root}/channels")))
        .await
        .expect("a mounted store lists its channels");
    let mut spent = 0usize;
    for c in &convs {
        let conv = format!("{root}/channels/{}", c.name);
        for y in newest_first(fs, &conv).await {
            for m in newest_first(fs, &format!("{conv}/{y}")).await {
                for d in newest_first(fs, &format!("{conv}/{y}/{m}")).await {
                    if spent >= budget {
                        eprintln!("gave up after {spent} probes");
                        return None;
                    }
                    spent += 1;
                    let at = format!("{y}/{m}/{d}");
                    let p = format!("{conv}/{at}/chat.jsonl");
                    if fs.stat(Path::new(&p)).await.is_ok_and(|st| st.size > 0) {
                        eprintln!("found content after {spent} probes");
                        return Some((conv, at));
                    }
                }
            }
        }
    }
    None
}

/// One level of the date axis, newest first. Listing it costs nothing.
async fn newest_first(fs: &WorkFs, at: &str) -> Vec<String> {
    let mut out: Vec<String> = fs
        .list(Path::new(at))
        .await
        .map(|es| es.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default();
    out.sort();
    out.reverse();
    out
}

/// Names directly under `path`, sorted — the shape of a listing, without depending on order.
async fn names(fs: &WorkFs, path: &str) -> Vec<String> {
    let mut out: Vec<String> = fs
        .list(Path::new(path))
        .await
        .unwrap_or_else(|e| panic!("{path} lists: {e:?}"))
        .iter()
        .map(|d| d.name.clone())
        .collect();
    out.sort();
    out
}

/// Two workspaces are two mounts, and neither store knows it is not the root.
///
/// This is the shape a second credential turns into — another Slack workspace, and later a
/// Discord guild or a Grid workspace, which one token spans several of. What it pins is the
/// *pair*: [`WorkFs`] re-bases a request onto the store's own root, and a messenger store
/// resolves paths without assuming where it was grafted. Both halves are tested apart from
/// each other; the failure this catches is the one that only appears together — a store that
/// resolved against the mount path would serve nothing under a nested mount, and a table that
/// re-based wrongly would serve the wrong workspace's bytes without erring.
///
/// Both stores here are the same workspace, because the subject is the composition and one
/// credential is all it takes to have two of them. That also makes the strongest assertion
/// available: the same relative path under either mount must read back the same bytes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real Slack token; see this file's docs"]
async fn a_second_workspace_is_a_second_mount() {
    let Some(config) = config() else { return };

    // Independent stores, deliberately: each keeps its own cache and its own attachment
    // window, which is what a second workspace costs and what sharing one would hide.
    let mut fs = WorkFs::new();
    fs.mount(
        "chat/one",
        MessengerFs::new(SlackSource::new(&config).expect("a config with a token builds")),
    )
    .expect("a store mounts");
    fs.mount(
        "chat/two/nested",
        MessengerFs::new(SlackSource::new(&config).expect("a config with a token builds")),
    )
    .expect("a store mounts at a nested path");

    // The table answers for the directories nobody mounted, or the mounts below them are
    // unreachable by a walk.
    assert_eq!(names(&fs, "/").await, ["chat"], "the root names the branch");
    assert_eq!(
        names(&fs, "/chat").await,
        ["one", "two"],
        "an intermediate directory is synthesized from the mount paths"
    );
    assert_eq!(
        names(&fs, "/chat/two").await,
        ["nested"],
        "and so is one that only exists because a mount sits below it"
    );

    // Each mount serves a whole messenger tree, not a fragment of one.
    for at in ["/chat/one", "/chat/two/nested"] {
        let sections = names(&fs, at).await;
        assert!(
            sections.contains(&"channels".to_string()) && sections.contains(&"users".to_string()),
            "{at} must serve its own sections, got {sections:?}"
        );
    }

    // The read path, which is where a mistake would be quiet rather than loud.
    let Some((conv, day)) = day_with_content(&fs, "/chat/one", 60).await else {
        eprintln!("no day in reach had messages; nothing further to exercise");
        return;
    };
    let relative = conv
        .strip_prefix("/chat/one/")
        .expect("the discovered path is under the mount it came from");
    eprintln!("reading {relative}/{day} through both mounts");

    let mut bytes = Vec::new();
    for at in ["/chat/one", "/chat/two/nested"] {
        let p = format!("{at}/{relative}/{day}/chat.jsonl");
        let stat = fs.stat(Path::new(&p)).await.expect("chat.jsonl stats");
        let mut buf = vec![0u8; stat.size as usize];
        let n = fs.read_at(Path::new(&p), &mut buf, 0).await.expect("reads");
        buf.truncate(n);
        assert!(!buf.is_empty(), "the day found with content reads as such: {p}");
        bytes.push(buf);
    }
    assert_eq!(
        bytes[0], bytes[1],
        "the same relative path under either mount is the same file, or the table re-based it \
         onto the wrong root"
    );
    eprintln!("  {} bytes, identical through both", bytes[0].len());

    // A path inside the branch but under no mount is not a directory the walk may enter.
    assert!(
        matches!(
            fs.stat(Path::new("/chat/two/nested/channels/nothing__X0")).await,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
        ),
        "a conversation the workspace does not have must not resolve under a nested mount"
    );
}
