use std::path::Path;
use std::sync::Mutex as StdMutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use serde_json::json;

use super::super::GdriveOrigins;
use super::*;

/// The size a listing row came with.
///
/// Every row this backend builds carries a [`Stat`] (see [`dirent_for`]), so a row
/// without one is a bug here rather than a case to handle.
fn size_of(e: &Dirent) -> u64 {
    e.stat()
        .expect("a gdrive listing row always carries its stat")
        .size
}

fn file_row(name: &str, id: &str, mime: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "mimeType": mime,
        "size": "47065",
        "modifiedTime": "2026-01-30T09:00:00Z",
        "createdTime": "2025-11-01T12:00:00Z",
        "webViewLink": format!("https://drive.google.com/file/d/{id}/view"),
        "owners": [{"displayName": "Mia Lopez", "emailAddress": "mia@acme.com"}],
    })
}

/// What one Drive row becomes on the mount.
///
/// A file with bytes keeps its own name and Drive's own size — in *bytes*, which is the
/// trap: a Korean or emoji name is longer in bytes than in characters, and a size counted
/// in characters truncates every read of it. A Docs-editors type has no bytes at all, so it
/// becomes one entry serving its API's JSON, named with the suffix that says which kind it
/// is, and with no length until something produces one. A native type with nothing to
/// convert is absent, because a name that cannot be read is worse than no name.
#[test]
fn a_row_becomes_the_thing_it_can_serve() {
    let entry = |name: &str, mime: &str| child_from_file(&file_row(name, "id1", mime));

    // Real bytes: plain name, Drive's own size, ranged reads.
    let pdf = entry("report.pdf", "application/pdf").unwrap();
    assert_eq!(pdf.vfs_name, "report.pdf");
    assert_eq!(pdf.serves, Serves::Original);
    assert_eq!(entry_size(&pdf), 47065, "Drive's reported size");
    assert_eq!(
        entry("notes.md", "text/markdown").unwrap().vfs_name,
        "notes.md",
        "already text — the file *is* its text"
    );

    // And in bytes, whatever the script.
    for name in [
        "분기 보고서.pdf",
        "日本語のファイル",
        "Ελληνικά έγγραφο",
        "мой документ",
        "مستند عربي",
        "minutes 📎 2026-03.txt",
    ] {
        assert!(
            name.len() > name.chars().count(),
            "{name}: this case should actually be multi-byte"
        );
        let c = entry(name, "application/pdf").unwrap();
        assert_eq!(c.vfs_name, name, "the entry name is the Drive name");
        assert_eq!(entry_size(&c), 47065, "{name}");
    }

    // Docs-editors types: one entry, the document's own API JSON. The suffix says which
    // kind, since the Drive name carries no extension — and it survives any script.
    for (mime, suffix, api) in [
        (
            "application/vnd.google-apps.document",
            ".gdoc.json",
            NativeApi::Doc,
        ),
        (
            "application/vnd.google-apps.spreadsheet",
            ".gsheet.json",
            NativeApi::Sheet,
        ),
        (
            "application/vnd.google-apps.presentation",
            ".gslide.json",
            NativeApi::Slides,
        ),
    ] {
        let c = entry("분기 보고서", mime).unwrap();
        assert_eq!(c.vfs_name, format!("분기 보고서{suffix}"), "{mime}");
        assert_eq!(c.serves, Serves::Native(api), "{mime}");
        assert_eq!(
            entry_size(&c),
            UNKNOWN_LENGTH_SIZE,
            "{mime}: no length until the API answers"
        );
    }

    // Nothing to convert, nothing to serve: not listed at all.
    for mime in [
        "application/vnd.google-apps.form",
        "application/vnd.google-apps.map",
        "application/vnd.google-apps.drawing",
    ] {
        assert!(entry("Survey", mime).is_none(), "{mime}");
    }

    // A folder stays a directory under its plain name.
    let dir = entry("Reports", FOLDER_MIME).unwrap();
    assert_eq!(
        (dir.kind, dir.vfs_name.as_str()),
        (GKind::Folder, "Reports")
    );
    assert_eq!(entry_size(&dir), 0);

    // A row Drive reported no size for still lists, with the placeholder rather than 0:
    // under the guest's `direct_io` mount a 0 was measured to clamp reads to nothing, and
    // a search tool skips a file it is told is empty.
    let mut sizeless = file_row("mystery.bin", "id1", "application/octet-stream");
    sizeless.as_object_mut().unwrap().remove("size");
    let c = child_from_file(&sizeless).unwrap();
    assert_eq!(c.serves, Serves::Original);
    assert_eq!(entry_size(&c), UNKNOWN_LENGTH_SIZE);
}

#[test]
fn drive_names_cannot_escape_their_directory() {
    // Drive allows `/` in a name; it must not become a path separator.
    let evil = child_from_file(&file_row("../../etc/passwd", "e1", "text/plain")).unwrap();
    assert!(!evil.vfs_name.contains('/'));
    let dotdot = child_from_file(&file_row("..", "e2", "text/plain")).unwrap();
    assert_eq!(dotdot.vfs_name, "untitled");
    // Same guard on the conversion path, where a suffix is appended.
    let native = child_from_file(&file_row(
        "..",
        "e3",
        "application/vnd.google-apps.document",
    ))
    .unwrap();
    assert_eq!(native.vfs_name, "untitled.gdoc.json");
}

#[test]
fn shared_drive_names_dodge_the_root_sections() {
    let existing: HashSet<String> =
        [MY_DRIVE_NAME.to_string(), SHARED_WITH_ME_NAME.to_string()].into();
    assert_eq!(unique_name("Team", &existing), "Team");
    assert_eq!(
        unique_name(MY_DRIVE_NAME, &existing),
        "My Drive [Shared Drive]"
    );
}

/// What a directory means by "the same name", and what it does when two entries mean it.
///
/// One name has two spellings in Unicode — `한` is a single code point composed, or three
/// jamo decomposed — and both are in play at once: macOS hands a lookup the *decomposed*
/// form of whatever a listing returned, while Drive stores whichever the uploading client
/// sent. Measured on one real folder, ten composed names beside four decomposed. So a byte
/// comparison answers `ENOENT` for a name `ls` printed a moment earlier, and which files it
/// does that to depends on what uploaded them.
///
/// Everything downstream of that comparison has to agree with it. Counting collisions by
/// bytes left a canonically equal pair *both* unnumbered while the lookup matched either
/// spelling to whichever came first: one file unopenable, and `cat` on it serving the other
/// one's contents. And the number follows the Drive id rather than arrival order, because
/// rows arrive `modifiedTime desc` with no defined tiebreak — numbering as they arrive meant
/// editing either of two `report.pdf` swapped which one was `report (2).pdf` at the next
/// listing, and the saved path still resolved, still succeeded, and opened the other
/// document.
#[test]
fn names_are_compared_and_numbered_by_composition_and_id() {
    let mk = |name: &str, id: &str, serves: Serves| Child {
        vfs_name: name.to_string(),
        id: id.into(),
        drive_id: None,
        kind: if matches!(serves, Serves::Nothing) {
            GKind::Folder
        } else {
            GKind::File
        },
        mtime: None,
        created: None,
        serves,
        size: None,
    };

    // The rule itself. Bytes first, then composition — and no ASCII short-circuit, because
    // a pair spanning that boundary can still compose to one name: `NFC("\u{212A}")`, the
    // Kelvin sign, is `"K"`.
    let composed = "한글.txt";
    let decomposed: String = composed.nfd().collect();
    assert_ne!(
        composed.as_bytes(),
        decomposed.as_bytes(),
        "the fixture has to be two spellings, or it tests nothing"
    );
    assert!(same_name(composed, &decomposed), "one name, two spellings");
    assert!(!same_name("한글.txt", "한국.txt"), "and not a collapse");
    assert!(!same_name("report.pdf", "report (2).pdf"));
    assert_eq!("\u{212A}".nfc().collect::<String>(), "K");
    assert!(same_name("2\u{212A} readings.txt", "2K readings.txt"));
    assert!(same_name("a\u{037E}b", "a;b"));

    // The number goes *before* the extension, or the entry leaves every glob a reader
    // would use — measured against a real account, two of 33 spreadsheets were invisible
    // to `**/*.gsheet.json`. A folder is not renamed around a dot.
    let mut children = vec![
        mk("report.gsheet.json", "s1", Serves::Native(NativeApi::Sheet)),
        mk("report.gsheet.json", "s2", Serves::Native(NativeApi::Sheet)),
        mk("report.gsheet.json", "s3", Serves::Native(NativeApi::Sheet)),
        mk("photo.jpeg", "p1", Serves::Original),
        mk("photo.jpeg", "p2", Serves::Original),
        mk("notes", "n1", Serves::Original),
        mk("notes", "n2", Serves::Original),
        mk("v1.2", "v1", Serves::Nothing),
        mk("v1.2", "v2", Serves::Nothing),
    ];
    disambiguate(&mut children);
    assert_eq!(
        children
            .iter()
            .map(|c| c.vfs_name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "report.gsheet.json",
            "report (2).gsheet.json",
            "report (3).gsheet.json",
            "photo.jpeg",
            "photo (2).jpeg",
            "notes",
            "notes (2)",
            "v1.2",
            "v1.2 (2)",
        ]
    );

    // A pair that differs only by composition is a collision, so one of them is numbered.
    let two = "보고서.pdf".to_string();
    let two_nfd: String = two.nfd().collect();
    let mut pair = vec![
        mk(&two, "a", Serves::Original),
        mk(&two_nfd, "b", Serves::Original),
    ];
    disambiguate(&mut pair);
    assert!(
        !same_name(&pair[0].vfs_name, &pair[1].vfs_name),
        "both were left as {:?} / {:?}, so one cannot be opened",
        pair[0].vfs_name,
        pair[1].vfs_name
    );
    assert!(
        pair.iter()
            .find(|c| c.vfs_name.contains("(2)"))
            .expect("one of the two is numbered")
            .vfs_name
            .ends_with(".pdf"),
        "and it is still findable by glob"
    );

    // And the assignment does not move when the listing order does — three edits, three
    // arrival orders, the same three names.
    let assign = |ids: [&str; 3]| {
        let mut c: Vec<Child> = ids
            .iter()
            .map(|i| mk("report.pdf", i, Serves::Original))
            .collect();
        disambiguate(&mut c);
        let mut by_id: Vec<(String, String)> = c.into_iter().map(|c| (c.id, c.vfs_name)).collect();
        by_id.sort();
        by_id
    };
    let a = assign(["x1", "x2", "x3"]);
    assert_eq!(a, assign(["x3", "x1", "x2"]), "an edit must not renumber");
    assert_eq!(a, assign(["x2", "x3", "x1"]), "an edit must not renumber");
    assert_eq!(
        a,
        vec![
            ("x1".to_string(), "report.pdf".to_string()),
            ("x2".to_string(), "report (2).pdf".to_string()),
            ("x3".to_string(), "report (3).pdf".to_string()),
        ]
    );
}

/// A shared drive scopes every listing under it, not just its own root.
///
/// The mock answered `/drives` with an empty array on every success path, so no runnable
/// test ever produced a `GKind::SharedDrive` child and nothing covered `driveId` at all —
/// deleting the propagation left the suite green. Against a real account it makes every
/// folder inside a shared drive list empty from the second level down.
#[tokio::test]
async fn a_shared_drive_scopes_the_listings_below_it() {
    let mock = start_full(
        json!([
            row("Sub", "F1", FOLDER_MIME, None),
            row("memo.txt", "B1", "text/plain", Some("12")),
        ]),
        HashMap::new(),
        None,
        None,
        Some(json!({"drives": [{"id": "DRV", "name": "Team"}]})),
    )
    .await;
    let fs = mounted(&mock.config());

    let root: Vec<String> = fs
        .list(Path::new("/"))
        .await
        .unwrap()
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        root.contains(&"Team".to_string()),
        "the drive is a section: {root:?}"
    );

    // The drive's own root, and then a folder inside it: both have to be asked for
    // within the drive, or Drive answers from My Drive and the folder lists empty.
    fs.list(Path::new("/Team")).await.unwrap();
    assert!(
        mock.asked_for("driveId=DRV"),
        "the drive's root was listed without scoping it to the drive"
    );
    assert!(mock.asked_for("corpora=drive"));

    mock.reset();
    fs.list(Path::new("/Team/Sub")).await.unwrap();
    assert!(
        mock.asked_for("driveId=DRV"),
        "the id did not reach a folder one level down, which is where it stops mattering"
    );
}

fn workbook(titles: &[Option<&str>]) -> Value {
    serde_json::json!({
        "sheets": titles
            .iter()
            .map(|t| match t {
                Some(t) => serde_json::json!({ "properties": { "title": t } }),
                None => serde_json::json!({ "properties": {} }),
            })
            .collect::<Vec<_>>()
    })
}

fn batch(pairs: &[(&str, &str)]) -> Value {
    serde_json::json!({
        "valueRanges": pairs
            .iter()
            .map(|(range, cell)| serde_json::json!({
                "range": range,
                "values": [[cell]],
            }))
            .collect::<Vec<_>>()
    })
}

fn values_of(wb: &Value, i: usize) -> Option<String> {
    wb["sheets"][i]["values"][0][0].as_str().map(str::to_string)
}

fn omitted_reason(wb: &Value, i: usize) -> Option<String> {
    wb["sheets"][i]["valuesOmitted"]["reason"]
        .as_str()
        .map(str::to_string)
}

/// How a workbook's cell values are paired, budgeted, and accounted for when they are not
/// there.
///
/// Every case here is the same failure from a different side: **cells under the wrong
/// sheet, or missing with nothing said about it.** Values are paired by the sheet a reply
/// *names* in A1 notation rather than by position, because a sheet the request had to skip
/// used to consume the next one's values and shift the rest. And every omission carries its
/// reason — a titleless sheet, one past the tab cap, one over the byte budget — because the
/// one thing a reader cannot recover from is an empty grid that looks like an empty sheet.
#[test]
fn a_workbooks_values_are_paired_by_name_and_budgeted_tab_by_tab() {
    // The reply names its sheet, and a quoted title can hold what would otherwise confuse
    // the parse.
    assert_eq!(range_title("Sheet1!A1:Z1000"), "Sheet1");
    assert_eq!(range_title("'연간 요약'!A1:Z968"), "연간 요약");
    assert_eq!(range_title("'Sheet1!B2'!A1:Z10"), "Sheet1!B2");
    assert_eq!(range_title("'it''s'!A1"), "it's");
    assert_eq!(range_title("Sheet1"), "Sheet1");

    // A sheet the request skipped takes nothing from the sheets around it.
    let mut wb = workbook(&[Some("Alpha"), None, Some("Gamma")]);
    fold_values(
        &mut wb,
        &batch(&[("Alpha!A1", "ALPHA-CELL"), ("Gamma!A1", "GAMMA-CELL")]),
        &["Alpha".to_string(), "Gamma".to_string()],
    );
    assert_eq!(values_of(&wb, 0).as_deref(), Some("ALPHA-CELL"));
    assert_eq!(values_of(&wb, 2).as_deref(), Some("GAMMA-CELL"));
    assert_eq!(values_of(&wb, 1), None, "the titleless sheet gets nothing");
    assert!(
        omitted_reason(&wb, 1).is_some_and(|r| r.contains("no title")),
        "and says why"
    );

    // Past the tab cap no request was made, so there are no values — and that used to be
    // the one silent omission here.
    let titles: Vec<String> = (0..MAX_TABS + 2).map(|i| format!("T{i}")).collect();
    let mut wb = workbook(&titles.iter().map(|t| Some(t.as_str())).collect::<Vec<_>>());
    // Asked for the way the read path asks, so the cap being *in* that call is what makes
    // the tail below unrequested — rather than this test deciding it separately.
    let asked = tab_titles(&wb);
    assert_eq!(
        asked,
        titles[..MAX_TABS],
        "the cap is applied where the ask is built"
    );
    let pairs: Vec<(String, String)> = asked
        .iter()
        .map(|t| (format!("{t}!A1"), format!("{t}-CELL")))
        .collect();
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(r, c)| (r.as_str(), c.as_str()))
        .collect();
    fold_values(&mut wb, &batch(&refs), &asked);
    assert_eq!(values_of(&wb, 0).as_deref(), Some("T0-CELL"));
    for i in MAX_TABS..MAX_TABS + 2 {
        assert_eq!(values_of(&wb, i), None);
        assert!(
            omitted_reason(&wb, i).is_some_and(|r| r.contains("tab cap")),
            "tab {i} must not be silently empty"
        );
    }

    // The budget is spent tab by tab, so one oversized tab does not drop every later one
    // however small.
    let mut wb = workbook(&[Some("small1"), Some("huge"), Some("small2")]);
    let huge = "x".repeat(GRID_BYTES_BUDGET as usize + 1);
    let mut b = batch(&[("small1!A1", "a"), ("small2!A1", "b")]);
    b["valueRanges"].as_array_mut().unwrap().insert(
        1,
        serde_json::json!({ "range": "huge!A1", "values": [[huge]] }),
    );
    fold_values(&mut wb, &b, &["small1", "huge", "small2"].map(String::from));
    assert_eq!(values_of(&wb, 0).as_deref(), Some("a"));
    assert!(omitted_reason(&wb, 1).is_some_and(|r| r.contains("budget")));
    assert_eq!(
        values_of(&wb, 2).as_deref(),
        Some("b"),
        "a small tab after a large one still fits"
    );

    // And that budget is counted in the form the file is served in. Indenting a grid of
    // short cells costs half again its compact size, so a budget checked against the
    // compact form quietly admits that much more.
    let values: Value = serde_json::json!(
        (0..200)
            .map(|r| (0..20).map(|c| format!("{r}-{c}")).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
    let compact = serde_json::to_vec(&values).unwrap().len() as u64;
    let served = served_len(&values);
    assert_eq!(
        served,
        serde_json::to_vec_pretty(&values).unwrap().len() as u64,
        "counted, not estimated"
    );
    assert!(
        served * 2 > compact * 3,
        "indenting a grid costs at least half again: {compact} -> {served}"
    );
}

/// A range that asks backwards is empty, not a crash. The window arrives from
/// whatever a client asked for, and `data[4..2]` aborts the process rather than
/// erroring — which makes this the provider's job, not the caller's.
#[test]
fn a_backwards_window_is_empty_rather_than_fatal() {
    let data = b"abcdef";
    // Built from values: a literal `4..2` is a lint, but a range arriving from a
    // client is just two numbers.
    let backwards = |start: u64, end: u64| slice(data, Some(start..end));
    assert!(backwards(4, 2).is_empty());
    assert!(backwards(9, 3).is_empty());
    assert!(backwards(6, 6).is_empty());
    // Past the end clamps to the end; the ordinary cases keep working.
    assert_eq!(backwards(4, 99).len(), 2);
    assert_eq!(slice(data, Some(1..3)), b"bc");
    assert_eq!(slice(data, None), data);
}

// ---------------------------------------------------------------------------
// Drive behind a loopback mock.
//
// A `tokio::net::TcpListener` answering canned Drive and Docs responses, pointed at
// with `GdriveOrigins`, counting the `Range` of every request and the bytes handed back.
// Which is the only way to tell a window from a whole object, and so the only way to
// hold the difference these tests exist for. No credentials, no new dependency.
// ---------------------------------------------------------------------------

/// One request as the mock saw it: enough to tell a window from a whole object.
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
    fn config(&self) -> GdriveConfig {
        GdriveConfig {
            client_id: "cid".into(),
            client_secret: "cs".into(),
            refresh_token: "rt".into(),
            origins: GdriveOrigins::behind(&self.addr),
        }
    }

    /// Range headers of the `alt=media` requests, in order. `None` means the whole
    /// object was asked for.
    fn media_ranges(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.target.contains("alt=media"))
            .map(|s| s.range.clone())
            .collect()
    }

    /// Whether any request went to a target containing `needle`.
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// `bytes=start-end` or `bytes=start-`.
fn parse_range(h: &str) -> Option<(u64, Option<u64>)> {
    let (s, e) = h.trim().strip_prefix("bytes=")?.split_once('-')?;
    Some((s.parse().ok()?, e.parse().ok()))
}

/// Serve a token, one folder listing, and one blob with `Range` support.
async fn start(listing: Value, blobs: HashMap<String, Vec<u8>>) -> Mock {
    start_full(listing, blobs, None, None, None).await
}

/// And a `/documents/` route answering `pad` bytes of JSON with no `Content-Length`, the
/// way the real Docs API does.
async fn start_with_document(
    listing: Value,
    blobs: HashMap<String, Vec<u8>>,
    document_pad: Option<usize>,
) -> Mock {
    start_full(listing, blobs, document_pad, None, None).await
}

/// The whole form, called directly by the two tests that want its last two arguments.
///
/// `drives_status` answers `/drives` with that status instead of a listing; `drives`
/// replaces the empty listing it answers with otherwise — that route answered empty on
/// every success path before this argument existed, which left `driveId` propagation with
/// no runnable test at all.
async fn start_full(
    listing: Value,
    blobs: HashMap<String, Vec<u8>>,
    document_pad: Option<usize>,
    drives_status: Option<u16>,
    drives: Option<Value>,
) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let body_bytes = Arc::new(StdMutex::new(0u64));
    let (log, written, blobs) = (seen.clone(), body_bytes.clone(), Arc::new(blobs));
    let listing = Arc::new(listing);
    let document_pad = Arc::new(document_pad);
    let drives_status = Arc::new(drives_status);
    let drives = Arc::new(drives);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (log, written, blobs, listing, document_pad, drives_status, drives) = (
                log.clone(),
                written.clone(),
                blobs.clone(),
                listing.clone(),
                document_pad.clone(),
                drives_status.clone(),
                drives.clone(),
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
                // Drain an announced body (the token POST) so the client isn't left
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
                    let mut h = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    if let Some(e) = extra {
                        h.push_str(&e);
                        h.push_str("\r\n");
                    }
                    h.push_str("\r\n");
                    let mut out = h.into_bytes();
                    out.extend_from_slice(&body);
                    (out, body.len() as u64)
                };

                // A document, streamed without a length: what the Docs API does.
                if let Some(pad) = *document_pad
                    && path.contains("/documents/")
                {
                    let body = json!({ "body": "x".repeat(pad) }).to_string().into_bytes();
                    let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                        .to_vec();
                    let _ = sock.write_all(&head).await;
                    let mut sent = 0u64;
                    for c in body.chunks(64 * 1024) {
                        let framed = format!("{:x}\r\n", c.len()).into_bytes();
                        if sock.write_all(&framed).await.is_err()
                            || sock.write_all(c).await.is_err()
                            || sock.write_all(b"\r\n").await.is_err()
                        {
                            break;
                        }
                        sent += c.len() as u64;
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                    *written.lock().unwrap() += sent;
                    let _ = sock.shutdown().await;
                    return;
                }
                let (out, body_len) = if method == "POST" && path.ends_with("/oauth2/token") {
                    reply(
                        200,
                        json!({"access_token": "at", "expires_in": 3600})
                            .to_string()
                            .into_bytes(),
                        None,
                    )
                } else if path.ends_with("/drives") {
                    match *drives_status {
                        Some(st) => reply(st, br#"{"error":"scripted"}"#.to_vec(), None),
                        None => reply(
                            200,
                            drives
                                .as_ref()
                                .clone()
                                .unwrap_or_else(|| json!({"drives": []}))
                                .to_string()
                                .into_bytes(),
                            None,
                        ),
                    }
                } else if path.ends_with("/drive/v3/files") {
                    reply(
                        200,
                        json!({ "files": *listing }).to_string().into_bytes(),
                        None,
                    )
                } else if query.contains("alt=media") {
                    let id = path.rsplit('/').next().unwrap_or("").to_string();
                    let blob = blobs.get(&id).cloned().unwrap_or_default();
                    match range.as_deref().and_then(parse_range) {
                        Some((start, _)) if start >= blob.len() as u64 => {
                            reply(416, Vec::new(), None)
                        }
                        Some((start, end)) => {
                            let last = end
                                .unwrap_or(blob.len() as u64 - 1)
                                .min(blob.len() as u64 - 1);
                            let window = blob[start as usize..=last as usize].to_vec();
                            let cr = format!("Content-Range: bytes {start}-{last}/{}", blob.len());
                            reply(206, window, Some(cr))
                        }
                        None => reply(200, blob, None),
                    }
                } else {
                    reply(404, br#"{"error":"no route"}"#.to_vec(), None)
                };
                if sock.write_all(&out).await.is_ok() {
                    *written.lock().unwrap() += body_len;
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

/// The provider as `build_mounts` assembles it.
/// The backend itself. agent-k drove a caching wrapper here; this crate has none —
/// a `FileSystem` holds whatever it means to hold, which is why the render cache
/// these tests watch lives inside [`GdriveFs`].
fn mounted(cfg: &GdriveConfig) -> GdriveFs {
    GdriveFs::new(cfg).unwrap()
}

fn row(name: &str, id: &str, mime: &str, size: Option<&str>) -> Value {
    let mut v = json!({
        "id": id,
        "name": name,
        "mimeType": mime,
        "modifiedTime": "2026-01-30T09:00:00Z",
    });
    if let Some(s) = size {
        v.as_object_mut().unwrap().insert("size".into(), json!(s));
    }
    v
}

/// What the length is remembered *against*: the `modifiedTime` the listing carried.
///
/// A clock would be wrong in both directions — it drops a length that is still right, and
/// serves one that is already wrong until it lapses. The listing already states when the
/// document last changed, so the entry can expire on exactly that and nothing else.
#[tokio::test]
async fn a_remembered_length_belongs_to_the_version_it_was_measured_from() {
    let mock = start(json!([]), HashMap::new()).await;
    let fs = mounted(&mock.config());
    let at = |secs: u64| Some(std::time::UNIX_EPOCH + Duration::from_secs(secs));
    let child = |mtime| Child {
        vfs_name: "notes.gdoc.json".into(),
        id: "D1".into(),
        drive_id: None,
        kind: GKind::File,
        mtime,
        created: None,
        serves: Serves::Native(NativeApi::Doc),
        size: None,
    };

    fs.remember_len(&child(at(1000)), 4242).await;
    assert_eq!(
        fs.remembered_len(&child(at(1000))).await,
        Some(4242),
        "the same version keeps its length"
    );
    assert_eq!(
        fs.remembered_len(&child(at(2000))).await,
        None,
        "an edited document does not keep the old one"
    );
    assert_eq!(
        fs.remembered_len(&child(None)).await,
        None,
        "and a row with no modifiedTime states nothing to match"
    );

    // Nor is such a row remembered in the first place. `None` matches `None`, so an entry
    // stamped with nothing is valid forever and no TTL underneath would retire it — and a
    // length that outlives its document is short the moment the document grows, which is
    // the one shape `read_at` cannot pad around.
    fs.remember_len(&child(None), 4242).await;
    assert_eq!(
        fs.remembered_len(&child(None)).await,
        None,
        "an undatable length is not kept"
    );
    assert_eq!(
        fs.lengths_remembered().await,
        1,
        "and nothing was stored for it"
    );
}

/// A document answers `stat` with two different numbers, and which one depends on whether
/// anything has read it.
///
/// Neither half is wrong on its own, which is what makes the asymmetry worth pinning. An
/// unread document has no length to report — the API answers `HEAD` with `400`, so there is
/// no way to learn one without producing the whole thing — so the placeholder stands, and
/// [`GdriveFs::read_at`] pads whatever it declines to serve out to that length. Once
/// something has produced the JSON, the same call answers what the document actually is.
///
/// And the number outlives the bytes it was measured from. It used to live inside the
/// render cache and went away with the JSON, so a listing after that reported the
/// placeholder again for a file nothing had changed: against a live account, 3.4 MB, then
/// 64 MiB, then 3.4 MB, on nothing but cache state. Keeping it apart costs a `u64` and a
/// timestamp.
#[tokio::test]
async fn a_documents_length_is_a_placeholder_until_it_is_read_and_then_keeps() {
    let mock = start_with_document(
        json!([row(
            "notes",
            "D1",
            "application/vnd.google-apps.document",
            None
        )]),
        HashMap::new(),
        Some(40_000),
    )
    .await;
    let fs = mounted(&mock.config());
    let dir = Path::new("/My Drive");
    let path = dir.join("notes.gdoc.json");

    // A listing cannot know either, so it offers the same placeholder — and names the
    // entry for what it serves rather than what Drive calls it.
    let listed = fs.list(dir).await.unwrap();
    assert_eq!(listed[0].name, "notes.gdoc.json");
    assert_eq!(size_of(&listed[0]), UNKNOWN_LENGTH_SIZE);
    assert_eq!(fs.stat(&path).await.unwrap().size, UNKNOWN_LENGTH_SIZE);
    assert_eq!(fs.lengths_remembered().await, 0, "nothing measured yet");

    // Producing it is what makes a length exist, so the same call answers differently.
    let real = fs.read_window(&path, None).await.unwrap().len() as u64;
    assert!(
        real < UNKNOWN_LENGTH_SIZE,
        "the document is far shorter than the placeholder claims"
    );
    assert_eq!(fs.stat(&path).await.unwrap().size, real, "read, so known");

    // The bytes go — the slot takes the next file, or the TTL lapses — and the length
    // stays, without producing the document a second time to recover it.
    fs.forget_rendered_for_test().await;
    assert!(fs.held_slot().await.is_none(), "the JSON is gone");
    mock.reset();
    assert_eq!(
        fs.stat(&path).await.unwrap().size,
        real,
        "and the length is still the length"
    );
    assert!(
        !mock.asked_for("/documents/D1"),
        "a remembered length is not a second render"
    );
}

/// The padding is spaces broken into lines, because a line-oriented tool pays per line.
///
/// It was newlines throughout, on the reasoning that empty lines keep `grep` cheap where
/// one enormous line would not. Measured on a 64 MiB tail, that is backwards by an order
/// of magnitude — jq 10.98s against 0.53s, grep 3.95s against 0.15s, sed 6.18s against
/// 0.03s. What the all-spaces form gives up is the shape of the tail: one 64 MB line,
/// which a `readline` hands over as one 64 MB string. A newline every `PAD_LINE` bytes
/// keeps the speed and the shape both.
#[tokio::test]
async fn the_padding_is_lines_of_spaces_and_not_a_run_of_newlines() {
    const PAD: usize = 4096;
    let mock = start_with_document(
        json!([row(
            "notes",
            "D1",
            "application/vnd.google-apps.document",
            None
        )]),
        HashMap::new(),
        Some(PAD),
    )
    .await;
    let fs = mounted(&mock.config());
    let path = Path::new("/My Drive/notes.gdoc.json");

    // One window well past the JSON, read the way the kernel reads: a fixed buffer at an
    // offset that does not divide the line length, so a seam falls inside it.
    let at = 1_000_003u64;
    let mut buf = vec![0u8; 64 * 1024];
    let n = fs.read_at(path, &mut buf, at).await.unwrap();
    assert_eq!(n, buf.len(), "well inside the claimed length");

    let newlines = buf.iter().filter(|b| **b == b'\n').count();
    let spaces = buf.iter().filter(|b| **b == b' ').count();
    assert_eq!(
        newlines + spaces,
        buf.len(),
        "the tail is whitespace and nothing else"
    );
    // Not a run of newlines: that is the shape whose cost was measured.
    assert_eq!(
        newlines,
        buf.len() / PAD_LINE as usize,
        "one newline per line, no more"
    );

    // Placed by absolute offset, so two windows meeting mid-line neither double a
    // newline nor drop one. Read the same span again in two halves and compare.
    let mut a = vec![0u8; 40_000];
    let mut b = vec![0u8; 25_536];
    fs.read_at(path, &mut a, at).await.unwrap();
    fs.read_at(path, &mut b, at + 40_000).await.unwrap();
    let mut joined = a;
    joined.extend_from_slice(&b);
    assert_eq!(joined, buf, "the seam does not move a newline");
}

/// The span the placeholder claims but the JSON does not fill gets whitespace, so a
/// document that is read in one go is still a document.
///
/// This is the whole point of padding it here rather than leaving the byte to the
/// kernel. Measured on a live mount before the change: a 1,574,113-byte deck read back
/// as 8,388,608 bytes whose last 6,814,495 were `0x00`, and `json.load` raised
/// `Expecting value` at the seam instead of skipping it. JSON ignores the whitespace
/// after a value and does not ignore a NUL, so the filler decides whether the read is
/// usable — and it costs nothing, being the same bytes either way.
#[tokio::test]
async fn a_document_is_padded_out_with_whitespace_and_not_with_zeros() {
    const PAD: usize = 4096;
    let mock = start_with_document(
        json!([row(
            "notes",
            "D1",
            "application/vnd.google-apps.document",
            None
        )]),
        HashMap::new(),
        Some(PAD),
    )
    .await;
    let fs = mounted(&mock.config());
    let path = Path::new("/My Drive/notes.gdoc.json");

    // Read it the way a reader that trusted `stat` reads it: to the claimed end.
    let claimed = fs.stat(path).await.unwrap().size;
    assert_eq!(claimed, UNKNOWN_LENGTH_SIZE);
    let mut whole = vec![0u8; claimed as usize];
    let mut at = 0usize;
    while at < whole.len() {
        let n = fs.read_at(path, &mut whole[at..], at as u64).await.unwrap();
        assert_ne!(
            n, 0,
            "a read inside the claimed length never says end of file"
        );
        at += n;
    }

    let json_len = serde_json::from_slice::<Value>(&whole[..])
        .map(|_| ())
        .map(|()| {
            whole
                .iter()
                .rposition(|b| !b.is_ascii_whitespace())
                .unwrap()
                + 1
        })
        .expect("the whole claimed length parses as one JSON document");
    assert!(
        json_len < claimed as usize,
        "the JSON is shorter than the claim"
    );
    assert!(
        whole[json_len..].iter().all(|b| b.is_ascii_whitespace()),
        "everything past the JSON is whitespace, and none of it is zero"
    );
}

/// A blob is bytes, so its short read stays short. Padding one would corrupt it, and
/// nothing needs padding: Drive sizes blobs exactly, so nobody reads past their end.
#[tokio::test]
async fn a_blob_is_never_padded() {
    const LEN: usize = 5000;
    let body: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
    let mock = start(
        json!([row("data", "B1", "application/octet-stream", Some("5000"))]),
        HashMap::from([("B1".to_string(), body.clone())]),
    )
    .await;
    let fs = mounted(&mock.config());
    let path = Path::new("/My Drive/data");
    assert_eq!(fs.stat(path).await.unwrap().size, LEN as u64);

    // A window straddling the end: the real bytes, and then the end, with nothing added.
    let mut buf = vec![0xAAu8; 64 * 1024];
    let n = fs
        .read_at(path, &mut buf, (LEN - 100) as u64)
        .await
        .unwrap();
    assert_eq!(n, 100, "a blob stops at its own end");
    assert_eq!(&buf[..100], &body[LEN - 100..]);
}

/// A document over the ceiling must stop being read, not be read and then refused.
///
/// None of these endpoints declares a length — Docs, Slides and Sheets all answer as
/// an HTTP/2 stream — so a guard that checks `Content-Length` first and buffers second
/// never fires, and the memory it exists to bound is already spent by the time it
/// looks. Measured here by counting what the server managed to write.
#[tokio::test]
async fn an_oversized_document_stops_being_read() {
    const PAD: usize = 96 * 1024 * 1024; // over the 64 MiB document ceiling
    let mock = start_with_document(
        json!([row(
            "huge",
            "D1",
            "application/vnd.google-apps.document",
            None
        )]),
        HashMap::new(),
        Some(PAD),
    )
    .await;
    let fs = mounted(&mock.config());
    let path = Path::new("/My Drive/huge.gdoc.json");
    // A document past the ceiling still stats: nothing renders it to find out.
    fs.stat(path).await.unwrap();

    assert!(
        fs.read_window(path, Some(0..256 * 1024)).await.is_err(),
        "a document past the ceiling is refused"
    );
    let sent = mock.bytes_sent();
    assert!(
        sent < PAD as u64 / 2,
        "the server should have been cut off early, but wrote {} MB of {} MB",
        sent / (1 << 20),
        PAD / (1 << 20)
    );
}

/// What the listing cache keeps, and what it refuses to keep.
///
/// Two failures, both of which read as "the tree is smaller than it is".
///
/// A shared-drive listing is best-effort, so its failure was swallowed: the root came back
/// with two sections, nothing said why, and that reduced root was cached — so a retry
/// inside the TTL made no attempt at all. It also sat on the full retry ladder, which put
/// half a minute of backoff in front of the first `ls` of a mount.
///
/// And the cache itself was a high-water mark: one listing per folder ever visited, held
/// for the life of the mount long after its TTL made it unusable, against a corpus with a
/// folder that lists 10,000 entries.
#[tokio::test]
async fn the_listing_cache_keeps_the_fresh_and_refuses_the_failed() {
    let mock = start_full(
        json!([row("a.txt", "F1", "text/plain", Some("3"))]),
        HashMap::new(),
        None,
        Some(500),
        None,
    )
    .await;
    let fs = mounted(&mock.config());
    let root = Path::new("/");
    let drives_attempts = |m: &Mock| {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.target.contains("/drives"))
            .count()
    };

    let t0 = std::time::Instant::now();
    let first = fs.list(root).await.unwrap();
    let elapsed = t0.elapsed();
    assert_eq!(
        first.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
        vec!["My Drive".to_string(), "Shared with me".to_string()]
    );
    assert_eq!(
        drives_attempts(&mock),
        1,
        "one attempt: a best-effort listing does not walk the retry ladder"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the first listing of a mount must not sit through backoff: {elapsed:?}"
    );

    // An incomplete root is not written down, so the *same* mount asks again rather than
    // serving the reduced answer for the TTL. This is the assertion that distinguishes
    // "not cached" from "cached and it happens to be a new instance".
    mock.reset();
    let again = fs.list(root).await.unwrap();
    assert_eq!(again.len(), first.len());
    assert_eq!(
        drives_attempts(&mock),
        1,
        "the second listing made its own attempt, so the first was not kept"
    );

    // And a fresh mount does not inherit it either.
    let fresh = mounted(&mock.config());
    mock.reset();
    let _ = fresh.list(root).await.unwrap();
    assert_eq!(drives_attempts(&mock), 1, "nor across mounts");

    // And what has aged out is dropped on the way in.
    //
    // A second mock, because the first half's whole point is that a root built from a
    // failed `drives.list` is *not* cached — so nothing accumulates there to age out.
    let ok = start(
        json!([row("a.txt", "F1", "text/plain", Some("3"))]),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&ok.config());
    let _ = fs.list(root).await.unwrap();
    let _ = fs.list(Path::new("/My Drive")).await.unwrap();
    assert!(fs.listings_retained().await >= 2, "the root and one folder");

    // Two survive, because resolving `/Shared with me` re-lists the root on the way to it
    // and both of those are fresh.
    fs.age_listings_for_test().await;
    let _ = fs.list(Path::new("/Shared with me")).await.unwrap();
    assert_eq!(
        fs.listings_retained().await,
        2,
        "expired listings are dropped, the fresh ones kept"
    );
}

/// One slot holds what is being read, whatever it is.
///
/// A document and a blob's span were two caches with the same sentence under each, and a
/// map under a byte budget was buying what one slot already gives — a reader works through
/// one file at a time, so what the next window needs is what the last one had. The budget
/// cost an eviction loop and a second ceiling's worth of memory to hold at once.
///
/// So the second document displaces the first, and a blob's span displaces a document.
/// Going back costs producing it again, which is what it cost to produce the first time.
#[tokio::test]
async fn one_slot_holds_whatever_is_being_read() {
    const PAD: usize = 200 * 1024;
    let blob = vec![b'z'; 4 * 1024 * 1024];
    let mock = start_with_document(
        json!([
            row("first", "D1", "application/vnd.google-apps.document", None),
            row("second", "D2", "application/vnd.google-apps.document", None),
            row(
                "big.pdf",
                "P1",
                "application/pdf",
                Some(&blob.len().to_string())
            ),
        ]),
        HashMap::from([("P1".to_string(), blob)]),
        Some(PAD),
    )
    .await;
    let fs = mounted(&mock.config());
    let dir = Path::new("/My Drive");
    fs.list(dir).await.unwrap();
    let first = dir.join("first.gdoc.json");
    let second = dir.join("second.gdoc.json");
    let pdf = dir.join("big.pdf");

    let a = fs.read_window(&first, None).await.unwrap();
    assert_eq!(
        fs.held_slot().await,
        Some(("D1".to_string(), a.len() as u64)),
        "the one just read"
    );

    // Chunks of the same document come out of the slot rather than a second render.
    mock.reset();
    for at in [0u64, 64 * 1024, 128 * 1024] {
        fs.read_window(&first, Some(at..at + 64 * 1024))
            .await
            .unwrap();
    }
    assert!(
        !mock.asked_for("/documents/D1"),
        "a chunked read of one document is one render"
    );

    // A second document displaces the first.
    let b = fs.read_window(&second, None).await.unwrap();
    assert_eq!(
        fs.held_slot().await,
        Some(("D2".to_string(), b.len() as u64)),
        "the second displaced the first"
    );

    // And a blob's span displaces a document, which is the same slot doing the same job.
    fs.read_window(&pdf, Some(0..64 * 1024)).await.unwrap();
    assert_eq!(
        fs.held_slot().await.map(|(id, _)| id),
        Some("P1".to_string()),
        "a blob's span took the slot from the document"
    );

    // Going back produces it again rather than serving something stale.
    mock.reset();
    let again = fs.read_window(&first, None).await.unwrap();
    assert_eq!(again, a);
    assert!(
        mock.asked_for("/documents/D1"),
        "the displaced document was produced again"
    );
}

/// Each service is reached at its own origin, and only its own.
///
/// A single `base_url` could not express this: it stood in for every host, with the
/// intermediate path (`/drive`, `/sheets`) chosen by the client rather than the
/// deployment, so a gateway serving one service somewhere else could not be pointed
/// at. Two listeners here, on paths neither Google nor our own mock uses.
#[tokio::test]
async fn one_service_can_move_without_moving_the_others() {
    let sheet = row(
        "budget",
        "S1",
        "application/vnd.google-apps.spreadsheet",
        None,
    );
    // Gateway A: Drive and the token endpoint, Drive on a path of its own choosing.
    let a = start(json!([sheet]), HashMap::new()).await;
    // Gateway B: Sheets only, on a different port.
    let b = start(json!([]), HashMap::new()).await;

    let fs = mounted(&GdriveConfig {
        client_id: "cid".into(),
        client_secret: "cs".into(),
        refresh_token: "rt".into(),
        origins: GdriveOrigins {
            drive: Some(format!("{}/drive", a.addr)),
            oauth: Some(format!("{}/oauth2", a.addr)),
            sheets: Some(format!("{}/sheets", b.addr)),
            ..Default::default()
        },
    });

    let listed = fs.list(Path::new("/My Drive")).await.unwrap();
    assert_eq!(listed[0].name, "budget.gsheet.json");

    // The listing came from A; the workbook has to come from B.
    let path = Path::new("/My Drive/budget.gsheet.json");
    fs.stat(path).await.unwrap();
    let _ = fs.read_window(path, None).await;

    let hit = |m: &Mock, needle: &str| {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.target.contains(needle))
            .count()
    };
    assert!(hit(&a, "/drive/v3/files") > 0, "A served the listing");
    assert!(hit(&a, "/oauth2/token") > 0, "A served the token");
    assert!(
        hit(&b, "/sheets/v4/spreadsheets/S1") > 0,
        "B served the workbook, so the override reached it"
    );
    assert_eq!(hit(&b, "/drive/v3"), 0, "and B was never asked for Drive");
    assert_eq!(
        hit(&a, "/sheets/v4"),
        0,
        "nor A for Sheets: one origin moving does not move the rest"
    );
}

/// The same window, asked backwards, through the pair a mount actually uses.
///
/// It has to be a large file: a small one is cached whole and sliced by the wrapper,
/// whose own slicing clamps. Past the cacheable limit the range goes down to the
/// provider, where the arithmetic deciding whether to slice ran `end - start` on it
/// (underflow) and the slice indexed `data[4..2]`. Neither errors; both abort the
/// process, and a filesystem read is where a client's range arrives.
#[tokio::test]
async fn a_backwards_window_does_not_take_the_process_down() {
    const REAL: usize = 10 * 1024 * 1024;
    let mock = start(
        json!([row(
            "big.bin",
            "B1",
            "application/octet-stream",
            Some("10485760")
        )]),
        HashMap::from([("B1".to_string(), vec![b'z'; REAL])]),
    )
    .await;
    let fs = mounted(&mock.config());
    let file = Path::new("/My Drive/big.bin");

    // Built from values, since a literal backwards range is a lint — but one arriving
    // from a client is just two numbers.
    for (start, end) in [(3_000_000u64, 1_000_000u64), (99_999_999, 10), (4096, 4096)] {
        let got = fs
            .read_window(file, Some(start..end))
            .await
            .expect("a backwards window answers instead of panicking");
        assert!(got.is_empty(), "{start}..{end} should read nothing");
    }
    // And a forwards one still works.
    let got = fs.read_window(file, Some(0..512)).await.unwrap();
    assert_eq!(got.len(), 512);
}

/// What a fetch costs, in every case that changes the answer.
///
/// The kernel asks in 64 KiB windows and that is not ours to choose; sending each one down
/// as its own ranged request is what made a 641 MB archive take two and a half hours. But a
/// span is not free either — 0.90 s for a window against 5.63 s for 64 MiB — and the tools
/// that read a file's head and stop would pay all of it for one buffer. So the size of a
/// fetch follows what the last one did: the first read of a file, and any jump away from
/// where the last span ended, takes the first span; a read carrying on from that end takes
/// a read span.
///
/// One fixture answers all of it, including the two degenerate cases. A span that runs off
/// the end comes back short and is marked `to_eof`, which is what serves the tail of a file
/// smaller than a span without fetching again. And a zero-length window is not a read at
/// all — it once fell through to the arm that sends no `Range`, so `read_bytes(0)` pulled
/// 20 MB to answer with an empty vector.
#[tokio::test]
async fn what_a_fetch_costs() {
    const REAL: usize = 10 * 1024 * 1024;
    const SPAN: u64 = 64 * 1024 * 1024;
    const SPAN_1: u64 = 8 * 1024 * 1024;
    const CHUNK: u64 = 64 * 1024;
    let mock = start(
        json!([row(
            "big.bin",
            "B1",
            "application/octet-stream",
            Some(&REAL.to_string())
        )]),
        HashMap::from([("B1".to_string(), vec![b'z'; REAL])]),
    )
    .await;
    let fs = mounted(&mock.config());
    let file = Path::new("/My Drive/big.bin");

    // The listing states a blob's length, so `stat` resolves it without asking. Drive
    // carries `size` on every non-native file — measured, all 182 of one account's.
    let listed = fs.list(Path::new("/My Drive")).await.unwrap();
    assert_eq!(size_of(&listed[0]), REAL as u64, "the listing carries it");
    mock.reset();
    assert_eq!(fs.stat(file).await.unwrap().size, REAL as u64);
    assert!(mock.media_ranges().is_empty(), "and stat spends no request");

    // An empty window is not a read. Before anything is held, because a span in the slot
    // would cover the window and the guard would not show.
    mock.reset();
    assert!(
        fs.read_window(file, Some(1024..1024))
            .await
            .unwrap()
            .is_empty(),
        "an empty window is empty"
    );
    assert!(mock.media_ranges().is_empty(), "and asks for nothing");
    assert_eq!(mock.bytes_sent(), 0, "and moves nothing");

    // What `file` and a `grep` that abandons a binary after one buffer do. The first span
    // rather than the window, because NFS fires read-ahead the moment a file is touched
    // and every window of it looks like a walk.
    mock.reset();
    assert_eq!(
        fs.read_window(file, Some(0..4096)).await.unwrap().len(),
        4096
    );
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!("bytes=0-{}", SPAN_1 - 1))],
        "the first fetch is the first span, not the window and not the whole file"
    );

    // Every window inside it is free, read-ahead included.
    mock.reset();
    for i in 1..16u64 {
        let w = fs
            .read_window(file, Some(i * CHUNK..(i + 1) * CHUNK))
            .await
            .unwrap();
        assert_eq!(w.len() as u64, CHUNK, "window {i}");
    }
    assert!(
        mock.media_ranges().is_empty(),
        "the first span already had them"
    );

    // Carrying on from the end of it is a walk, and a walk gets a read span. The window
    // straddles the boundary, so this also says no read comes back short of what it asked
    // for — which `read_at` would report to the kernel as the end of the file.
    mock.reset();
    let at = SPAN_1 - CHUNK / 2;
    let over = fs.read_window(file, Some(at..at + CHUNK)).await.unwrap();
    assert_eq!(
        over.len() as u64,
        CHUNK,
        "a window across the span boundary"
    );
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!("bytes={}-{}", at, at + SPAN - 1))],
        "a read span, beginning where the reader asked rather than on a fixed boundary"
    );

    // That span ran off the end, so it came back short. The rest of the file is inside it
    // and costs nothing — which is also why a file smaller than one span is fetched once
    // however the kernel chops it up.
    mock.reset();
    let tail_at = REAL as u64 - CHUNK / 2;
    let tail = fs
        .read_window(file, Some(tail_at..tail_at + CHUNK))
        .await
        .unwrap();
    assert_eq!(
        tail.len() as u64,
        CHUNK / 2,
        "short because the file ends, not because the span did"
    );
    assert!(
        mock.media_ranges().is_empty(),
        "a span that reached the end serves everything up to it"
    );

    // A jump away from it is not a walk, so it pays for a first span again rather than
    // pulling 64 MiB to answer 4 KiB somewhere new.
    mock.reset();
    assert_eq!(
        fs.read_window(file, Some(1024..1024 + 4096))
            .await
            .unwrap()
            .len(),
        4096
    );
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!("bytes=1024-{}", 1024 + SPAN_1 - 1))],
        "a read that does not continue the last one is not a walk"
    );
}
