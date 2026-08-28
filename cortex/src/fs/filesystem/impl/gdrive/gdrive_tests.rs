use std::io;
use std::path::{Path, PathBuf};
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

/// What one Drive row becomes on the mount. A file with bytes keeps its own
/// name and size; a native doc has no bytes, so its conversion takes the name
/// with `.txt` and lists as 0 until read; a native with nothing to convert is
/// absent, because a name that cannot be read is worse than no name.
#[test]
fn a_row_becomes_the_thing_it_can_serve() {
    let entry = |name: &str, mime: &str| child_from_file(&file_row(name, "id1", mime));

    // Real bytes: plain name, Drive's own size, ranged reads.
    let pdf = entry("report.pdf", "application/pdf").unwrap();
    assert_eq!(pdf.vfs_name, "report.pdf");
    assert_eq!(pdf.serves, Serves::Original);
    assert_eq!(entry_size(&pdf), 47065, "Drive's reported size");
    // Already text — nothing extra needed, the file *is* its text.
    assert_eq!(
        entry("notes.md", "text/markdown").unwrap().vfs_name,
        "notes.md"
    );

    // Docs-editors types: one entry, the document's own API JSON. The suffix
    // says which kind it is, since the Drive name carries no extension.
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
        let c = entry("Q3 Plan", mime).unwrap();
        assert_eq!(c.vfs_name, format!("Q3 Plan{suffix}"), "{mime}");
        assert_eq!(c.serves, Serves::Native(api), "{mime}");
        // Its length is only known once the API has answered.
        assert_eq!(entry_size(&c), UNKNOWN_LENGTH_SIZE, "{mime}");
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
}

/// Non-ASCII names must survive the whole path — the entry name, and the size
/// in *bytes*. Sizing in characters is the trap: a Korean or emoji name is
/// longer in bytes than in chars, and a short size truncates reads.
#[test]
fn non_ascii_names_survive_and_sizes_are_drives_own() {
    for name in [
        "분기 보고서.pdf",
        "日本語のファイル",
        "Ελληνικά έγγραφο",
        "мой документ",
        "مستند عربي",
        "party 🎉 notes.txt",
    ] {
        let c = child_from_file(&file_row(name, "id1", "application/pdf")).unwrap();
        assert_eq!(c.vfs_name, name, "entry name is the Drive name");
        // Drive's own byte count, carried through untouched.
        assert_eq!(entry_size(&c), 47065, "{name}");
        assert!(
            name.len() > name.chars().count(),
            "{name}: this case should actually be multi-byte"
        );
    }

    // A native doc's name gains only the suffix, whatever the script.
    let doc = child_from_file(&file_row(
        "분기 보고서",
        "id1",
        "application/vnd.google-apps.document",
    ))
    .unwrap();
    assert_eq!(doc.vfs_name, "분기 보고서.gdoc.json");
}

/// A row Drive reported no size for still lists, with the placeholder rather
/// than 0 — under the guest's `direct_io` mount a 0 was measured to clamp reads
/// to nothing, and a search tool skips a file it is told is empty.
#[test]
fn an_unknown_size_is_a_placeholder_never_zero() {
    let mut row = file_row("mystery.bin", "id1", "application/octet-stream");
    row.as_object_mut().unwrap().remove("size");
    let c = child_from_file(&row).unwrap();
    assert_eq!(c.serves, Serves::Original);
    assert_eq!(entry_size(&c), UNKNOWN_LENGTH_SIZE);
    // Never 0 (that reads as empty), and at or under the content cache's
    // per-object limit, so the first read of a document replaces the
    // placeholder with its exact length for every later listing.
    const _: () = assert!(UNKNOWN_LENGTH_SIZE > 0);
    const _: () = assert!(UNKNOWN_LENGTH_SIZE <= 8 * 1024 * 1024);
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
fn disambiguate_keeps_a_name_findable_by_its_extension() {
    let mk = |n: &str, serves: Serves| Child {
        vfs_name: n.to_string(),
        id: "x".into(),
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
    let mut children = vec![
        // Three spreadsheets of the same Drive name: the number has to land
        // before `.gsheet.json` or a `**/*.gsheet.json` search loses two of them.
        mk("report.gsheet.json", Serves::Native(NativeApi::Sheet)),
        mk("report.gsheet.json", Serves::Native(NativeApi::Sheet)),
        mk("report.gsheet.json", Serves::Native(NativeApi::Sheet)),
        // A plain file keeps its own extension.
        mk("photo.jpeg", Serves::Original),
        mk("photo.jpeg", Serves::Original),
        // No extension to preserve, and a folder is not renamed around a dot.
        mk("notes", Serves::Original),
        mk("notes", Serves::Original),
        mk("v1.2", Serves::Nothing),
        mk("v1.2", Serves::Nothing),
    ];
    disambiguate(&mut children);
    let names: Vec<&str> = children.iter().map(|c| c.vfs_name.as_str()).collect();
    assert_eq!(
        names,
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

/// The budget bounds the file that gets served, so it has to be measured in the
/// form that gets served: indenting a grid of short cells costs half again its
/// compact size (1.66x measured here), and a budget checked against the compact
/// form quietly allows that much more.
#[test]
fn a_tabs_cost_is_measured_as_it_will_be_written() {
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

/// A search reports what the index found, including the types this mount cannot
/// serve — a Form has no readable form, but "the phrase is in this Form" is still
/// the answer to the question.
#[tokio::test]
async fn a_search_hit_the_mount_cannot_serve_is_still_reported() {
    let form = file_row("설문지", "f1", "application/vnd.google-apps.form");
    assert!(
        child_from_file(&form).is_none(),
        "a Form has no entry in the tree"
    );
    // The command's shaping of a hit keeps the name and id either way; the note is
    // what tells them apart, and it says why in the line itself.
    let served = file_row("보고서.pdf", "p1", "application/pdf");
    for (row, listed) in [(&form, false), (&served, true)] {
        let child = child_from_file(row);
        assert_eq!(child.is_some(), listed);
        let name = child
            .as_ref()
            .map(|c| c.vfs_name.clone())
            .or_else(|| row.get("name").and_then(|n| n.as_str()).map(str::to_string));
        assert!(name.is_some(), "a hit always has a name to report");
    }
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

/// Values are paired by the sheet they name, not by their position in the reply.
/// A sheet the request had to skip — one with no title — used to consume the next
/// sheet's values and shift the rest, which is the same wrong-cells outcome
/// `quote_a1` exists to prevent, reached from the other side.
#[test]
fn a_skipped_sheet_does_not_shift_everyone_elses_values() {
    let mut wb = workbook(&[Some("Alpha"), None, Some("Gamma")]);
    // The request only asked for the titled sheets.
    let asked = vec!["Alpha".to_string(), "Gamma".to_string()];
    fold_values(
        &mut wb,
        &batch(&[("Alpha!A1", "ALPHA-CELL"), ("Gamma!A1", "GAMMA-CELL")]),
        &asked,
    );
    assert_eq!(values_of(&wb, 0).as_deref(), Some("ALPHA-CELL"));
    assert_eq!(values_of(&wb, 2).as_deref(), Some("GAMMA-CELL"));
    assert_eq!(values_of(&wb, 1), None, "the titleless sheet gets nothing");
    assert!(
        omitted_reason(&wb, 1).is_some_and(|r| r.contains("no title")),
        "and says why"
    );
}

/// Past the cap no request was made, so the tail has no values — and used to have
/// no explanation either, the one silent omission in this file.
#[test]
fn a_tab_past_the_cap_says_it_was_never_asked_for() {
    let titles: Vec<String> = (0..MAX_TABS + 2).map(|i| format!("T{i}")).collect();
    let mut wb = workbook(&titles.iter().map(|t| Some(t.as_str())).collect::<Vec<_>>());
    let asked = titles[..MAX_TABS].to_vec();
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
}

/// One oversized tab used to zero the budget, so every later tab was dropped
/// however small. The budget is spent tab by tab, which is what the constant says.
#[test]
fn an_oversized_tab_does_not_spend_the_rest_of_the_budget() {
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
}

/// The reply names its sheet in A1 notation, which is what the pairing reads.
#[test]
fn a_returned_range_names_its_sheet() {
    assert_eq!(range_title("Sheet1!A1:Z1000"), "Sheet1");
    assert_eq!(range_title("'메인화면'!A1:Z968"), "메인화면");
    // A quoted title can hold the characters that would otherwise confuse this.
    assert_eq!(range_title("'Sheet1!B2'!A1:Z10"), "Sheet1!B2");
    assert_eq!(range_title("'it''s'!A1"), "it's");
    assert_eq!(range_title("Sheet1"), "Sheet1");
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

#[test]
fn split_last_shapes() {
    assert_eq!(split_last("/a/b"), ("/a".to_string(), "b".to_string()));
    assert_eq!(split_last("/a"), ("/".to_string(), "a".to_string()));
    assert_eq!(
        split_last("/My Drive/x.json"),
        ("/My Drive".to_string(), "x.json".to_string())
    );
}

/// Config for the enterprise-mock integration test. The mock hands the refresh
/// token straight back as the bearer token, so any user token from its
/// `tokens.yaml` (or the admin token) works with the normal OAuth flow.
///
/// The token var is deliberately not `GOOGLE_REFRESH_TOKEN`: sharing it with
/// [`live_config`] means one shell with both set sends a real Google token to the
/// mock, which then fails for a reason that looks like a code bug.
fn mock_config() -> Option<GdriveConfig> {
    Some(GdriveConfig {
        client_id: "mock".into(),
        client_secret: "mock".into(),
        refresh_token: std::env::var("GOOGLE_MOCK_TOKEN")
            .unwrap_or_else(|_| "admin-service-token".into()),
        origins: GdriveOrigins::behind(&std::env::var("GOOGLE_API_BASE_URL").ok()?),
    })
}

/// Tree round-trip against an enterprise-mock, local or hosted. Ignored by
/// default; run with:
///
///   # local: python -m app.importer.byo \
///   #   examples/bring-your-own-corpus/sample_corpus.jsonl
///   #        then python -m uvicorn app.main:app --port 8000
///   GOOGLE_API_BASE_URL=http://localhost:8000 \
///     [GOOGLE_MOCK_TOKEN=…] cargo test -p workspace gdrive_mock -- --ignored --nocapture
///
/// The walk is bounded so the same test runs against a five-file sample
/// corpus and a 25k-document hosted one; a real corpus also spans several
/// listing pages, which exercises the accessor's pagination for free.
#[tokio::test]
#[ignore = "requires a running enterprise-mock (GOOGLE_API_BASE_URL)"]
async fn gdrive_mock_tree_and_reads() {
    /// Bounds on the walk — enough to cross a page boundary on a real
    /// corpus, small enough to stay quick on a tiny one.
    const WALK_DIRS: usize = 12;
    const WALK_FILES: usize = 200;

    let Some(cfg) = mock_config() else {
        eprintln!("set GOOGLE_API_BASE_URL (e.g. http://localhost:8000) to run");
        return;
    };
    let r = GdriveFs::new(&cfg).unwrap();

    let root = r.list(Path::new("/")).await.expect("root readdir");
    eprintln!(
        "root: {:?}",
        root.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    assert!(root.iter().any(|e| e.name == MY_DRIVE_NAME));
    assert!(root.iter().any(|e| e.name == SHARED_WITH_ME_NAME));

    // A path that cannot exist must be NotFound (from the parent listing),
    // not a 500 — the WebDAV layer turns this into a 404.
    for bogus in [
        format!("/{MY_DRIVE_NAME}/definitely-not-here-9f3c.bin"),
        "/NoSuchSection".to_string(),
        format!("/{MY_DRIVE_NAME}/nope/deeper.bin"),
    ] {
        let p = Path::new(&bogus);
        assert!(
            matches!(r.stat(p).await, Err(e) if e.kind() == io::ErrorKind::NotFound),
            "stat {bogus} should be NotFound"
        );
        assert!(
            matches!(r.read_window(p, None).await, Err(e) if e.kind() == io::ErrorKind::NotFound),
            "read {bogus} should be NotFound"
        );
    }

    // Walk the tree (bounded). Every file reads back, and an entry that
    // reported a size must hand over exactly that many bytes — a listing
    // that lies about length truncates every reader.
    let mut queue: Vec<String> = root
        .iter()
        .filter(|e| e.kind == DirentKind::Dir)
        .map(|e| format!("/{}", e.name))
        .collect();
    let (mut files, mut dirs, mut biggest, mut converted) = (0usize, 0usize, 0usize, 0usize);
    while let Some(dir) = queue.pop() {
        if dirs >= WALK_DIRS || files >= WALK_FILES {
            eprintln!("walk bound reached ({dirs} dirs, {files} files); stopping");
            break;
        }
        dirs += 1;
        let entries = r.list(Path::new(&dir)).await.expect("folder readdir");
        biggest = biggest.max(entries.len());
        eprintln!("  {dir} -> {} entries", entries.len());
        for e in entries.iter().take(WALK_FILES.saturating_sub(files)) {
            let p = format!("{dir}/{}", e.name);
            if e.kind == DirentKind::Dir {
                queue.push(p);
                continue;
            }
            let mp = Path::new(&p);
            let bytes = r.read_window(mp, None).await.expect("read file");
            if is_native_json(e) {
                // A document's JSON: the listing could only estimate its
                // length, so just check the read produced something.
                converted += 1;
                assert!(!bytes.is_empty(), "{p}: native json was empty");
            } else {
                assert_eq!(size_of(e), bytes.len() as u64, "{p}: listed size vs read");
                let st = r.stat(mp).await.expect("stat");
                assert_eq!(st.size, size_of(e), "{p}: listing and stat disagree");
                // A ranged read returns its window, and past-EOF is empty.
                let head = r.read_window(mp, Some(0..8)).await.expect("ranged read");
                assert_eq!(head.len(), 8.min(bytes.len()), "{p}");
                assert!(
                    r.read_window(mp, Some(size_of(e)..size_of(e) + 16))
                        .await
                        .expect("past EOF")
                        .is_empty(),
                    "{p}: past EOF should be empty"
                );
            }
            files += 1;
        }
    }
    eprintln!(
        "{files} files read across {dirs} dirs ({converted} conversions); \
         biggest listing {biggest}"
    );
    assert!(files > 0, "the corpus should expose files");
    if biggest > 1000 {
        eprintln!("pagination exercised: one listing spanned {biggest} entries");
    }
}

fn live_config() -> Option<GdriveConfig> {
    Some(GdriveConfig {
        client_id: std::env::var("GOOGLE_CLIENT_ID").ok()?,
        client_secret: std::env::var("GOOGLE_CLIENT_SECRET").ok()?,
        refresh_token: std::env::var("GOOGLE_REFRESH_TOKEN").ok()?,
        origins: Default::default(),
    })
}

/// Live tree round-trip against real Google Drive: the root sections, both
/// section listings, and one ranged read per section at the length the
/// listing promised. Ignored by default; run with:
///
///   GOOGLE_CLIENT_ID=… GOOGLE_CLIENT_SECRET=… GOOGLE_REFRESH_TOKEN=… \
///     cargo test -p workspace gdrive_live -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires GOOGLE_* env + network"]
async fn gdrive_live_tree_and_reads() {
    let Some(cfg) = live_config() else {
        eprintln!("set GOOGLE_CLIENT_ID / GOOGLE_CLIENT_SECRET / GOOGLE_REFRESH_TOKEN to run");
        return;
    };
    let r = GdriveFs::new(&cfg).unwrap();

    let root = r.list(Path::new("/")).await.expect("root readdir");
    eprintln!(
        "root: {:?}",
        root.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    assert!(root.iter().any(|e| e.name == MY_DRIVE_NAME));

    for section in [MY_DRIVE_NAME, SHARED_WITH_ME_NAME] {
        let entries = r
            .list(&PathBuf::from(format!("/{section}")))
            .await
            .expect("section readdir");
        eprintln!("{section}: {} entries", entries.len());
        for e in entries.iter().take(5) {
            eprintln!("  {:>6}B {:?} {}", size_of(e), e.kind, e.name);
        }
        // Read the first file: what comes back is the file itself, at the
        // length the listing promised.
        if let Some(f) = entries
            .iter()
            .find(|e| e.kind == DirentKind::File && !is_native_json(e) && size_of(e) > 0)
        {
            let mp = PathBuf::from(format!("/{section}/{}", f.name));
            let head = r
                .read_window(&mp, Some(0..64.min(size_of(f))))
                .await
                .expect("ranged read");
            eprintln!(
                "  first file {} ({}B) head: {:?}",
                f.name,
                size_of(f),
                String::from_utf8_lossy(&head)
                    .chars()
                    .take(40)
                    .collect::<String>()
            );
            assert_eq!(head.len() as u64, 64.min(size_of(f)), "range honored");
            assert_eq!(
                r.stat(&mp).await.expect("stat").size,
                size_of(f),
                "listing and stat must agree on length"
            );
        }
    }
}
/// Live: a Docs-editors document is served as its own API's JSON, and a
/// spreadsheet's stays small — the grid it deliberately omits runs to hundreds
/// of megabytes.
#[tokio::test]
#[ignore = "requires GOOGLE_* env + network"]
async fn gdrive_live_native_json_is_served() {
    let Some(cfg) = live_config() else {
        eprintln!("set GOOGLE_CLIENT_ID / GOOGLE_CLIENT_SECRET / GOOGLE_REFRESH_TOKEN to run");
        return;
    };
    let r = GdriveFs::new(&cfg).unwrap();

    let mut seen = 0usize;
    for section in [SHARED_WITH_ME_NAME, MY_DRIVE_NAME] {
        let entries = r
            .list(&PathBuf::from(format!("/{section}")))
            .await
            .expect("section readdir");
        for suffix in [".gsheet.json", ".gdoc.json", ".gslide.json"] {
            let Some(e) = entries.iter().find(|e| e.name.ends_with(suffix)) else {
                continue;
            };
            let p = PathBuf::from(format!("/{section}/{}", e.name));
            let t0 = std::time::Instant::now();
            let bytes = r.read_window(&p, None).await.expect("read native json");
            let v: Value = serde_json::from_slice(&bytes).expect("native json parses");
            eprintln!(
                "  {} -> {} bytes in {:.2}s, top keys {:?}",
                e.name,
                bytes.len(),
                t0.elapsed().as_secs_f64(),
                v.as_object().map(|o| o.keys().take(4).collect::<Vec<_>>())
            );
            // A spreadsheet carries its cells, unless its allocated grid is
            // over the limit — then it says so instead of moving 200-349MB.
            if suffix == ".gsheet.json" {
                assert!(v.get("sheets").is_some(), "workbook shape is present");
                // `includeGridData` would put cells under sheets[].data. Nothing
                // should be taking that route — it measured 189MB on this very
                // workbook.
                assert!(
                    !v["sheets"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|s| s.get("data").is_some()),
                    "{}: cells must come from values, not the allocated grid",
                    e.name
                );
                let tabs = v["sheets"].as_array().cloned().unwrap_or_default();
                for t in &tabs {
                    eprintln!(
                        "    tab {:?}: {} rows{}",
                        t.pointer("/properties/title").and_then(|x| x.as_str()),
                        t["values"].as_array().map_or(0, |r| r.len()),
                        if t.get("valuesOmitted").is_some() {
                            " (values omitted)"
                        } else {
                            ""
                        }
                    );
                }
                assert!(
                    tabs.iter()
                        .all(|t| t.get("values").is_some() || t.get("valuesOmitted").is_some()),
                    "{}: every tab carries its values or says why not",
                    e.name
                );
                // The point of carrying values at all: a cell's text is in the
                // bytes, so a reader searching the tree finds it. Cells holding a
                // line break are excluded on purpose — JSON escapes those to
                // `\n`, so a phrase spanning one is not literally in the file.
                let a_cell = tabs
                    .iter()
                    .flat_map(|t| t["values"].as_array().cloned().unwrap_or_default())
                    .flat_map(|row| row.as_array().cloned().unwrap_or_default())
                    .find_map(|c| {
                        c.as_str()
                            .filter(|s| s.trim().len() > 3 && !s.contains(['\n', '"', '\\']))
                            .map(str::to_string)
                    })
                    .expect("some cell holds plain text");
                eprintln!("    a cell reads {a_cell:?}");
                assert!(
                    String::from_utf8_lossy(&bytes).contains(&a_cell),
                    "a cell's text is greppable in the served bytes"
                );
            }
            seen += 1;
        }
        if seen > 0 {
            break;
        }
    }
    assert!(seen > 0, "no native document found");
}

#[tokio::test]
#[ignore = "requires GOOGLE_* env + network"]
async fn gdrive_live_originals_read_by_range() {
    let Some(cfg) = live_config() else {
        eprintln!("set GOOGLE_CLIENT_ID / GOOGLE_CLIENT_SECRET / GOOGLE_REFRESH_TOKEN to run");
        return;
    };
    let r = GdriveFs::new(&cfg).unwrap();

    for section in [MY_DRIVE_NAME, SHARED_WITH_ME_NAME] {
        let entries = r
            .list(&PathBuf::from(format!("/{section}")))
            .await
            .expect("section readdir");
        // The biggest original in the section: exactly the file a full read
        // must never be needed for.
        let Some(big) = entries
            .iter()
            .filter(|e| e.kind == DirentKind::File && !is_native_json(e) && size_of(e) > 0)
            .max_by_key(|e| size_of(e))
        else {
            continue;
        };
        let p = PathBuf::from(format!("/{section}/{}", big.name));
        eprintln!(
            "{section}: largest original {} = {} bytes",
            big.name,
            size_of(big)
        );

        let t0 = std::time::Instant::now();
        let head = r.read_window(&p, Some(0..4096)).await.expect("ranged read");
        eprintln!(
            "  head 4096B -> {} bytes in {:.2}s",
            head.len(),
            t0.elapsed().as_secs_f64()
        );
        assert_eq!(head.len(), 4096.min(size_of(big) as usize), "range honored");

        // A window from the middle differs from the head — proof the offset
        // reached Drive instead of being sliced off a full download.
        if size_of(big) > 100_000 {
            let mid = r
                .read_window(&p, Some(50_000..54_096))
                .await
                .expect("mid read");
            assert_eq!(mid.len(), 4096);
            assert_ne!(mid, head, "a mid-file window is not the head");
        }

        // Past EOF is a clean empty read (Drive answers 416) — what a reader
        // walking to the end expects.
        assert!(
            r.read_window(&p, Some(size_of(big)..size_of(big) + 4096))
                .await
                .expect("read past EOF")
                .is_empty(),
            "EOF reads empty"
        );
        assert_eq!(r.stat(&p).await.expect("stat").size, size_of(big));
        return;
    }
    panic!("no original found to read");
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

/// Serve a token, one folder listing, and one blob with `Range` support. A document
/// route answers with `pad` bytes of JSON and no `Content-Length`, the way the real
/// Docs API does.
async fn start_with_document(
    listing: Value,
    blobs: HashMap<String, Vec<u8>>,
    document_pad: Option<usize>,
) -> Mock {
    start_inner(listing, blobs, document_pad).await
}

/// Serve a token, one folder listing, and one blob with `Range` support.
async fn start(listing: Value, blobs: HashMap<String, Vec<u8>>) -> Mock {
    start_inner(listing, blobs, None).await
}

async fn start_inner(
    listing: Value,
    blobs: HashMap<String, Vec<u8>>,
    document_pad: Option<usize>,
) -> Mock {
    start_full(listing, blobs, document_pad, None).await
}

/// `drives_status` answers `/drives` with that status instead of a listing.
async fn start_full(
    listing: Value,
    blobs: HashMap<String, Vec<u8>>,
    document_pad: Option<usize>,
    drives_status: Option<u16>,
) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let body_bytes = Arc::new(StdMutex::new(0u64));
    let (log, written, blobs) = (seen.clone(), body_bytes.clone(), Arc::new(blobs));
    let listing = Arc::new(listing);
    let document_pad = Arc::new(document_pad);
    let drives_status = Arc::new(drives_status);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (log, written, blobs, listing, document_pad, drives_status) = (
                log.clone(),
                written.clone(),
                blobs.clone(),
                listing.clone(),
                document_pad.clone(),
                drives_status.clone(),
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
                        None => reply(200, json!({"drives": []}).to_string().into_bytes(), None),
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

/// A file Drive listed without a `size`, read the way every caller reads: list the
/// folder, then read windows of it.
///
/// The listing can only offer a placeholder, and the danger is that the placeholder is
/// believed — `stat` has to resolve it, from one ranged byte rather than a download, or
/// `ls -l` lies and a reader stops at 8 MiB. The windows after it then come out of one
/// span, which is the point of the span: 80 MB moved to deliver 1 MB before any of this
/// was in place.
#[tokio::test]
async fn an_unsized_file_is_measured_then_read_by_spans() {
    const REAL: usize = 20 * 1024 * 1024;
    const CHUNK: u64 = 256 * 1024;
    let mock = start(
        json!([row("big.pdf", "P1", "application/pdf", None)]),
        HashMap::from([("P1".to_string(), vec![b'a'; REAL])]),
    )
    .await;
    let fs = mounted(&mock.config());
    let dir = Path::new("/My Drive");
    let file = Path::new("/My Drive/big.pdf");

    let listed = fs.list(dir).await.unwrap();
    // The placeholder, because Drive listed no size. A `Stat` has no field for saying
    // the number is a placeholder rather than a length, so the number itself is the
    // whole of what a listing can report — see `dirent_for`.
    assert_eq!(
        size_of(&listed[0]),
        UNKNOWN_LENGTH_SIZE,
        "a listing that cannot know the length reports the placeholder"
    );

    // The stat behind the listing resolves it — one ranged byte, not a download.
    mock.reset();
    let st = fs.stat(file).await.unwrap();
    assert_eq!(st.size, REAL as u64, "stat answers the real length");
    assert_eq!(
        mock.media_ranges(),
        vec![Some("bytes=0-0".to_string())],
        "measured by asking for one byte"
    );

    // And the windows after it come out of one span rather than one request each.
    mock.reset();
    for i in 0..4u64 {
        let got = fs
            .read_window(file, Some(i * CHUNK..(i + 1) * CHUNK))
            .await
            .unwrap();
        assert_eq!(got.len() as u64, CHUNK);
    }
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!("bytes=0-{}", 8 * 1024 * 1024 - 1))],
        "one span answered all four windows"
    );
}

/// A document is the other kind of placeholder: no ranged read exists for it, so the
/// wrapper has to fetch it whole and keep it, or every window rebuilds it.
#[tokio::test]
async fn a_document_is_built_once_and_served_from_the_cache() {
    const CHUNK: u64 = 256 * 1024;
    let mock = start(
        json!([row(
            "notes",
            "D1",
            "application/vnd.google-apps.document",
            None
        )]),
        HashMap::new(),
    )
    .await;
    let fs = mounted(&mock.config());
    let listed = fs.list(Path::new("/My Drive")).await.unwrap();
    assert_eq!(listed[0].name, "notes.gdoc.json");
    assert_eq!(size_of(&listed[0]), UNKNOWN_LENGTH_SIZE);

    // The mock has no Docs endpoint, so a read fails — what matters is that the
    // listing already refused to call the placeholder a length, which is what keeps
    // `whole_or_small` from treating it as a small, cacheable object.
    let st = fs
        .stat(Path::new("/My Drive/notes.gdoc.json"))
        .await
        .unwrap();
    assert_eq!(
        st.size, UNKNOWN_LENGTH_SIZE,
        "a document has no length until it is built"
    );
    assert!(
        fs.read_window(Path::new("/My Drive/notes.gdoc.json"), Some(0..CHUNK))
            .await
            .is_err(),
        "the mock serves no document API"
    );
}

/// The placeholder is what an *unread* document shows, so one document answers `stat`
/// with two different numbers depending on whether anything has read it yet.
///
/// This is the asymmetry a reader trips on, and it is worth a test of its own because
/// neither half is wrong on its own. A first read is told 8 MiB and the kernel pads
/// whatever [`GdriveFs::read_at`] declines to serve out to that length; a second read
/// inside [`DIR_TTL`] is told the truth and stops at the end. So a parser that chokes
/// on the padding succeeds when it is simply run again, which reads as a flake rather
/// than as the length having been a placeholder.
#[tokio::test]
async fn a_document_stats_as_the_placeholder_until_it_is_read() {
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

    // Nothing has rendered it, so the only number there is to report is the placeholder.
    assert_eq!(
        fs.stat(path).await.unwrap().size,
        UNKNOWN_LENGTH_SIZE,
        "an unread document has no length to report"
    );

    let body = fs.read_window(path, Some(0..64 * 1024)).await.unwrap();
    assert!(
        (body.len() as u64) < UNKNOWN_LENGTH_SIZE,
        "the document is far shorter than the placeholder claims it is"
    );

    // Rendering it is what produces the length, so the same call now answers differently.
    assert_eq!(
        fs.stat(path).await.unwrap().size,
        body.len() as u64,
        "a document that has been read reports what it actually is"
    );
}

/// The span the placeholder claims but the JSON does not fill gets newlines, so a
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
        .map(|()| whole.iter().rposition(|b| *b != b'\n').unwrap() + 1)
        .expect("the whole claimed length parses as one JSON document");
    assert!(
        json_len < claimed as usize,
        "the JSON is shorter than the claim"
    );
    assert!(
        whole[json_len..].iter().all(|b| *b == b'\n'),
        "everything past the JSON is newline, and none of it is zero"
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

/// A shared-drive listing that fails must not become "this account has none", cached.
///
/// The call is best-effort, so its failure was swallowed: the root came back with two
/// sections, nothing said why, and the reduced root was cached for the listing TTL, so
/// a retry inside five minutes made no attempt at all. It also sat on the full retry
/// ladder, which put half a minute of backoff in front of the first `ls` of a mount.
#[tokio::test]
async fn a_failed_shared_drive_listing_is_not_cached_as_an_answer() {
    let mock = start_full(
        json!([row("a.txt", "F1", "text/plain", Some("3"))]),
        HashMap::new(),
        None,
        Some(500),
    )
    .await;
    let fs = mounted(&mock.config());
    let root = Path::new("/");

    let t0 = std::time::Instant::now();
    let first = fs.list(root).await.unwrap();
    let elapsed = t0.elapsed();
    let drives_attempts = |m: &Mock| {
        m.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.target.contains("/drives"))
            .count()
    };
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

    // The reduced root is not kept by the provider, so a fresh mount tries again
    // rather than inheriting the failure. Within one mount the wrapper's own listing
    // cache still answers for its TTL — that layer has no per-listing way to say "this
    // one is incomplete", which is what it would take to recover sooner.
    let fresh = mounted(&mock.config());
    mock.reset();
    let _ = fresh.list(root).await.unwrap();
    assert_eq!(
        drives_attempts(&mock),
        1,
        "a new mount asks again instead of inheriting a degraded root"
    );
}

/// A zero-length window asks the server for nothing.
///
/// It used to fall through to the arm that sends no `Range` at all, so `read_bytes(0)`
/// pulled the whole object and returned none of it — 20 MB to answer with an empty
/// vector.
#[tokio::test]
async fn an_empty_window_does_not_fetch_the_object() {
    const REAL: usize = 20 * 1024 * 1024;
    let mock = start(
        json!([row("big.pdf", "P1", "application/pdf", Some("20971520"))]),
        HashMap::from([("P1".to_string(), vec![b'a'; REAL])]),
    )
    .await;
    let fs = mounted(&mock.config());
    let file = Path::new("/My Drive/big.pdf");
    // Populates what a listing and a probe would, so the reset below leaves only
    // what the read itself asks for.
    fs.stat(file).await.unwrap();

    mock.reset();
    let got = fs.read_window(file, Some(1024..1024)).await.unwrap();
    assert!(got.is_empty());
    assert_eq!(
        mock.media_ranges(),
        Vec::<Option<String>>::new(),
        "no request at all"
    );
    assert_eq!(mock.bytes_sent(), 0);
}

/// The provider's own caches are high-water marks unless something prunes them.
///
/// Nothing did: one listing per folder ever visited, held for the life of the mount
/// long after its TTL made it unusable, and the corpus has a folder that lists 10,000
/// entries. The wrapper above bounds both of its equivalents.
#[tokio::test]
async fn a_listing_cache_does_not_grow_without_bound() {
    let mock = start(
        json!([row("a.txt", "F1", "text/plain", Some("3"))]),
        HashMap::new(),
    )
    .await;
    let fs = GdriveFs::new(&mock.config()).unwrap();

    // The root plus one folder listing.
    let _ = fs.list(Path::new("/")).await.unwrap();
    let _ = fs.list(Path::new("/My Drive")).await.unwrap();
    assert!(fs.listings_retained().await >= 2);

    // Age them out. The next listing drops what expired instead of keeping it for the
    // life of the mount — two survive, because resolving `/Shared with me` re-lists the
    // root on the way to it, and both of those are fresh.
    fs.age_listings_for_test().await;
    let _ = fs.list(Path::new("/Shared with me")).await.unwrap();
    assert_eq!(
        fs.listings_retained().await,
        2,
        "expired listings are dropped, the fresh ones kept"
    );
}

/// The document cache is a budget, not a high-water mark.
///
/// The TTL alone did not bound it. Nothing over the accessor's ceiling arrives, so no
/// single entry is unbounded — but the map held everything read in the last five
/// minutes, and reading is what this tree is for. A `grep -r` over a folder of documents
/// produces every one of them, and without a budget keeps every one.
///
/// The budget is the per-document ceiling, so the rule is one sentence: whatever is
/// being read is held, and nothing more is promised. Two documents that cannot both fit
/// are the case that says so.
#[tokio::test]
async fn the_document_cache_drops_the_oldest_to_fit_the_newest() {
    // Two documents, each over half the budget, so the second cannot join the first.
    const PAD: usize = 40 * 1024 * 1024;
    let mock = start_with_document(
        json!([
            row("first", "D1", "application/vnd.google-apps.document", None),
            row("second", "D2", "application/vnd.google-apps.document", None),
        ]),
        HashMap::new(),
        Some(PAD),
    )
    .await;
    let fs = mounted(&mock.config());
    let dir = Path::new("/My Drive");
    fs.list(dir).await.unwrap();

    let first = dir.join("first.gdoc.json");
    let second = dir.join("second.gdoc.json");

    let a = fs.read_window(&first, None).await.unwrap();
    assert!(
        a.len() as u64 > MAX_DOCUMENT_BYTES / 2,
        "one that cannot share"
    );
    let (n, held) = fs.rendered_held().await;
    assert_eq!(n, 1, "the one just read");
    assert!(held <= RENDERED_BUDGET);

    let b = fs.read_window(&second, None).await.unwrap();
    assert_eq!(b.len(), a.len(), "the mock pads both the same");
    let (n, held) = fs.rendered_held().await;
    assert_eq!(n, 1, "the first was dropped rather than joined");
    assert!(
        held <= RENDERED_BUDGET,
        "{held} bytes held against a {RENDERED_BUDGET} budget"
    );

    // And the one being read is the one held: reading the first again produces it again
    // rather than answering from a cache that had let it go.
    mock.reset();
    let again = fs.read_window(&first, None).await.unwrap();
    assert_eq!(again.len(), a.len());
    assert!(
        mock.asked_for("/documents/D1"),
        "the dropped document was produced again, not served stale"
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

/// A small file is fetched once, however the kernel chops the reads up.
///
/// Drive charges by the request: a download is 200 quota units whether it moves 200 KiB
/// or 20 MB, so answering eighty chunk reads with eighty ranged requests costs eighty
/// times what one span does. A file smaller than the first span is the degenerate case —
/// that span comes back short, holding all of it, and every window after is free.
#[tokio::test]
async fn a_small_file_is_fetched_once_for_every_chunk_of_it() {
    const REAL: usize = 300 * 1024;
    const CHUNK: u64 = 64 * 1024;
    let mock = start(
        json!([row(
            "small.bin",
            "S1",
            "application/octet-stream",
            Some("307200")
        )]),
        HashMap::from([("S1".to_string(), vec![b'q'; REAL])]),
    )
    .await;
    let fs = mounted(&mock.config());
    let file = Path::new("/My Drive/small.bin");
    fs.list(Path::new("/My Drive")).await.unwrap();
    mock.reset();

    let mut got = 0usize;
    for i in 0..(REAL as u64 / CHUNK) {
        let w = fs
            .read_window(file, Some(i * CHUNK..(i + 1) * CHUNK))
            .await
            .unwrap();
        assert_eq!(w.len() as u64, CHUNK, "chunk {i}");
        got += w.len();
    }
    assert_eq!(got as u64, (REAL as u64 / CHUNK) * CHUNK);

    let media = mock.media_ranges();
    assert_eq!(
        media.len(),
        1,
        "one fetch for the whole file, but the server saw {media:?}"
    );
    assert_eq!(
        media[0],
        Some(format!("bytes=0-{}", 8 * 1024 * 1024 - 1)),
        "the first span, which a file this size answers whole"
    );
}

/// A head-read costs its window; only a walk pays for a span.
///
/// The kernel asks in 64 KiB windows and that is not ours to choose; sending each one
/// down as its own ranged request is what made a 641 MB archive take two and a half
/// hours. But a span is not free either — 0.90 s for a window against 5.63 s for 64 MiB
/// — and the tools that read a file's head and stop would pay all of it for one buffer.
/// So the size of a fetch follows what the last one did: the first read of a file, and
/// any jump away from where the last span ended, takes its window; a read carrying on
/// from that end takes a span.
#[tokio::test]
async fn a_head_read_costs_its_window_and_only_a_walk_pays_for_a_span() {
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
    fs.list(Path::new("/My Drive")).await.unwrap();
    mock.reset();

    const SPAN: u64 = 64 * 1024 * 1024;
    const SPAN_1: u64 = 8 * 1024 * 1024;
    const CHUNK: u64 = 64 * 1024;

    // What `file` and a `grep` that abandons a binary after one buffer do. One span, not
    // the 10 MB behind it — and the first span rather than the window, because NFS fires
    // read-ahead the moment a file is touched and every window of it looks like a walk.
    let head = fs.read_window(file, Some(0..4096)).await.unwrap();
    assert_eq!(head.len(), 4096);
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!("bytes=0-{}", SPAN_1 - 1))],
        "the first fetch is the first span, not the window and not the whole file"
    );

    // Every window inside it is free, read-ahead included.
    mock.reset();
    for i in 1..16 {
        let w = fs
            .read_window(file, Some(i * CHUNK..(i + 1) * CHUNK))
            .await
            .unwrap();
        assert_eq!(w.len() as u64, CHUNK, "window {i}");
    }
    assert_eq!(
        mock.media_ranges(),
        Vec::<Option<String>>::new(),
        "the first span already had them"
    );

    // Carrying on from the end of it is a walk, and a walk gets a read span. The window
    // straddles the boundary, so this also says no read comes back short of what it
    // asked for — which `read_at` would report to the kernel as the end of the file.
    mock.reset();
    let over = fs
        .read_window(file, Some(SPAN_1 - CHUNK / 2..SPAN_1 + CHUNK / 2))
        .await
        .unwrap();
    assert_eq!(
        over.len() as u64,
        CHUNK,
        "a window across the span boundary"
    );
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!(
            "bytes={}-{}",
            SPAN_1 - CHUNK / 2,
            SPAN_1 - CHUNK / 2 + SPAN - 1
        ))],
        "a read span, beginning where the reader asked rather than on a fixed boundary"
    );

    // And a jump away from it is not a walk, so it pays for a first span again rather
    // than pulling 64 MiB to answer 4 KiB somewhere new.
    mock.reset();
    let back = fs.read_window(file, Some(1024..1024 + 4096)).await.unwrap();
    assert_eq!(back.len(), 4096);
    assert_eq!(
        mock.media_ranges(),
        vec![Some(format!("bytes=1024-{}", 1024 + SPAN_1 - 1))],
        "a read that does not continue the last one is not a walk"
    );
}
