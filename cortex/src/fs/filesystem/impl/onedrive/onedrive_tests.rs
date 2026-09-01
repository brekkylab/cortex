use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::{OnedriveConfig, OnedriveOrigins};
use super::*;

// ---------------------------------------------------------------------------
// Pure, no I/O
// ---------------------------------------------------------------------------

/// A package — a OneNote notebook is the one you meet — is neither a file nor a folder,
/// and has no bytes this API will serve. A name that cannot be read is worse than an
/// absence, so it is not listed.
#[test]
fn a_package_and_a_facetless_row_are_not_listed() {
    let mut notebook = folder_row("Work Notes", "N1");
    notebook
        .as_object_mut()
        .unwrap()
        .insert("package".into(), json!({"type": "oneNote"}));
    assert!(
        child_from_item(&notebook).is_none(),
        "a package is dropped even though it also carries a folder facet"
    );

    let mut neither = json!({"id": "X1", "name": "mystery", "size": 10});
    assert!(child_from_item(&neither).is_none(), "no file, no folder");
    neither
        .as_object_mut()
        .unwrap()
        .insert("file".into(), json!({}));
    assert!(
        child_from_item(&neither).is_some(),
        "a file facet is enough"
    );
}

/// A name is one path segment. OneDrive refuses most of what would need removing, so this
/// guards against a gateway that is less strict — `/` being the one that would silently
/// address something else.
#[test]
fn names_cannot_escape_their_directory() {
    let evil = child_from_item(&file_row("../../etc/passwd", "E1", 1)).unwrap();
    assert!(!evil.name.contains('/'));
    let dots = child_from_item(&file_row("..", "E2", 1)).unwrap();
    assert_eq!(dots.name, "untitled");
    let blank = child_from_item(&file_row("   ", "E3", 1)).unwrap();
    assert_eq!(blank.name, "untitled");
}

/// `..` is refused rather than walked: a name survives the sanitizer with almost anything
/// in it, so a parent reference resolved here would address a directory nobody named.
#[test]
fn a_path_is_normalized_and_a_parent_reference_refused() {
    assert_eq!(vpath(Path::new("/")).unwrap(), "/");
    assert_eq!(vpath(Path::new("/a/b")).unwrap(), "/a/b");
    assert_eq!(vpath(Path::new("/a//b/")).unwrap(), "/a/b");
    assert_eq!(vpath(Path::new("/a/./b")).unwrap(), "/a/b");
    assert!(vpath(Path::new("/a/../b")).is_err(), "`..` is refused");
}

/// A window past the end is empty, not a panic — a hostile range is structurally unable
/// to take the process down.
#[test]
fn a_window_past_the_end_is_empty() {
    let data = b"0123456789";
    assert_eq!(slice(data, Some(2..5)), b"234");
    assert_eq!(slice(data, Some(8..99)), b"89");
    assert_eq!(slice(data, Some(99..200)), b"");
    // Built from values rather than written literally: a backwards range is what a
    // caller's arithmetic produces, never what it types.
    let (from, to) = (5u64, 2u64);
    assert_eq!(
        slice(data, Some(from..to)),
        b"",
        "backwards is empty, not fatal"
    );
    assert_eq!(slice(data, None), data);
}

// ---------------------------------------------------------------------------
// Mock-backed
// ---------------------------------------------------------------------------

/// One listing answers resolution and every attribute under it, at any depth.
///
/// Three things at once, because they are one behaviour. Graph addresses a folder by path,
/// so a path of any depth needs only its parent's listing — Drive has no paths at all and
/// pays one `files.list` per directory to walk to the same place. That listing carries
/// `size`, the times and the tag, so a `stat` after it spends nothing, which is what makes
/// the kernel's per-entry `getattr` free: FUSE-T serves over NFS and an NFS client fills an
/// attribute for every name it lists. And the lookup composes, because macOS hands it the
/// decomposed spelling of whatever the listing returned.
#[tokio::test]
async fn one_listing_answers_the_whole_directory() {
    let composed = "보고서.docx";
    let mock = start(
        json!({
            "/": [folder_row("Documents", "D1")],
            "/Documents": [folder_row("2026", "D2")],
            "/Documents/2026": [
                file_row(composed, "F9", 1234),
                folder_row("drafts", "D3"),
            ],
        }),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&mock.config());
    let deep = PathBuf::from("/Documents/2026").join(composed);

    let st = fs.stat(&deep).await.unwrap();
    assert_eq!(st.size, 1234, "exact, straight off the listing");
    assert_eq!(
        st.etag.as_deref(),
        Some(format!("ctag-{composed}").as_str())
    );
    assert!(st.mtime.is_some());

    let listings: Vec<String> = mock
        .targets()
        .into_iter()
        .filter(|t| t.contains("/children"))
        .collect();
    assert_eq!(
        listings.len(),
        1,
        "three segments deep and still one listing: {listings:?}"
    );
    assert!(
        listings[0].contains("root:/Documents/2026:/children"),
        "addressed by path rather than walked to: {listings:?}"
    );

    // Everything else in that folder is then free — including under the spelling macOS
    // hands a lookup rather than the one the listing used.
    mock.reset();
    let decomposed: String = composed.nfd().collect();
    assert_ne!(composed.as_bytes(), decomposed.as_bytes(), "two spellings");
    assert_eq!(
        fs.stat(&PathBuf::from("/Documents/2026").join(&decomposed))
            .await
            .unwrap()
            .size,
        1234,
        "found under the other spelling"
    );
    let listed = fs.list(Path::new("/Documents/2026")).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed
            .iter()
            .find(|d| d.name == "drafts")
            .and_then(|d| d.stat())
            .map(|s| (s.kind, s.size)),
        Some((DirentKind::Dir, 0)),
        "a folder's `size` is what it contains, not a length"
    );
    assert!(
        mock.targets().is_empty(),
        "none of that cost a request: {:?}",
        mock.targets()
    );
}

/// The span policy, in the cases that distinguish it — and the slot it lives in.
///
/// The kernel's window is 64 KiB and not ours to choose. One ranged request per window is
/// the pathology this exists to avoid: measured against Drive, whose round trip is the same
/// shape, it put a 641 MB archive at 0.04 MB/s. Two sizes rather than one because a span is
/// not free either — a reader that takes a file's head and stops would pay a whole
/// [`READ_SPAN`] for one buffer.
#[tokio::test]
async fn a_walk_pays_a_span_and_a_head_read_pays_less() {
    const REAL: usize = 80 * 1024 * 1024;
    const CHUNK: u64 = 64 * 1024;
    let mock = start(
        json!({"/": [
            file_row("big.bin", "P1", REAL as u64),
            file_row("other.bin", "P2", 4096),
        ]}),
        HashMap::from([
            ("P1".to_string(), vec![b'z'; REAL]),
            ("P2".to_string(), vec![b'y'; 4096]),
        ]),
    )
    .await;
    let fs = mounted(&mock.config());
    let file = Path::new("/big.bin");
    fs.list(Path::new("/")).await.unwrap();

    // A first read takes the smaller span, and the windows after it come out of that span
    // rather than each costing a request — which is the whole point.
    mock.reset();
    for i in 0..8u64 {
        let got = fs
            .read_window(file, Some(i * CHUNK..(i + 1) * CHUNK))
            .await
            .unwrap();
        assert_eq!(got.len() as u64, CHUNK);
    }
    assert_eq!(
        mock.content_ranges(),
        vec![Some(format!("bytes=0-{}", FIRST_SPAN - 1))],
        "eight windows, one span, and it is the smaller one"
    );

    // Carrying on from where that span ended is a walk, and pays the bigger one.
    mock.reset();
    fs.read_window(file, Some(FIRST_SPAN..FIRST_SPAN + CHUNK))
        .await
        .unwrap();
    assert_eq!(
        mock.content_ranges(),
        vec![Some(format!(
            "bytes={}-{}",
            FIRST_SPAN,
            FIRST_SPAN + READ_SPAN - 1
        ))],
        "continuing pays READ_SPAN"
    );

    // An empty window is not a read at all. The guard is at both levels, so neither alone
    // is load-bearing.
    mock.reset();
    assert!(
        fs.read_window(file, Some(1024..1024))
            .await
            .unwrap()
            .is_empty(),
        "an empty window is empty"
    );
    assert_eq!(mock.bytes_sent(), 0, "and moves nothing");
    assert!(mock.content_ranges().is_empty());

    // One slot: another file displaces what was held, and going back costs fetching it
    // again — which is what it cost the first time.
    fs.read_window(Path::new("/other.bin"), Some(0..16))
        .await
        .unwrap();
    assert_eq!(
        fs.held_slot().await.map(|(p, _)| p),
        Some("/other.bin".into()),
        "the second file displaced the first"
    );
    mock.reset();
    fs.read_window(file, Some(0..CHUNK)).await.unwrap();
    assert_eq!(
        mock.content_ranges(),
        vec![Some(format!("bytes=0-{}", FIRST_SPAN - 1))],
        "the displaced file was fetched again, and as a first read rather than a walk"
    );
}

/// A window straddling the end of the held span is still one walk, and the span that
/// answers it begins exactly where the read does.
///
/// This is what makes "a short return means EOF" true. If a span could begin on a fixed
/// boundary instead, a window could fall across two of them and come back half full —
/// which a reader is obliged to read as the end of the file.
#[tokio::test]
async fn a_window_straddling_a_span_boundary_is_still_a_walk() {
    const REAL: usize = 80 * 1024 * 1024;
    // A window size that does not divide the span, so the last one reaches past its end
    // from inside rather than landing on the boundary.
    const CHUNK: u64 = 3 * 1024 * 1024;
    let mock = start(
        json!({"/": [file_row("big.bin", "P1", REAL as u64)]}),
        HashMap::from([("P1".to_string(), vec![b'q'; REAL])]),
    )
    .await;
    let fs = mounted(&mock.config());
    let file = Path::new("/big.bin");
    fs.list(Path::new("/")).await.unwrap();

    let mut at = 0u64;
    while at < FIRST_SPAN + CHUNK {
        let got = fs.read_window(file, Some(at..at + CHUNK)).await.unwrap();
        assert_eq!(got.len() as u64, CHUNK, "at {at}");
        at += CHUNK;
    }
    let asked = mock.content_ranges();
    assert_eq!(asked.len(), 2, "one span, then its continuation: {asked:?}");
    // Windows land at 0, 3, 6 and 9 MiB. The one at 6 MiB reaches to 9 MiB, past the end
    // of an 8 MiB span it began inside — that is the straddle, and it is a walk.
    assert_eq!(
        asked[1],
        Some(format!("bytes={}-{}", 2 * CHUNK, 2 * CHUNK + READ_SPAN - 1)),
        "the second span begins where the read that missed began, not on a boundary"
    );
}

/// **The response, not the request, says where the bytes begin.**
///
/// Two shapes, both legitimate, and believing the request through either one serves the
/// wrong part of the file — silently, with the right length and the wrong content.
///
/// Microsoft documents the first: *"If the range can't be generated the Range header may
/// be ignored and an HTTP 200 response would be returned with the full contents of the
/// file."* The second is plain HTTP — a server may satisfy a range with a different one,
/// and `Content-Range` is then the only thing that knows which.
#[tokio::test]
async fn a_window_is_read_from_where_the_response_says_it_starts() {
    const REAL: usize = 1024 * 1024;
    const AT: u64 = 700_000;
    let body: Vec<u8> = (0..REAL).map(|i| (i % 251) as u8).collect();
    let want = &body[AT as usize..AT as usize + 4096];

    for (what, mode) in [
        ("200 with the whole file", RangeMode::Ignore),
        ("206 from an offset it chose", RangeMode::Shifted),
    ] {
        let mock = start_full(
            json!({"/": [file_row("whole.bin", "P1", REAL as u64)]}),
            HashMap::from([("P1".to_string(), body.clone())]),
            mode,
            false,
        )
        .await;
        let fs = mounted(&mock.config());
        let file = Path::new("/whole.bin");
        fs.list(Path::new("/")).await.unwrap();
        let got = fs.read_window(file, Some(AT..AT + 4096)).await.unwrap();
        assert_eq!(got.len(), 4096, "{what}");
        assert_eq!(
            got, want,
            "{what}: the window is the file's bytes at that offset"
        );
    }
}

/// A download URL is short-lived by design, so one that has expired costs one refetch and
/// not a failed read. Only one: a fresh URL that fails again is a fault, and retrying it
/// forever would hide it.
#[tokio::test]
async fn an_expired_download_url_is_refetched_once() {
    const REAL: usize = 4096;
    let body = vec![b'k'; REAL];
    let mock = start_full(
        json!({"/": [file_row("stale.bin", "P1", REAL as u64)]}),
        HashMap::from([("P1".to_string(), body.clone())]),
        RangeMode::Honour,
        // The URL the listing hands out has expired; the one an item fetch hands out has
        // not. That is the shape of the real failure.
        true,
    )
    .await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();

    mock.reset();
    let got = fs
        .read_window(Path::new("/stale.bin"), Some(0..REAL as u64))
        .await
        .unwrap();
    assert_eq!(got, body, "the read succeeded on the fresh url");
    assert!(
        mock.asked_for("/me/drive/root:/stale.bin?"),
        "by asking the service for the item again: {:?}",
        mock.targets()
    );
    assert_eq!(
        mock.content_ranges().len(),
        2,
        "one failed attempt and one that worked"
    );
}

/// What the listing cache does and does not remember.
///
/// A failure is not an answer: caching one would turn a single throttled request into five
/// minutes of an empty directory, which reads as "the folder is gone" rather than "ask
/// again". And what has aged out is dropped on the way in, so a traversal does not leave a
/// listing per folder it ever visited behind it.
#[tokio::test]
async fn the_listing_cache_forgets_what_it_should() {
    let mock = start(
        json!({
            "/": [folder_row("a", "D1"), folder_row("b", "D2")],
            "/a": [],
            "/b": [],
        }),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&mock.config());

    assert!(
        fs.list(Path::new("/nowhere")).await.is_err(),
        "the mock has no such folder"
    );
    assert_eq!(
        fs.listings_retained().await,
        0,
        "and a failure was not written down as one"
    );

    fs.list(Path::new("/a")).await.unwrap();
    fs.list(Path::new("/b")).await.unwrap();
    assert!(fs.listings_retained().await >= 2);

    fs.age_listings_for_test().await;
    fs.list(Path::new("/a")).await.unwrap();
    assert_eq!(
        fs.listings_retained().await,
        1,
        "the aged-out ones were swept on the way in"
    );
}

// ---------------------------------------------------------------------------
// Live
// ---------------------------------------------------------------------------

fn live_config() -> Option<OnedriveConfig> {
    Some(OnedriveConfig {
        client_id: std::env::var("ONEDRIVE_CLIENT_ID").ok()?,
        // A personal-account app registration is normally a public client, which has no
        // secret — absent here means absent from the form too, not a failure.
        client_secret: std::env::var("ONEDRIVE_CLIENT_SECRET").ok(),
        refresh_token: std::env::var("ONEDRIVE_REFRESH_TOKEN").ok()?,
        origins: Default::default(),
    })
}

/// Walk a real account and read what it lists.
///
///     set -a; . ./.env; set +a
///     cargo test -p cortex --features onedrive onedrive_live -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires ONEDRIVE_* env + network"]
async fn onedrive_live_tree_and_reads() {
    let Some(cfg) = live_config() else {
        eprintln!("set ONEDRIVE_CLIENT_ID / ONEDRIVE_REFRESH_TOKEN to run");
        return;
    };
    let fs = OnedriveFs::new(&cfg).unwrap();

    let root = fs.list(Path::new("/")).await.expect("root listing");
    eprintln!("  root: {} entries", root.len());
    assert!(!root.is_empty(), "an account with no files tests nothing");

    for e in root.iter().take(10) {
        let st = e.stat().expect("a listing row carries its stat");
        eprintln!(
            "  {:<40} {:>12} {}",
            e.name,
            st.size,
            if st.kind == DirentKind::Dir {
                "dir"
            } else {
                ""
            }
        );
        // Every entry's stat comes off the listing, so this states the real length and
        // never a placeholder.
        assert_eq!(
            fs.stat(&PathBuf::from("/").join(&e.name))
                .await
                .unwrap()
                .size,
            st.size,
            "stat and the listing agree for {}",
            e.name
        );
    }
}

/// Read a real file by ranges and confirm the windows line up with the whole.
#[tokio::test]
#[ignore = "requires ONEDRIVE_* env + network"]
async fn onedrive_live_reads_by_range() {
    let Some(cfg) = live_config() else {
        eprintln!("set ONEDRIVE_CLIENT_ID / ONEDRIVE_REFRESH_TOKEN to run");
        return;
    };
    let fs = OnedriveFs::new(&cfg).unwrap();
    let root = fs.list(Path::new("/")).await.expect("root listing");
    let Some(file) = root
        .iter()
        .find(|e| e.kind == DirentKind::File && e.stat().is_some_and(|s| s.size > 4096))
    else {
        eprintln!("  no file over 4 KiB at the root; nothing to range-read");
        return;
    };
    let path = PathBuf::from("/").join(&file.name);
    let size = file.stat().unwrap().size;
    eprintln!("  {} is {} bytes", file.name, size);

    let head = fs.read_window(&path, Some(0..4096)).await.expect("head");
    assert_eq!(head.len(), 4096);
    let mid = fs.read_window(&path, Some(2048..4096)).await.expect("mid");
    assert_eq!(
        mid,
        &head[2048..4096],
        "a window is the file's bytes at that offset"
    );

    // Past the end is an ordinary end rather than an error.
    let past = fs
        .read_window(&path, Some(size..size + 4096))
        .await
        .expect("past the end");
    assert!(past.is_empty());
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mounted(cfg: &OnedriveConfig) -> OnedriveFs {
    OnedriveFs::new(cfg).unwrap()
}

/// A file row as Graph returns one, carrying its own download URL the way a `$select`ed
/// listing does.
fn file_row(name: &str, id: &str, size: u64) -> Value {
    json!({
        "id": id,
        "name": name,
        "size": size,
        "file": {"mimeType": "application/octet-stream"},
        "lastModifiedDateTime": "2026-01-30T09:00:00Z",
        "createdDateTime": "2025-11-01T12:00:00Z",
        "cTag": format!("ctag-{name}"),
        "eTag": format!("etag-{name}"),
        "@microsoft.graph.downloadUrl": format!("{{HOST}}/content/{id}?listing=1"),
    })
}

fn folder_row(name: &str, id: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        // A folder states a size too — the sum of what it contains.
        "size": 987_654,
        "folder": {"childCount": 0},
        "lastModifiedDateTime": "2026-01-30T09:00:00Z",
        "createdDateTime": "2025-11-01T12:00:00Z",
        "eTag": format!("etag-{name}"),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum RangeMode {
    /// 206 with `Content-Range`, 416 past the end — what Graph's CDN does.
    Honour,
    /// 200 with the whole body, ignoring the header — which Microsoft documents as a
    /// legitimate answer when the range "can't be generated".
    Ignore,
    /// 206, but starting earlier than asked. A server may satisfy a range with a
    /// different one and say so in `Content-Range`; RFC 9110 lets it, and the header is
    /// then the only thing that knows where the bytes begin.
    Shifted,
}

#[derive(Clone)]
struct Seen {
    target: String,
    range: Option<String>,
}

struct Mock {
    addr: String,
    seen: Arc<StdMutex<Vec<Seen>>>,
    body_bytes: Arc<StdMutex<u64>>,
}

impl Mock {
    fn config(&self) -> OnedriveConfig {
        OnedriveConfig {
            client_id: "cid".into(),
            client_secret: Some("cs".into()),
            refresh_token: "rt".into(),
            origins: OnedriveOrigins::behind(&self.addr),
        }
    }

    /// Every request target, in order.
    fn targets(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.target.clone())
            .filter(|t| !t.contains("/oauth2/"))
            .collect()
    }

    /// `Range` headers of the content requests, in order. `None` = the whole object.
    fn content_ranges(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.target.contains("/content/"))
            .map(|s| s.range.clone())
            .collect()
    }

    fn asked_for(&self, needle: &str) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.target.contains(needle))
    }

    fn bytes_sent(&self) -> u64 {
        *self.body_bytes.lock().unwrap()
    }

    fn reset(&self) {
        self.seen.lock().unwrap().clear();
        *self.body_bytes.lock().unwrap() = 0;
    }
}

/// Serve a token, a tree of listings, and content with working ranges.
///
/// `tree` maps a mount path (`"/"`, `"/Documents"`) to the rows that folder lists.
async fn start(tree: Value, blobs: HashMap<String, Vec<u8>>) -> Mock {
    start_full(tree, blobs, RangeMode::Honour, false).await
}

/// `stale_listing_urls` makes the URL a *listing* hands out answer 401, while the one an
/// item fetch hands out works — which is what an expired preauthenticated URL looks like.
async fn start_full(
    tree: Value,
    blobs: HashMap<String, Vec<u8>>,
    range_mode: RangeMode,
    stale_listing_urls: bool,
) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let body_bytes = Arc::new(StdMutex::new(0u64));
    let (log, written, blobs) = (seen.clone(), body_bytes.clone(), Arc::new(blobs));
    let tree = Arc::new(tree);
    let host = addr.clone();

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (log, written, blobs, tree, host) = (
                log.clone(),
                written.clone(),
                blobs.clone(),
                tree.clone(),
                host.clone(),
            );
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let start_line = lines.next().unwrap_or("").to_string();
                let mut parts = start_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();
                // Drain an announced body (the token POST) so the client is not left
                // writing into a socket nobody reads.
                if let Some(cl) =
                    header(&headers, "content-length").and_then(|v| v.parse::<usize>().ok())
                {
                    while buf.len() < head_end + cl {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                }
                let range = header(&headers, "range").map(str::to_string);
                log.lock().unwrap().push(Seen {
                    target: target.clone(),
                    range: range.clone(),
                });

                let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
                let reply = |status: u16, body: Vec<u8>, extra: Option<String>| {
                    let len = body.len();
                    let mut out = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {len}\r\nConnection: close\r\n"
                    );
                    if let Some(e) = extra {
                        out.push_str(&e);
                        out.push_str("\r\n");
                    }
                    out.push_str("\r\n");
                    let mut bytes = out.into_bytes();
                    bytes.extend_from_slice(&body);
                    (bytes, len)
                };

                let (out, body_len) = if method == "POST" && path.contains("/oauth2/") {
                    reply(
                        200,
                        json!({"access_token": "at", "expires_in": 3600})
                            .to_string()
                            .into_bytes(),
                        None,
                    )
                } else if let Some(id) = path.strip_prefix("/content/") {
                    let fresh = query.contains("fresh=1");
                    if stale_listing_urls && !fresh {
                        // What an expired preauthenticated URL answers.
                        reply(401, br#"{"error":"expired"}"#.to_vec(), None)
                    } else {
                        let blob = blobs.get(id).cloned().unwrap_or_default();
                        serve_content(blob, range.as_deref(), range_mode)
                    }
                } else if let Some(folder) = graph_children_path(path) {
                    match tree.get(&folder).and_then(|v| v.as_array()) {
                        Some(rows) => {
                            let rows: Vec<Value> =
                                rows.iter().map(|r| with_host(r, &host, false)).collect();
                            reply(200, json!({"value": rows}).to_string().into_bytes(), None)
                        }
                        None => reply(404, br#"{"error":"itemNotFound"}"#.to_vec(), None),
                    }
                } else if let Some(item) = graph_item_path(path) {
                    match find_item(&tree, &item) {
                        // The item fetch hands out a URL marked fresh, so the refetch
                        // path can be told apart from the listing's.
                        Some(row) => reply(
                            200,
                            with_host(&row, &host, true).to_string().into_bytes(),
                            None,
                        ),
                        None => reply(404, br#"{"error":"itemNotFound"}"#.to_vec(), None),
                    }
                } else {
                    reply(404, br#"{"error":"no route"}"#.to_vec(), None)
                };
                if sock.write_all(&out).await.is_ok() {
                    *written.lock().unwrap() += body_len as u64;
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    Mock {
        addr,
        seen,
        body_bytes,
    }
}

/// Content, with or without a working `Range`.
fn serve_content(blob: Vec<u8>, range: Option<&str>, mode: RangeMode) -> (Vec<u8>, usize) {
    let head = |status: u16, len: usize, extra: Option<String>| {
        let mut out = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/octet-stream\r\n\
             Content-Length: {len}\r\nConnection: close\r\n"
        );
        if let Some(e) = extra {
            out.push_str(&e);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    };
    match (mode, range.and_then(parse_range)) {
        (RangeMode::Shifted, Some((start, end))) => {
            // Answer from a bit earlier than asked, and state it.
            let start = start.saturating_sub(1024);
            let last = end
                .unwrap_or(blob.len() as u64 - 1)
                .min(blob.len() as u64 - 1);
            let window = blob[start as usize..=last as usize].to_vec();
            let len = window.len();
            let cr = format!("Content-Range: bytes {start}-{last}/{}", blob.len());
            let mut out = head(206, len, Some(cr));
            out.extend_from_slice(&window);
            (out, len)
        }
        (RangeMode::Ignore, Some(_)) | (_, None) => {
            let len = blob.len();
            let mut out = head(200, len, None);
            out.extend_from_slice(&blob);
            (out, len)
        }
        (RangeMode::Honour, Some((start, end))) => {
            if start >= blob.len() as u64 {
                return (head(416, 0, None), 0);
            }
            let last = end
                .unwrap_or(blob.len() as u64 - 1)
                .min(blob.len() as u64 - 1);
            let window = blob[start as usize..=last as usize].to_vec();
            let len = window.len();
            let cr = format!("Content-Range: bytes {start}-{last}/{}", blob.len());
            let mut out = head(206, len, Some(cr));
            out.extend_from_slice(&window);
            (out, len)
        }
    }
}

/// `/graph/v1.0/me/drive/root/children` or `/graph/v1.0/me/drive/root:/A/B:/children`
/// into the mount path the tree is keyed by.
fn graph_children_path(path: &str) -> Option<String> {
    let rest = path.split_once("/me/drive/root")?.1;
    if rest == "/children" {
        return Some("/".to_string());
    }
    let inner = rest.strip_prefix(":/")?.strip_suffix(":/children")?;
    Some(format!("/{}", decode(inner)))
}

/// `/graph/v1.0/me/drive/root:/A/B` into the same.
fn graph_item_path(path: &str) -> Option<String> {
    let rest = path.split_once("/me/drive/root")?.1;
    if rest.is_empty() {
        return Some("/".to_string());
    }
    let inner = rest.strip_prefix(":/")?;
    (!inner.contains(':')).then(|| format!("/{}", decode(inner)))
}

/// The row a mount path names, by walking the tree the way the store does.
fn find_item(tree: &Value, path: &str) -> Option<Value> {
    let (parent, name) = split_last(path);
    tree.get(&parent)?
        .as_array()?
        .iter()
        .find(|r| r.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
        .cloned()
}

/// Point a row's download URL at this mock, marking it fresh when an item fetch hands it
/// out rather than a listing.
fn with_host(row: &Value, host: &str, fresh: bool) -> Value {
    let mut row = row.clone();
    if let Some(u) = row
        .get("@microsoft.graph.downloadUrl")
        .and_then(|u| u.as_str())
    {
        let mut u = u.replace("{HOST}", host);
        if fresh {
            u.push_str("&fresh=1");
        }
        row.as_object_mut()
            .unwrap()
            .insert("@microsoft.graph.downloadUrl".into(), json!(u));
    }
    row
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn parse_range(h: &str) -> Option<(u64, Option<u64>)> {
    let spec = h.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start = a.trim().parse().ok()?;
    let end = b.trim();
    Some((start, (!end.is_empty()).then(|| end.parse().ok()).flatten()))
}
