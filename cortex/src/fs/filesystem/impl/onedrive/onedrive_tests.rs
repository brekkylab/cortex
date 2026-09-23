use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::accessor::encode_path;
use super::super::{OnedriveConfig, OnedriveOrigins};
use super::*;

// Each test says what it pins. Why the behaviour is what it is lives once, on the code it
// points to.

// ---------------------------------------------------------------------------
// Pure, no I/O
// ---------------------------------------------------------------------------

/// A listing row becomes the entry the mount shows, or nothing. See [`child_from_item`].
#[test]
fn a_row_becomes_the_entry_it_should() {
    let mut notebook = folder_row("Work Notes", "N1");
    notebook["package"] = json!({"type": "oneNote"});
    assert!(
        child_from_item(&notebook).is_none(),
        "a package, despite its folder facet"
    );

    let mut neither = json!({"id": "X1", "name": "mystery", "size": 10});
    assert!(child_from_item(&neither).is_none(), "no file, no folder");
    neither["file"] = json!({});
    assert!(
        child_from_item(&neither).is_some(),
        "a file facet is enough"
    );

    let mut idless = file_row("report.docx", "F1", 1234);
    idless.as_object_mut().unwrap().remove("id");
    assert!(child_from_item(&idless).is_none(), "no id, no entry");

    let file = child_from_item(&file_row("report.docx", "F1", 1234)).unwrap();
    assert_eq!(
        (file.size, file.etag.as_deref()),
        (1234, Some("ctag-report.docx"))
    );
    let folder = child_from_item(&folder_row("Documents", "D1")).unwrap();
    assert_eq!(
        folder.size, 0,
        "a folder's size is what it holds, not a length"
    );
}

/// A name is one path segment and a path never walks `..`. See [`sanitize_name`], [`vpath`].
#[test]
fn nothing_addresses_what_it_does_not_name() {
    let name = |n: &str| child_from_item(&file_row(n, "E", 1)).unwrap().name;
    assert!(!name("../../etc/passwd").contains('/'));
    assert_eq!(
        (name(".."), name("   ")),
        ("untitled".into(), "untitled".into())
    );
    assert_eq!(vpath(Path::new("/a//b/")).unwrap(), "/a/b");
    assert_eq!(vpath(Path::new("/a/./b")).unwrap(), "/a/b");
    assert!(vpath(Path::new("/a/../b")).is_err());
}

// ---------------------------------------------------------------------------
// Mock-backed
// ---------------------------------------------------------------------------

/// What the mock's `429` asks to wait, in seconds: past `MAX_RETRY_AFTER`, so refused.
const THROTTLED_FOR: u64 = 900;
/// What its `4290` asks instead: within the budget, so actually slept. Small for that reason.
const THROTTLED_SHORT: u64 = 1;

/// One listing answers resolution and every attribute under it, at any depth and under
/// either spelling of a name. See [`OnedriveFs::resolve`].
#[tokio::test]
async fn one_listing_answers_the_whole_directory() {
    let composed = "보고서.docx";
    let (mock, fs) = mount(
        json!({
            "/": [folder_row("Documents", "D1")],
            "/Documents": [folder_row("2026", "D2")],
            "/Documents/2026": [file_row(composed, "F9", 1234), folder_row("drafts", "D3")],
        }),
        HashMap::new(),
    )
    .await;
    let dir = PathBuf::from("/Documents/2026");

    let st = fs.stat(&dir.join(composed)).await.unwrap();
    assert_eq!(
        (st.size, st.etag.as_deref()),
        (1234, Some("ctag-보고서.docx"))
    );
    assert!(st.mtime.is_some());
    assert_eq!(
        mock.targets().len(),
        1,
        "three deep, one listing: {:?}",
        mock.targets()
    );
    assert!(
        mock.asked_for("root:/Documents/2026:/children"),
        "by path, not walked to"
    );

    // Everything else in that folder is then free, under the spelling macOS hands over too.
    mock.reset();
    let decomposed: String = composed.nfd().collect();
    assert_ne!(composed, decomposed, "two spellings");
    assert_eq!(fs.stat(&dir.join(&decomposed)).await.unwrap().size, 1234);
    let listed = fs.list(&dir).await.unwrap();
    let drafts = listed
        .iter()
        .find(|d| d.name == "drafts")
        .and_then(|d| d.stat())
        .unwrap();
    assert_eq!(
        (listed.len(), drafts.kind, drafts.size),
        (2, DirentKind::Dir, 0)
    );
    assert!(
        mock.targets().is_empty(),
        "none of that cost a request: {:?}",
        mock.targets()
    );
}

/// A path resolves under the spelling the kernel hands over, including one whose segments are
/// stored in different forms. See [`OnedriveFs::list_unspellable`].
#[tokio::test]
async fn a_path_resolves_under_the_spelling_the_kernel_hands_over() {
    // Stored composed: one fallback reaches it. Composed over decomposed: only the walk does.
    let simple: String = "문서".nfc().collect();
    let parent: String = "상위".nfc().collect();
    let child: String = "하위".nfd().collect();
    assert_ne!(child, "하위".nfc().collect::<String>(), "two spellings");
    let mut tree = json!({"/": [folder_row(&simple, "D1"), folder_row(&parent, "D2")]});
    tree[format!("/{simple}")] = json!([file_row("note.txt", "F1", 5)]);
    tree[format!("/{parent}")] = json!([folder_row(&child, "D3")]);
    tree[format!("/{parent}/{child}")] = json!([file_row("deep.txt", "F3", 5)]);
    let (mock, fs) = mount(tree, HashMap::from([("F1".to_string(), b"hello".to_vec())])).await;
    let names = |d: Vec<Dirent>| d.into_iter().map(|d| d.name).collect::<Vec<_>>();

    // One segment asked decomposed: as given, then composed, and it reads through that.
    let asked: String = format!("/{simple}").nfd().collect();
    assert_eq!(
        names(fs.list(Path::new(&asked)).await.unwrap()),
        ["note.txt"]
    );
    assert_eq!(by_path(&mock), 2, "{:?}", mock.targets());
    assert!(
        names(fs.list(Path::new("/")).await.unwrap()).contains(&simple),
        "its own spelling"
    );
    assert_eq!(
        read(&fs, &format!("{asked}/note.txt"), 0..5).await,
        b"hello"
    );

    // Two segments in different forms: resolved through the parent, listed by id.
    mock.reset();
    let asked: String = format!("/{parent}/{child}").nfd().collect();
    assert_eq!(
        names(fs.list(Path::new(&asked)).await.unwrap()),
        ["deep.txt"]
    );
    assert!(
        mock.asked_for("/me/drive/items/D3/children"),
        "{:?}",
        mock.targets()
    );
}

/// A backend failure is not an absence, whatever digits its body carries. See
/// [`is_not_found`].
#[tokio::test]
async fn a_backend_failure_is_not_an_absence() {
    // `/denied` answers 403 with a correlation id that happens to contain `404`.
    let (_mock, fs) = mount(
        json!({"/": [folder_row("denied", "D1"), folder_row("gone", "D2")], "/denied": 403}),
        HashMap::new(),
    )
    .await;
    let kind = async |p: &str| fs.list(Path::new(p)).await.err().map(|e| e.kind());
    assert!(!matches!(
        kind("/denied").await,
        None | Some(io::ErrorKind::NotFound)
    ));
    assert_eq!(kind("/gone").await, Some(io::ErrorKind::NotFound));
}

/// A throttle is waited out as asked, bounded across the whole ladder, and then respected:
/// nothing is sent until the service said it may be asked again. See `MAX_RETRY_AFTER`.
#[tokio::test]
async fn a_throttle_is_waited_out_as_asked_bounded_and_then_respected() {
    // More than a call may spend waiting: reported at once, nothing slept.
    let (mock, fs) = mount(
        json!({"/": [folder_row("busy", "D1")], "/busy": 429}),
        HashMap::new(),
    )
    .await;
    let started = Instant::now();
    let e = fs.list(Path::new("/busy")).await.err().unwrap();
    assert_ne!(e.kind(), io::ErrorKind::NotFound, "not an absence: {e}");
    assert_eq!(by_path(&mock), 1, "asked once: {:?}", mock.targets());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "not slept through"
    );

    // And then nothing is sent: a give-up that keeps asking is worse than waiting.
    mock.reset();
    for _ in 0..19 {
        assert!(fs.list(Path::new("/busy")).await.is_err());
    }
    assert_eq!(by_path(&mock), 0, "{:?}", mock.targets());

    // A wait that fits the budget is actually taken, and the ladder stops inside it.
    let (_mock, fs) = mount(
        json!({"/": [folder_row("slow", "D1")], "/slow": 4290}),
        HashMap::new(),
    )
    .await;
    let started = Instant::now();
    assert!(fs.list(Path::new("/slow")).await.is_err());
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(THROTTLED_SHORT),
        "slept as asked: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "within the budget: {waited:?}"
    );
}

/// A first read pays [`FIRST_SPAN`], a walk pays [`READ_SPAN`] from where it carried on, and a
/// short read is the end of the file. See [`OnedriveFs::span`].
#[tokio::test]
async fn a_walk_pays_a_span_and_a_head_read_pays_less() {
    const REAL: u64 = 80 * 1024 * 1024;
    const CHUNK: u64 = 64 * 1024;
    let (mock, fs) = mount(
        json!({"/": [file_row("big.bin", "P1", REAL)]}),
        HashMap::from([("P1".to_string(), vec![b'z'; REAL as usize])]),
    )
    .await;
    fs.list(Path::new("/")).await.unwrap();
    let range = |at: u64, len: u64| Some(format!("bytes={at}-{}", at + len - 1));

    // Eight windows out of one span, and it is the smaller one.
    mock.reset();
    for i in 0..8 {
        assert_eq!(
            read(&fs, "/big.bin", i * CHUNK..(i + 1) * CHUNK)
                .await
                .len() as u64,
            CHUNK
        );
    }
    assert_eq!(mock.content_ranges(), [range(0, FIRST_SPAN)]);

    // Carrying on pays the bigger one from where the read began: from inside the spent span,
    // then from exactly its end.
    let mut at = FIRST_SPAN - CHUNK / 2;
    for _ in 0..2 {
        mock.reset();
        assert_eq!(
            read(&fs, "/big.bin", at..at + CHUNK).await.len() as u64,
            CHUNK
        );
        assert_eq!(mock.content_ranges(), [range(at, READ_SPAN)], "at {at}");
        at += READ_SPAN;
    }

    // Short is the end, and wholly past it is empty. Neither is an error.
    let tail = read(&fs, "/big.bin", REAL - CHUNK / 2..REAL + CHUNK / 2).await;
    assert_eq!(tail.len() as u64, CHUNK / 2);
    assert!(
        read(&fs, "/big.bin", REAL + CHUNK..REAL + 2 * CHUNK)
            .await
            .is_empty()
    );
}

/// A span nobody came back to stops dividing the budget after [`ACTIVE`] and stops being kept
/// after [`DIR_TTL`]. See [`OnedriveFs::held`].
#[tokio::test]
async fn spans_left_behind_stop_counting_and_stop_being_kept() {
    const SMALL: u64 = 4 * 1024 * 1024;
    const BIG: u64 = 32 * 1024 * 1024;
    const W: u64 = 32 * 1024;
    let mut rows = vec![file_row("big.bin", "BIG", BIG)];
    let mut blobs = HashMap::from([("BIG".to_string(), vec![b'z'; BIG as usize])]);
    for i in 0..5 {
        rows.push(file_row(&format!("s{i}.bin"), &format!("S{i}"), SMALL));
        blobs.insert(format!("S{i}"), vec![b's'; SMALL as usize]);
    }
    let (mock, fs) = mount(json!({"/": rows}), blobs).await;
    fs.list(Path::new("/")).await.unwrap();

    // Touched once each and abandoned, the way a traversal leaves them.
    for i in 0..5 {
        read(&fs, &format!("/s{i}.bin"), 0..W).await;
    }
    fs.age_spans_for_test(ACTIVE + Duration::from_secs(1)).await;

    // The only reader gets the whole budget: a first span, then the rest of the file.
    mock.reset();
    for at in (0..BIG).step_by(W as usize) {
        read(&fs, "/big.bin", at..(at + W).min(BIG)).await;
    }
    assert_eq!(
        mock.content_ranges().len(),
        2,
        "{:?}",
        mock.content_ranges()
    );
    assert_eq!(mock.bytes_sent(), BIG, "nothing fetched twice");

    // Past the TTL they are swept on the way in.
    fs.age_spans_for_test(DIR_TTL + Duration::from_secs(1))
        .await;
    read(&fs, "/s0.bin", 0..W).await;
    for p in ["/big.bin", "/s1.bin", "/s2.bin", "/s3.bin", "/s4.bin"] {
        assert!(fs.held_bytes(p).await.is_none(), "{p} swept");
    }
}

/// Files read in turn each keep their span, so nothing is fetched twice. One slot could not;
/// see [`OnedriveFs::held`]. Each file is larger than a span, so a constant [`READ_SPAN`]
/// would overrun the budget and the division is measured.
#[tokio::test]
async fn interleaved_files_each_keep_a_span() {
    const REAL: u64 = 32 * 1024 * 1024;
    // 512 KiB chunks per file, as `grep -r` alternated over a local mount. Only the chunk
    // matters, so the window is widened from 32 KiB to keep the walk cheap.
    const W: u64 = 256 * 1024;
    const CHUNK: u64 = 2 * W;
    let (mock, fs) = mount(
        json!({"/": (0..3).map(|i| file_row(&format!("f{i}.bin"), &format!("P{i}"), REAL))
            .collect::<Vec<_>>()}),
        (0..3)
            .map(|i| (format!("P{i}"), vec![b'z'; REAL as usize]))
            .collect(),
    )
    .await;
    fs.list(Path::new("/")).await.unwrap();
    mock.reset();

    for at in (0..REAL).step_by(CHUNK as usize) {
        for i in 0..3 {
            for o in (at..(at + CHUNK).min(REAL)).step_by(W as usize) {
                let got = read(&fs, &format!("/f{i}.bin"), o..(o + W).min(REAL)).await;
                assert_eq!(got.len() as u64, (REAL - o).min(W), "f{i} at {o}");
            }
        }
    }

    // Nothing fetched twice, up to under a window of overlap per span boundary. No ceiling on
    // the span count: reverting the share sizing fetches *fewer*, wastefully larger, spans.
    let spans = mock.content_ranges().len() as u64;
    let waste = mock.bytes_sent().saturating_sub(3 * REAL);
    assert!(waste < spans * W, "{waste} wasted over {spans} spans");
    for i in 0..3 {
        assert!(
            fs.held_bytes(&format!("/f{i}.bin")).await.is_some(),
            "f{i} still held"
        );
    }
}

/// The response, not the request, says where the bytes begin: a range answered `200` with the
/// whole file is sliced from zero. See `OnedriveAccessor::download`.
#[tokio::test]
async fn a_window_is_read_from_where_the_response_says_it_starts() {
    const AT: usize = 700_000;
    let body: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mock = start_full(
        json!({"/": [file_row("whole.bin", "P1", body.len() as u64)]}),
        HashMap::from([("P1".to_string(), body.clone())]),
        RangeMode::Ignore,
        false,
    )
    .await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();
    let got = read(&fs, "/whole.bin", AT as u64..AT as u64 + 4096).await;
    assert_eq!(got, body[AT..AT + 4096]);
}

/// An expired download URL costs exactly one refetch. A fresh URL that works is read; one that
/// fails too is a fault, not an absence, and its error does not carry the URL, whose query
/// string is the grant. `dead.bin` points at a closed port: the transport path. See
/// [`OnedriveFs::span`].
#[tokio::test]
async fn an_expired_download_url_is_refetched_once_and_then_given_up() {
    const SENTINEL: &str = "SENTINEL-GRANT-DO-NOT-LOG";
    let body = vec![b'k'; 4096];
    let mut dead = file_row("dead.bin", "P2", 4096);
    dead[DOWNLOAD_URL_KEY] = json!(format!("http://127.0.0.1:1/blob?tempauth={SENTINEL}"));
    // Listings hand out expired URLs; an item fetch hands out a fresh one.
    let mock = start_full(
        json!({"/": [file_row("stale.bin", "P1", 4096), dead]}),
        HashMap::from([("P1".to_string(), body.clone())]),
        RangeMode::Honour,
        true,
    )
    .await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();

    mock.reset();
    assert_eq!(read(&fs, "/stale.bin", 0..4096).await, body);
    assert!(
        mock.asked_for("/me/drive/items/P1?"),
        "refetched by id: {:?}",
        mock.targets()
    );
    assert_eq!(mock.content_ranges().len(), 2, "one failure, one success");

    let e = fs
        .read_window(Path::new("/dead.bin"), Some(0..4096))
        .await
        .err()
        .unwrap();
    assert_ne!(e.kind(), io::ErrorKind::NotFound, "the item answered: {e}");
    assert!(
        !e.to_string().contains(SENTINEL),
        "the grant reached the error: {e}"
    );
}

/// The listing cache writes down no failure, looks nowhere else for a name with one spelling,
/// and sweeps what has aged out on the way in. See [`OnedriveFs::list_dir`].
#[tokio::test]
async fn the_listing_cache_forgets_what_it_should() {
    let (mock, fs) = mount(
        json!({"/": [folder_row("a", "D1"), folder_row("b", "D2")], "/a": [], "/b": []}),
        HashMap::new(),
    )
    .await;
    // One request for a miss, and not cached: macOS probes `.DS_Store` in every directory.
    assert!(fs.list(Path::new("/nowhere")).await.is_err());
    assert_eq!(mock.targets().len(), 1, "{:?}", mock.targets());
    assert_eq!(fs.listings_retained().await, 0);

    fs.list(Path::new("/a")).await.unwrap();
    fs.list(Path::new("/b")).await.unwrap();
    assert!(fs.listings_retained().await >= 2);
    fs.age_listings_for_test().await;
    fs.list(Path::new("/a")).await.unwrap();
    assert_eq!(
        fs.listings_retained().await,
        1,
        "aged out, swept on the way in"
    );
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn mounted(cfg: &OnedriveConfig) -> OnedriveFs {
    OnedriveFs::new(cfg).unwrap()
}

/// A mock serving `tree` and `blobs`, and a store pointed at it.
async fn mount(tree: Value, blobs: HashMap<String, Vec<u8>>) -> (Mock, OnedriveFs) {
    let mock = start(tree, blobs).await;
    let fs = mounted(&mock.config());
    (mock, fs)
}

/// `range` of the file at `path`, which must read.
async fn read(fs: &OnedriveFs, path: &str, range: Range<u64>) -> Vec<u8> {
    fs.read_window(Path::new(path), Some(range)).await.unwrap()
}

/// How many listings were addressed by path (`root:/…:/children`) rather than by id.
fn by_path(mock: &Mock) -> usize {
    mock.targets()
        .iter()
        .filter(|t| t.contains(":/children"))
        .count()
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
        DOWNLOAD_URL_KEY: format!("{{HOST}}/content/{id}?listing=1"),
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
                let reply = |status: u16, body: Vec<u8>| {
                    let len = body.len();
                    let mut out = http_head(status, "application/json", len, None);
                    out.extend_from_slice(&body);
                    (out, len)
                };

                let (out, body_len) = if method == "POST" && path.contains("/oauth2/") {
                    reply(
                        200,
                        json!({"access_token": "at", "expires_in": 3600})
                            .to_string()
                            .into_bytes(),
                    )
                } else if let Some(id) = path.strip_prefix("/content/") {
                    let fresh = query.contains("fresh=1");
                    if stale_listing_urls && !fresh {
                        // What an expired preauthenticated URL answers.
                        reply(401, br#"{"error":"expired"}"#.to_vec())
                    } else {
                        // A blob the fixture does not carry is a 404, not an empty file.
                        // Serving empty bytes would make a download failure unreachable
                        // from a test, which is the shape the read path classifies on.
                        match blobs.get(id) {
                            Some(blob) => serve_content(blob.clone(), range.as_deref(), range_mode),
                            None => reply(404, br#"{"error":"itemNotFound"}"#.to_vec()),
                        }
                    }
                } else if let Some(folder) = graph_children_path(&tree, path) {
                    match tree.get(&folder) {
                        Some(Value::Array(rows)) => {
                            let rows: Vec<Value> = rows
                                .iter()
                                .map(|r| with_host(r, &host, false, query))
                                .collect();
                            reply(200, json!({"value": rows}).to_string().into_bytes())
                        }
                        // A number in place of a folder's rows is the status it answers
                        // with, so a test can ask for a failure that is *not* an absence.
                        // The body is the shape Graph sends: an inner error carrying a
                        // correlation id, which is hex and so sometimes spells `404` while
                        // meaning nothing by it. A `429` carries `Retry-After` as the
                        // service does, stating a wait far past what a mount may sleep.
                        Some(Value::Number(n)) => {
                            let code = n.as_u64().unwrap_or(500) as u16;
                            let body = br#"{"error":{"code":"accessDenied","innerError":
                                {"request-id":"74bd5c8a-0083-404f-b5b2-9602fa4d4ca3"}}}"#
                                .to_vec();
                            // 429 asks for a wait far past what one call may spend;
                            // 4290 is the same throttle asking for one that fits.
                            let (code, extra) = match code {
                                429 => (429, Some(format!("Retry-After: {THROTTLED_FOR}"))),
                                4290 => (429, Some(format!("Retry-After: {THROTTLED_SHORT}"))),
                                other => (other, None),
                            };
                            let mut out = http_head(code, "application/json", body.len(), extra);
                            let len = body.len();
                            out.extend_from_slice(&body);
                            (out, len)
                        }
                        _ => reply(404, br#"{"error":"itemNotFound"}"#.to_vec()),
                    }
                } else if let Some(id) = graph_children_of_id(path) {
                    match folder_path_of_id(&tree, id).and_then(|k| tree.get(&k)) {
                        Some(Value::Array(rows)) => {
                            let rows: Vec<Value> = rows
                                .iter()
                                .map(|r| with_host(r, &host, false, query))
                                .collect();
                            reply(200, json!({"value": rows}).to_string().into_bytes())
                        }
                        _ => reply(404, br#"{"error":"itemNotFound"}"#.to_vec()),
                    }
                } else if let Some(id) = graph_item_id(path) {
                    match find_item_by_id(&tree, id) {
                        // The item fetch hands out a URL marked fresh, so the refetch
                        // path can be told apart from the listing's.
                        Some(row) => reply(
                            200,
                            with_host(&row, &host, true, query).to_string().into_bytes(),
                        ),
                        None => reply(404, br#"{"error":"itemNotFound"}"#.to_vec()),
                    }
                } else {
                    reply(404, br#"{"error":"no route"}"#.to_vec())
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

/// One HTTP response head. `extra`, when given, is a whole header line.
fn http_head(status: u16, ctype: &str, len: usize, extra: Option<String>) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\n\
         Content-Length: {len}\r\nConnection: close\r\n"
    );
    if let Some(e) = extra {
        out.push_str(&e);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Content, answering a `Range` the way `mode` says this origin would.
fn serve_content(blob: Vec<u8>, range: Option<&str>, mode: RangeMode) -> (Vec<u8>, usize) {
    let body = |status: u16, bytes: &[u8], extra: Option<String>| {
        let mut out = http_head(status, "application/octet-stream", bytes.len(), extra);
        out.extend_from_slice(bytes);
        (out, bytes.len())
    };
    let asked = range.and_then(parse_range);
    // No range asked, or one this origin ignores — the whole file, which Microsoft
    // documents as a legitimate answer to a range it cannot generate.
    let Some((from, to)) = asked.filter(|_| mode != RangeMode::Ignore) else {
        return body(200, &blob, None);
    };
    if mode == RangeMode::Honour && from >= blob.len() as u64 {
        return body(416, &[], None);
    }
    let end = blob.len() as u64 - 1;
    let last = to.unwrap_or(end).min(end);
    let window = &blob[from as usize..=last as usize];
    let cr = format!("Content-Range: bytes {from}-{last}/{}", blob.len());
    body(206, window, Some(cr))
}

/// `/graph/v1.0/me/drive/root/children` or `/graph/v1.0/me/drive/root:/A/B:/children`
/// into the mount path the tree is keyed by.
fn graph_children_path(tree: &Value, path: &str) -> Option<String> {
    let rest = path.split_once("/me/drive/root")?.1;
    if rest == "/children" {
        return Some("/".to_string());
    }
    let inner = rest.strip_prefix(":/")?.strip_suffix(":/children")?;
    tree.as_object()?
        .keys()
        .find(|k| encode_path(k) == inner)
        .cloned()
}

/// The encoded id in `/graph/v1.0/me/drive/items/{id}`, which addresses one item rather
/// than a folder's children.
fn graph_item_id(path: &str) -> Option<&str> {
    let rest = path.split_once("/me/drive/items/")?.1;
    (!rest.contains('/')).then_some(rest)
}

/// The encoded id in `/graph/v1.0/me/drive/items/{id}/children`, which addresses a
/// folder's children without naming the folder.
fn graph_children_of_id(path: &str) -> Option<&str> {
    path.split_once("/me/drive/items/")?
        .1
        .strip_suffix("/children")
}

/// The tree key of the folder whose row carries `encoded`.
///
/// Found through the row rather than guessed from the id, so a folder is located by the
/// same thing the service would use: the parent that lists it, plus its own name. Two
/// folders of one name under different parents stay distinct.
fn folder_path_of_id(tree: &Value, encoded: &str) -> Option<String> {
    for (parent, rows) in tree.as_object()? {
        for row in rows.as_array().into_iter().flatten() {
            if row.get("folder").is_none() {
                continue;
            }
            let id = row.get("id")?.as_str()?;
            if encode_path(id) != encoded {
                continue;
            }
            let name = row.get("name")?.as_str()?;
            let sep = if parent.ends_with('/') { "" } else { "/" };
            return Some(format!("{parent}{sep}{name}"));
        }
    }
    None
}

/// The row whose id encodes to `encoded`.
///
/// Encoding the fixtures forwards rather than decoding the request: the accessor's own
/// encoder is the definition of what an id becomes on the wire, so a mock that compares
/// against it cannot disagree with the thing under test about what was asked for. A
/// second, hand-written decoder here could.
fn find_item_by_id(tree: &Value, encoded: &str) -> Option<Value> {
    tree.as_object()?
        .values()
        .filter_map(|rows| rows.as_array())
        .flatten()
        .find(|row| {
            row.get("id")
                .and_then(|v| v.as_str())
                .map(encode_path)
                .as_deref()
                == Some(encoded)
        })
        .cloned()
}

/// Point a row's download URL at this mock, marking it fresh when an item fetch hands it
/// out rather than a listing — and only when `$select` asked for it the way Graph requires.
///
/// That last part is the one place this mock is deliberately as unhelpful as the service:
/// Graph accepts a `$select` naming the URL by the key it answers under, and then omits it
/// from every row without saying so. A mock that answered anyway would hide the mistake,
/// which is exactly what it did until a live read failed.
fn with_host(row: &Value, host: &str, fresh: bool, query: &str) -> Value {
    let mut row = row.clone();
    let Some(u) = row.get(DOWNLOAD_URL_KEY).and_then(|u| u.as_str()) else {
        return row;
    };
    if !query.contains("content.downloadUrl") {
        row.as_object_mut().unwrap().remove(DOWNLOAD_URL_KEY);
        return row;
    }
    let mut u = u.replace("{HOST}", host);
    if fresh {
        u.push_str("&fresh=1");
    }
    row.as_object_mut()
        .unwrap()
        .insert(DOWNLOAD_URL_KEY.into(), json!(u));
    row
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
