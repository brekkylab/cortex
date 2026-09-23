use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::accessor::encode_path;
use super::super::{OnedriveConfig, OnedriveOrigins};
use super::*;

// ---------------------------------------------------------------------------
// Pure, no I/O
// ---------------------------------------------------------------------------

/// One listing row becomes the entry the mount shows, or nothing at all.
///
/// A **package** — a OneNote notebook is the one you meet — is dropped: Microsoft calls it
/// "a package instead of a folder or file", treated as a folder by some clients and a file
/// by others, and it has no bytes this API will serve. An item that is neither `file` nor
/// `folder` goes the same way. A name that cannot be read is worse than an absence.
#[test]
fn a_row_becomes_the_entry_it_should() {
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

    // A row that cannot be addressed after the listing is dropped, the same judgement one
    // more time. It is the id every later request uses — the download-URL refresh runs in
    // the middle of a read — so a `Child` carrying an empty one would not omit the file,
    // it would break reading it, and only once the URL expired.
    let mut idless = file_row("report.docx", "F1", 1234);
    idless.as_object_mut().unwrap().remove("id");
    assert!(child_from_item(&idless).is_none(), "no id, no entry");

    let file = child_from_item(&file_row("report.docx", "F1", 1234)).unwrap();
    assert_eq!(
        (file.size, file.etag.as_deref()),
        (1234, Some("ctag-report.docx")),
        "the exact size, and the content tag rather than the other one"
    );
    // A folder states a size too — the sum of what it contains, which is not a length
    // anything reads.
    assert_eq!(
        child_from_item(&folder_row("Documents", "D1"))
            .unwrap()
            .size,
        0
    );
}

/// Nothing addresses what it does not name.
///
/// A name is one path segment. OneDrive refuses most of what would need removing — `\ / :
/// * ? " < > |` are not allowed in one — so this guards a gateway that is less strict, `/`
/// being the one that would silently become a separator. And `..` in a *path* is refused
/// rather than walked: a name survives the sanitizer with almost anything in it, and the
/// tree has no `..` of its own for it to mean.
#[test]
fn nothing_addresses_what_it_does_not_name() {
    let evil = child_from_item(&file_row("../../etc/passwd", "E1", 1)).unwrap();
    assert!(!evil.name.contains('/'));
    assert_eq!(
        child_from_item(&file_row("..", "E2", 1)).unwrap().name,
        "untitled"
    );
    assert_eq!(
        child_from_item(&file_row("   ", "E3", 1)).unwrap().name,
        "untitled"
    );

    assert_eq!(vpath(Path::new("/a//b/")).unwrap(), "/a/b");
    assert_eq!(vpath(Path::new("/a/./b")).unwrap(), "/a/b");
    assert!(vpath(Path::new("/a/../b")).is_err(), "`..` is refused");
}

// ---------------------------------------------------------------------------
// Mock-backed
// ---------------------------------------------------------------------------

/// What the mock's `429` asks a client to wait, in seconds. Far past `MAX_RETRY_AFTER`,
/// which is the case that separates honouring a wait from sleeping through one.
const THROTTLED_FOR: u64 = 900;

/// What the mock's `4290` asks for instead: a wait that fits inside `MAX_RETRY_AFTER` on
/// its own, so what stops the ladder is the running total rather than any one sleep. Kept
/// small because the test really does sleep it.
const THROTTLED_SHORT: u64 = 1;

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

/// A path has two spellings, the service answers to one, and which one varies per segment.
///
/// `same_name` covers half of this already: a *name* is matched against a listing under
/// composition, so a lookup finds what `ls` printed. The other half is the path itself,
/// which is how this store addresses a folder. Measured against the live service, on one
/// folder under one name, `root:/문서:/children` is `200` composed and `404` decomposed,
/// while macOS hands a lookup the decomposed spelling. So `ls` on a directory it had just
/// listed answered `ENOENT`, and a read under it went the same way through its parent.
///
/// Composing the whole path answers that, and only that: it tries two spellings,
/// all-decomposed and all-composed. A drive touched by two clients has neither, because a
/// folder made on the web sits composed and one made under it by the macOS sync client sits
/// decomposed. So the last resort is what the Drive store does by construction, resolving
/// segment by segment and listing by the id that comes back.
///
/// One fixture holds both shapes.
#[tokio::test]
async fn a_path_resolves_under_the_spelling_the_kernel_hands_over() {
    // Stored composed, the ordinary case: one fallback reaches it.
    let simple: String = "문서".nfc().collect();
    // Stored composed over decomposed, which no single spelling of the path names.
    let parent: String = "상위".nfc().collect();
    let child: String = "하위".nfd().collect();
    assert_ne!(child, "하위".nfc().collect::<String>(), "two spellings");

    let mut tree = json!({
        "/": [folder_row(&simple, "D1"), folder_row(&parent, "D2")],
    });
    let obj = tree.as_object_mut().unwrap();
    obj.insert(format!("/{simple}"), json!([file_row("note.txt", "F1", 5)]));
    obj.insert(format!("/{parent}"), json!([folder_row(&child, "D3")]));
    obj.insert(
        format!("/{parent}/{child}"),
        json!([file_row("deep.txt", "F3", 5)]),
    );
    let mock = start(tree, HashMap::from([("F1".to_string(), b"hello".to_vec())])).await;
    let fs = mounted(&mock.config());
    let by_path = |m: &Mock| {
        m.targets()
            .iter()
            .filter(|t| t.contains(":/children"))
            .count()
    };

    // One segment, stored composed, asked decomposed. Two requests: as given, then composed.
    let asked: String = format!("/{simple}").nfd().collect();
    let listed = fs.list(Path::new(&asked)).await.expect("found composed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "note.txt");
    assert_eq!(by_path(&mock), 2, "{:?}", mock.targets());
    assert!(
        fs.list(Path::new("/"))
            .await
            .unwrap()
            .iter()
            .any(|d| d.name == simple),
        "and a listing still prints the service's own spelling"
    );
    // A file under it reads, through that same listing.
    let got = fs
        .read_window(Path::new(&format!("{asked}/note.txt")), Some(0..5))
        .await
        .expect("a file under it reads");
    assert_eq!(got, b"hello");

    // Two segments stored in different forms. Neither whole-path spelling names this, so it
    // resolves through the parent and lists by id.
    mock.reset();
    let asked: String = format!("/{parent}/{child}").nfd().collect();
    let listed = fs
        .list(Path::new(&asked))
        .await
        .expect("mixed still resolves");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "deep.txt");
    assert!(
        mock.asked_for("/me/drive/items/D3/children"),
        "resolved through the parent and listed by id: {:?}",
        mock.targets()
    );
}

/// A backend failure is not an absence.
///
/// `NotFound` is the one error a traversal is entitled to skip, so everything else must
/// stay distinguishable from it. Graph states the status in the status line and a
/// correlation id in the body, and a correlation id is hex: measured on a live token
/// failure, `Correlation ID: 74bd5c8a-0083-434f-b5b2-9602fa4d4ca3`. Roughly one in a
/// hundred spells `404` and means nothing by it. Read out of the message rather than off
/// the status, a folder you may not read becomes a folder that is not there.
#[tokio::test]
async fn a_backend_failure_is_not_an_absence() {
    let mock = start(
        json!({
            "/": [
                folder_row("denied", "D1"),
                folder_row("gone", "D2"),
            ],
            // A 403 whose correlation id happens to contain `404`.
            "/denied": 403,
        }),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&mock.config());
    let kind = async |p: &str| match fs.list(Path::new(p)).await {
        Err(e) => e.kind(),
        Ok(_) => panic!("{p} is an error"),
    };

    assert_ne!(
        kind("/denied").await,
        io::ErrorKind::NotFound,
        "a 403 is not an absence, whatever its correlation id spells"
    );
    assert_eq!(
        kind("/gone").await,
        io::ErrorKind::NotFound,
        "and a real 404 still is one"
    );
}

/// A throttle is waited out as asked, bounded, and then respected.
///
/// Three rules that only make sense together, so one fixture pins all three.
///
/// **Taken as asked or not at all.** Microsoft's guidance is to wait exactly what
/// `Retry-After` says, because usage keeps accruing while a client is throttled, so coming
/// back early lengthens the window. Shortening the wait while keeping the retry spends the
/// budget inside the window and extends it on every attempt.
///
/// **Bounded by the ladder, not by one sleep.** What blocks the mount is the sum: the FUSE
/// session loop is single-threaded, so this is the whole mount not answering. A cap on one
/// sleep would allow `MAX_RETRIES` of them and claim a ceiling five times smaller than the
/// truth.
///
/// **And giving up is a pause, not a green light.** A mount's callers re-ask constantly and
/// a failed listing is not cached, so a give-up with no cooldown sends *more* requests per
/// second than the waiting it replaced, against the limit that caused the throttle.
#[tokio::test]
async fn a_throttle_is_waited_out_as_asked_bounded_and_then_respected() {
    let of_folder = |m: &Mock| {
        m.targets()
            .iter()
            .filter(|t| t.contains(":/children"))
            .count()
    };

    // Asked for more than one call may spend waiting: reported at once, nothing slept.
    let mock = start(
        json!({"/": [folder_row("busy", "D1")], "/busy": 429}),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&mock.config());
    let started = Instant::now();
    let Err(e) = fs.list(Path::new("/busy")).await else {
        panic!("a throttle is an error");
    };
    assert_ne!(e.kind(), io::ErrorKind::NotFound, "not an absence: {e}");
    assert_eq!(of_folder(&mock), 1, "asked once: {:?}", mock.targets());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "and did not sleep through it, in {:?}",
        started.elapsed()
    );

    // And then said nothing more. Nineteen further asks, none of which reach the wire:
    // this is the half that makes giving up honest rather than merely fast.
    mock.reset();
    for _ in 0..19 {
        assert!(fs.list(Path::new("/busy")).await.is_err());
    }
    assert_eq!(
        of_folder(&mock),
        0,
        "a give-up that keeps asking is worse than waiting: {:?}",
        mock.targets()
    );

    // A wait that fits the budget *is* taken, and the ladder stops when their sum would
    // leave it. `THROTTLED_SHORT` is well under `MAX_RETRY_AFTER`, so what bounds this is
    // the running total and not any single sleep.
    let mock = start(
        json!({"/": [folder_row("slow", "D1")], "/slow": 4290}),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&mock.config());
    let started = Instant::now();
    assert!(fs.list(Path::new("/slow")).await.is_err());
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(THROTTLED_SHORT),
        "the wait it asked for was actually taken, not skipped: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "and the ladder stopped inside the budget rather than sleeping it per attempt: \
         {waited:?}"
    );
}

/// The span policy, in the cases that distinguish it, and the map it lives in.
///
/// The kernel's window is 64 KiB, 32 through FUSE-T, and not ours to choose. One ranged
/// request per window is
/// the pathology this exists to avoid: measured against Drive, whose round trip is the same
/// shape, it put a 641 MB archive at 0.04 MB/s. Two sizes rather than one because a span is
/// not free either — a reader that takes a file's head and stops would pay a whole
/// [`READ_SPAN`] for one buffer.
#[tokio::test]
async fn a_walk_pays_a_span_and_a_head_read_pays_less() {
    const REAL: u64 = 80 * 1024 * 1024;
    const CHUNK: u64 = 64 * 1024;
    let mock = start(
        json!({"/": [
            file_row("big.bin", "P1", REAL),
            file_row("other.bin", "P2", 4096),
        ]}),
        HashMap::from([
            ("P1".to_string(), vec![b'z'; REAL as usize]),
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

    // Carrying on from the span the reader spent is a walk, and pays the bigger one. Both
    // shapes of carrying on: a window that begins inside the span and reaches past its end
    // (which is what a window whose size does not divide the span does), and one that
    // begins exactly at the end. Each new span begins where its read began rather than on
    // a fixed boundary, which is what keeps a window from being split across two of them —
    // and so what makes a short return mean end of file.
    let mut at = FIRST_SPAN - CHUNK / 2;
    for _ in 0..2 {
        mock.reset();
        let got = fs.read_window(file, Some(at..at + CHUNK)).await.unwrap();
        assert_eq!(got.len() as u64, CHUNK, "at {at}");
        assert_eq!(
            mock.content_ranges(),
            vec![Some(format!("bytes={at}-{}", at + READ_SPAN - 1))],
            "carrying on from {at} pays READ_SPAN, from where the read began"
        );
        at += READ_SPAN;
    }

    // Past the end is an ordinary end. A window that starts inside the file and runs off it
    // comes back short — which is the only thing short is allowed to mean — and one wholly
    // past it comes back empty. Neither is an error.
    let tail = fs
        .read_window(file, Some(REAL - CHUNK / 2..REAL + CHUNK / 2))
        .await
        .unwrap();
    assert_eq!(tail.len() as u64, CHUNK / 2, "short, and short is the end");
    assert!(
        fs.read_window(file, Some(REAL + CHUNK..REAL + 2 * CHUNK))
            .await
            .unwrap()
            .is_empty(),
        "wholly past the end is empty"
    );
}

/// A span nobody has come back to stops dividing the budget, and then stops being kept.
///
/// Presence in the map is the wrong test for both. An entry lives for [`DIR_TTL`], so a
/// traversal across a folder leaves one behind per file it passed. Counting those as readers
/// cuts the share of the file actually being walked — nineteen of them took a walk from two
/// requests to five — and keeping them forever is how a map grows unbounded.
#[tokio::test]
async fn spans_left_behind_stop_counting_and_stop_being_kept() {
    const SMALL: u64 = 4 * 1024 * 1024;
    const BIG: u64 = 32 * 1024 * 1024;
    const W: u64 = 32 * 1024;
    let mut rows = vec![file_row("big.bin", "BIG", BIG)];
    let mut blobs: HashMap<String, Vec<u8>> =
        HashMap::from([("BIG".to_string(), vec![b'z'; BIG as usize])]);
    for i in 0..5 {
        rows.push(file_row(&format!("s{i}.bin"), &format!("S{i}"), SMALL));
        blobs.insert(format!("S{i}"), vec![b's'; SMALL as usize]);
    }
    let mock = start(json!({"/": rows}), blobs).await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();

    // Touched once each and not returned to, the way a traversal leaves them.
    for i in 0..5 {
        fs.read_window(Path::new(&format!("/s{i}.bin")), Some(0..W))
            .await
            .unwrap();
    }
    fs.age_spans_for_test(ACTIVE + Duration::from_secs(1)).await;

    // Now walk one file. It is the only reader, so it gets the whole budget as its span and
    // the walk is two fetches: a first span, then the rest of the file.
    mock.reset();
    let big = Path::new("/big.bin");
    let mut at = 0;
    while at < BIG {
        fs.read_window(big, Some(at..(at + W).min(BIG)))
            .await
            .unwrap();
        at += W;
    }
    assert_eq!(
        mock.content_ranges().len(),
        2,
        "the abandoned spans are not readers: {:?}",
        mock.content_ranges()
    );
    assert_eq!(mock.bytes_sent(), BIG, "and nothing was fetched twice");

    // Past the TTL they are not even kept. The sweep runs when something is held, since
    // nothing else ever removes an entry.
    fs.age_spans_for_test(DIR_TTL + Duration::from_secs(1))
        .await;
    fs.read_window(Path::new("/s0.bin"), Some(0..W))
        .await
        .unwrap();
    assert!(
        fs.held_bytes("/big.bin").await.is_none(),
        "aged out and swept on the way in"
    );
    for i in 1..5 {
        assert!(
            fs.held_bytes(&format!("/s{i}.bin")).await.is_none(),
            "s{i} too"
        );
    }
}

/// Reads of several files interleave, and each one keeps its span.
///
/// Not exotic and not threaded. FUSE ops are serialized, so alternating is all it takes, and
/// the 512 KiB chunk this alternates in is what a local `PassthroughFs` mount produced under
/// `grep -r`, where the NFS client's read-ahead pulled the next file in before the current
/// one was done. A network store does not do that on its own, so this pattern is the shape of
/// the hazard rather than a claim about what one tool costs here — see [`OnedriveFs::held`]
/// for what two concurrent readers measured live.
///
/// The assertion is that nothing is fetched twice. One slot could not make it: each read
/// found another file's span, so `walking` never became true and every window bought a whole
/// [`FIRST_SPAN`]. Three files rather than two, and each usefully larger than a span, because
/// a constant [`READ_SPAN`] clamped by a smaller file never overruns the budget and the
/// division would go unmeasured.
#[tokio::test]
async fn interleaved_files_each_keep_a_span() {
    const REAL: u64 = 32 * 1024 * 1024;
    // The traced window is 32 KiB and the traced chunk 512 KiB. Only the chunk decides what
    // this exercises, so the window is widened to keep the walk cheap.
    const W: u64 = 256 * 1024;
    const CHUNK: u64 = 2;
    let ids = ["P1", "P2", "P3"];
    let mock = start(
        json!({"/": ids.iter().enumerate()
            .map(|(i, id)| file_row(&format!("f{i}.bin"), id, REAL))
            .collect::<Vec<_>>()}),
        ids.iter()
            .map(|id| (id.to_string(), vec![b'z'; REAL as usize]))
            .collect(),
    )
    .await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();
    mock.reset();

    let mut at = 0;
    while at < REAL {
        for i in 0..ids.len() {
            let path = format!("/f{i}.bin");
            for k in 0..CHUNK {
                let o = at + k * W;
                if o >= REAL {
                    break;
                }
                let got = fs
                    .read_window(Path::new(&path), Some(o..(o + W).min(REAL)))
                    .await
                    .unwrap();
                assert_eq!(got.len() as u64, (REAL - o).min(W), "f{i} at {o}");
            }
        }
        at += CHUNK * W;
    }

    // Nothing is fetched twice, up to the overlap a span boundary costs: a span begins where
    // the reader asks, so a window straddling the end of one makes the next start inside it.
    // That overlap is under one window per span, and it is what buys never splitting a window
    // across two spans — which is what makes a short read mean EOF.
    //
    // No ceiling on the span count, because one cannot catch this: reverting the share-sizing
    // half alone fetches *fewer* spans than the fix does, by taking a whole `READ_SPAN` each
    // time and throwing most of it away. What separates them is the waste.
    let consumed = REAL * ids.len() as u64;
    let spans = mock.content_ranges().len() as u64;
    let waste = mock.bytes_sent().saturating_sub(consumed);
    assert!(
        waste < spans * W,
        "{waste} wasted over {spans} spans is more than a boundary each: {:?}",
        mock.content_ranges()
    );
    for (i, id) in ids.iter().enumerate() {
        assert!(
            fs.held_bytes(&format!("/f{i}.bin")).await.is_some(),
            "f{i} ({id}) still holds a span at the end"
        );
    }
}

/// **The response, not the request, says where the bytes begin.**
///
/// Microsoft documents it: *"If the range can't be generated the Range header may be
/// ignored and an HTTP 200 response would be returned with the full contents of the
/// file."* Believing the request then serves the front of the file as though it came from
/// the middle, silently, with the right length and the wrong content.
#[tokio::test]
async fn a_window_is_read_from_where_the_response_says_it_starts() {
    const REAL: usize = 1024 * 1024;
    const AT: u64 = 700_000;
    let body: Vec<u8> = (0..REAL).map(|i| (i % 251) as u8).collect();
    let want = &body[AT as usize..AT as usize + 4096];

    let mock = start_full(
        json!({"/": [file_row("whole.bin", "P1", REAL as u64)]}),
        HashMap::from([("P1".to_string(), body.clone())]),
        RangeMode::Ignore,
        false,
    )
    .await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();
    let got = fs
        .read_window(Path::new("/whole.bin"), Some(AT..AT + 4096))
        .await
        .unwrap();
    assert_eq!(got, want, "the window is the file's bytes at that offset");
}

/// A download URL is short-lived by design, so an expired one costs exactly one refetch.
///
/// Exactly one, in both directions. A fresh URL that works makes the read succeed. A fresh
/// URL that fails too is a fault, and retrying it forever would hide it; it is not an
/// absence either, since the item answered `get_item_by_id`. And that error must not carry
/// the URL. A preauthenticated download URL's query string *is* the grant, and reqwest
/// attaches the URL to a transport error, so a refused connection would put a token that
/// hands over the file into whatever reads the error. `dead.bin` points at a closed port
/// for that reason: the transport path, which the hand-built status message does not cover.
#[tokio::test]
async fn an_expired_download_url_is_refetched_once_and_then_given_up() {
    const SENTINEL: &str = "SENTINEL-GRANT-DO-NOT-LOG";
    let body = vec![b'k'; 4096];
    let mut dead = file_row("dead.bin", "P2", 4096);
    dead.as_object_mut().unwrap().insert(
        DOWNLOAD_URL_KEY.into(),
        json!(format!("http://127.0.0.1:1/blob?tempauth={SENTINEL}")),
    );
    let mock = start_full(
        json!({"/": [file_row("stale.bin", "P1", 4096), dead]}),
        HashMap::from([("P1".to_string(), body.clone())]),
        RangeMode::Honour,
        // The URL a listing hands out has expired; the one an item fetch hands out has
        // not. That is the shape of the real failure.
        true,
    )
    .await;
    let fs = mounted(&mock.config());
    fs.list(Path::new("/")).await.unwrap();

    mock.reset();
    let got = fs
        .read_window(Path::new("/stale.bin"), Some(0..4096))
        .await
        .unwrap();
    assert_eq!(got, body, "the read succeeded on the fresh url");
    assert!(
        mock.asked_for("/me/drive/items/P1?"),
        "by asking the service for the item again, by id: {:?}",
        mock.targets()
    );
    assert_eq!(
        mock.content_ranges().len(),
        2,
        "one failed attempt and one that worked"
    );

    let Err(e) = fs.read_window(Path::new("/dead.bin"), Some(0..4096)).await else {
        panic!("a download that never succeeds is an error");
    };
    assert_ne!(
        e.kind(),
        io::ErrorKind::NotFound,
        "the item answered, so the bytes failing is a fault: {e}"
    );
    assert!(
        !format!("{e}").contains(SENTINEL),
        "the grant reached the error text: {e}"
    );
}

/// What the listing cache does and does not remember.
///
/// A failure is not an answer: caching one would turn a single throttled request into five
/// minutes of an empty directory, which reads as "the folder is gone" rather than "ask
/// again". Nor is a miss a reason to go looking: a name with one spelling that the service
/// says is absent is absent. And what has aged out is dropped on the way in, so a traversal
/// does not leave a listing per folder it ever visited behind it.
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
    // One request and nothing else. A name with one spelling cannot be rescued by trying
    // another or by walking to it, and a macOS mount probes for `.DS_Store` in every
    // directory it touches, so each such miss must stay the single request it already is.
    assert_eq!(mock.targets().len(), 1, "{:?}", mock.targets());
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
