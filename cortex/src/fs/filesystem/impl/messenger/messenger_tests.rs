//! Tests for the tree every source is served through.
//!
//! Driven by an in-memory source rather than a platform, which is the point twice over: what
//! is asserted here holds for every source that will ever be added, and writing one at all is
//! the first thing that says whether [`MessengerSource`] is shaped to be implemented.
//!
//! Several of these count *requests*. The rules this tree is built on — a day directory is
//! never empty, an unlisted day inside a walked range costs nothing to refuse — are claims
//! about what is not asked for, and a test that only checks the answer cannot tell a refusal
//! from a fetch that happened to come back empty.

use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::error::{ApiError, ErrorClass, SourceError, SourceResult};
use super::super::source::{Author, Capabilities, Message, Thread};
use super::*;

/// The default attachment ceiling, which these tests exercise the edges of.
///
/// A function rather than a constant, because it is now a field on
/// [`MessengerLimits`] — the tests follow the default rather than restating it.
fn window() -> u64 {
    MessengerLimits::default().window
}
use crate::BoxFuture;

/// Seconds since the epoch, as an instant.
fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// 2026-08-10T00:00:00Z, the day most of these are written around.
///
/// Asserted rather than trusted: a hand-computed epoch that is off by a few days still makes
/// every date test *pass against itself*, comparing one wrong answer with another.
const D10: u64 = 1_786_320_000;
const DAY: u64 = 86_400;

#[test]
fn the_fixture_day_is_the_day_it_says() {
    assert_eq!(day_of(at(D10)).to_string(), "2026-08-10");
    assert_eq!(day_of(at(D10 + 2 * DAY)).to_string(), "2026-08-12");
    assert_eq!(day_of(at(D10 + 30 * DAY)).to_string(), "2026-09-09");
}

fn msg(id: &str, ts: u64, text: &str) -> Message {
    Message {
        id: MsgId(id.into()),
        ts: at(ts),
        from: Author {
            id: "U1".into(),
            name: None,
            claimed: None,
        },
        text: text.into(),
        thread: None,
        files: Vec::new(),
        raw: serde_json::json!({}),
    }
}

#[derive(Default)]
struct Calls {
    history: AtomicUsize,
    thread: AtomicUsize,
    /// Every range `fetch_file` was asked for, in order. `None` is a whole-file request,
    /// which is the only shape the source can length-check.
    files: Mutex<Vec<Option<Range<u64>>>>,
}

struct TestSource {
    convs: Vec<Conversation>,
    /// Every message the workspace has, by conversation.
    msgs: Vec<(ConvId, Message)>,
    replies: Vec<(MsgId, Vec<Message>)>,
    users: Vec<User>,
    caps: Capabilities,
    /// Conversations that answer every read with a per-conversation denial.
    denied: Vec<ConvId>,
    /// Answer every ranged fetch with the whole file, as a server that ignores `Range` does.
    ignores_range: bool,
    calls: Calls,
}

impl TestSource {
    fn new(convs: Vec<Conversation>, msgs: Vec<(ConvId, Message)>) -> Self {
        TestSource {
            convs,
            msgs,
            replies: Vec::new(),
            users: Vec::new(),
            caps: Capabilities {
                channels: true,
                dms: true,
            },
            denied: Vec::new(),
            ignores_range: false,
            calls: Calls::default(),
        }
    }

    fn deny(mut self, conv: &str) -> Self {
        self.denied.push(ConvId(conv.into()));
        self
    }

    fn no_dms(mut self) -> Self {
        self.caps.dms = false;
        self
    }

    fn no_channels(mut self) -> Self {
        self.caps.channels = false;
        self
    }


    fn ignores_range(mut self) -> Self {
        self.ignores_range = true;
        self
    }

    fn of(&self, conv: &ConvId) -> Vec<Message> {
        let mut v: Vec<Message> = self
            .msgs
            .iter()
            .filter(|(c, _)| c == conv)
            .map(|(_, m)| m.clone())
            .collect();
        v.sort_by_key(|m| m.ts);
        v
    }

    fn denied_err() -> SourceError {
        SourceError::Api(ApiError {
            op: "history".into(),
            code: "not_in_channel".into(),
            detail: None,
            class: ErrorClass::ConversationDenied,
        })
    }
}

impl MessengerSource for TestSource {
    fn conversations<'a>(&'a self) -> BoxFuture<'a, SourceResult<(Vec<Conversation>, bool)>> {
        Box::pin(async move { Ok((self.convs.clone(), false)) })
    }

    fn history<'a>(
        &'a self,
        conv: &'a ConvId,
        window: Window,
    ) -> BoxFuture<'a, SourceResult<(Vec<Message>, bool)>> {
        Box::pin(async move {
            self.calls.history.fetch_add(1, Ordering::SeqCst);
            if self.denied.contains(conv) {
                return Err(Self::denied_err());
            }
            let msgs = self
                .of(conv)
                .into_iter()
                .filter(|m| m.ts >= window.start && m.ts < window.end)
                .collect();
            Ok((msgs, false))
        })
    }

    fn thread<'a>(
        &'a self,
        _conv: &'a ConvId,
        root: &'a MsgId,
    ) -> BoxFuture<'a, SourceResult<(Vec<Message>, bool)>> {
        Box::pin(async move {
            self.calls.thread.fetch_add(1, Ordering::SeqCst);
            let msgs = self
                .replies
                .iter()
                .find(|(r, _)| r == root)
                .map(|(_, m)| m.clone())
                .unwrap_or_default();
            Ok((msgs, false))
        })
    }

    fn users<'a>(&'a self) -> BoxFuture<'a, SourceResult<(Vec<User>, bool)>> {
        Box::pin(async move { Ok((self.users.clone(), false)) })
    }

    fn fetch_file<'a>(
        &'a self,
        file: &'a FileRef,
        range: Option<Range<u64>>,
    ) -> BoxFuture<'a, SourceResult<(Vec<u8>, bool)>> {
        Box::pin(async move {
            self.calls.files.lock().unwrap().push(range.clone());
            // Content a test can locate itself in: byte `n` is `n as u8`, so a window served
            // from the wrong offset is visible rather than merely short.
            let all: Vec<u8> = (0..file.size).map(|i| i as u8).collect();
            match range {
                // The second value is the whole point: `false` says the range was ignored and
                // these are the file's bytes from zero.
                Some(_) if self.ignores_range => Ok((all, false)),
                Some(r) => {
                    let end = r.end.min(file.size) as usize;
                    Ok((all[r.start as usize..end].to_vec(), true))
                }
                None => Ok((all, false)),
            }
        })
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }
}

/// A conversation created five days before [`D10`], so the calendar its date axis is generated
/// from starts there and reaches today.
fn conv(id: &str, name: &str, kind: ConvKind) -> Conversation {
    conv_born(id, name, kind, D10 - 5 * DAY)
}

fn conv_born(id: &str, name: &str, kind: ConvKind, created: u64) -> Conversation {
    Conversation {
        id: ConvId(id.into()),
        name: name.into(),
        kind,
        created: at(created),
    }
}

/// One channel with a message on the 10th and one on the 12th — a gap on the 11th, which is
/// the case the whole date design exists for.
fn two_days() -> MessengerFs<TestSource> {
    MessengerFs::new(TestSource::new(
        vec![conv("C1", "pricing", ConvKind::Channel)],
        vec![
            (ConvId("C1".into()), msg("1", D10 + 100, "on the tenth")),
            (
                ConvId("C1".into()),
                msg("2", D10 + 2 * DAY, "on the twelfth"),
            ),
        ],
    ))
}

/// The kind of an error, for asserting on one without matching a whole `io::Error`.
fn kind_of(e: Option<std::io::Error>) -> Option<std::io::ErrorKind> {
    e.map(|e| e.kind())
}

/// Whether a refusal is the read-only one every write on this store answers with.
fn read_only(e: Option<std::io::Error>) -> bool {
    kind_of(e) == Some(std::io::ErrorKind::ReadOnlyFilesystem)
}

fn names(entries: &[Dirent]) -> Vec<String> {
    let mut v: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn the_root_lists_the_sections_the_source_has() {
    let vol = two_days();
    assert_eq!(
        names(&vol.list(Path::new("/")).await.unwrap()),
        ["channels", "dms", "users"]
    );
}

/// A source with no channels — one serving only 1:1 and group chats, where the platform's
/// channels are a forum and belong to another shape — must not get an empty `channels/`
/// either. The asymmetry is what a four-platform tree turns up: the reader would conclude
/// there are no channels, when the truth is that they are somewhere else.
#[tokio::test]
async fn a_source_without_channels_has_no_channels_directory() {
    let vol = MessengerFs::new(
        TestSource::new(vec![conv("D1", "박지훈", ConvKind::Dm)], vec![]).no_channels(),
    );
    assert_eq!(
        names(&vol.list(Path::new("/")).await.unwrap()),
        ["dms", "users"]
    );
    assert!(matches!(
        vol.stat(Path::new("/channels")).await,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
    ));
    // And the section it does have still works.
    assert_eq!(
        names(&vol.list(Path::new("/dms")).await.unwrap()),
        ["박지훈__D1"]
    );
}

/// A source that cannot enumerate DMs must not get an empty `dms/`. The directory would say
/// there are none, where the truth is that this credential cannot see them — and the reader
/// has no way to tell the two apart.
#[tokio::test]
async fn a_source_without_dms_has_no_dms_directory() {
    let vol = MessengerFs::new(
        TestSource::new(vec![conv("C1", "pricing", ConvKind::Channel)], vec![]).no_dms(),
    );
    assert_eq!(
        names(&vol.list(Path::new("/")).await.unwrap()),
        ["channels", "users"]
    );
    assert!(matches!(
        vol.stat(Path::new("/dms")).await,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
    ));
}

/// The date axis is three levels of calendar, and listing any of them asks nothing.
///
/// This is the trade the axis exists for: a walk would spend a request per conversation and
/// still only reach its newest messages, so the listing it produced would be a window that read
/// like a whole. Arithmetic over `created` reaches every day the conversation ever had, for
/// nothing.
#[tokio::test]
async fn the_date_axis_is_a_calendar_and_costs_no_request() {
    let vol = two_days();
    // The conversation listing is the only request any of this needs.
    vol.list(Path::new("/channels")).await.unwrap();
    let before = vol.source.calls.history.load(Ordering::SeqCst);

    let years = names(&vol.list(Path::new("/channels/pricing__C1")).await.unwrap());
    assert_eq!(years, ["2026"]);
    let months = names(&vol.list(Path::new("/channels/pricing__C1/2026")).await.unwrap());
    assert!(months.contains(&"08".to_string()), "{months:?}");
    let days = names(&vol.list(Path::new("/channels/pricing__C1/2026/08")).await.unwrap());

    // Created on the 5th, so the 5th is the first day and the 11th — silent — is there too.
    assert!(days.contains(&"05".to_string()), "{days:?}");
    assert!(days.contains(&"11".to_string()), "a silent day is still a day");
    assert!(!days.contains(&"04".to_string()), "nothing before it existed");

    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        before,
        "three listings, no history call"
    );
}

/// A silent day inside the span is a real day that is empty, and finding out costs the one
/// request that reads it. That is the calendar's price, paid where the reader asked rather than
/// on every listing.
#[tokio::test]
async fn a_silent_day_is_served_empty() {
    let vol = two_days();
    let p = Path::new("/channels/pricing__C1/2026/08/11/chat.jsonl");
    let stat = vol.stat(p).await.expect("the day exists");
    assert_eq!(stat.size, 0, "nothing was said that day");

    let mut buf = vec![0u8; 64];
    assert_eq!(vol.read_at(p, &mut buf, 0).await.unwrap(), 0);
}

/// Outside the span there is nothing to fetch, and no request is spent saying so. The bottom is
/// the conversation's own creation — a date before it never existed to have messages in.
#[tokio::test]
async fn a_day_before_the_conversation_existed_costs_no_request() {
    let vol = two_days();
    let before = vol.source.calls.history.load(Ordering::SeqCst);
    for path in [
        "/channels/pricing__C1/2026/08/04", // one day before `created`
        "/channels/pricing__C1/2025",       // a year before it
        "/channels/pricing__C1/2026/07",    // a month before it
    ] {
        assert!(
            matches!(vol.stat(Path::new(path)).await,
                     Err(ref e) if e.kind() == std::io::ErrorKind::NotFound),
            "{path} is outside the span"
        );
    }
    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        before,
        "the span is arithmetic; being outside it is not a question for the source"
    );
}

/// One line per message, and the day holds exactly its own.
#[tokio::test]
async fn a_day_serves_its_messages_one_per_line() {
    let vol = two_days();
    let p = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    let stat = vol.stat(p).await.unwrap();
    let mut buf = vec![0u8; stat.size as usize];
    vol.read_at(p, &mut buf, 0).await.unwrap();
    let text = String::from_utf8(buf).unwrap();

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "one message, one line: {text:?}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["text"], "on the tenth");
    assert_eq!(v["ts"], "2026-08-10T00:01:40Z");
}

/// `threads/` and `files/` exist only when they hold something. An empty one is a claim the
/// tree cannot support — and the reader would pay a listing to find that out.
#[tokio::test]
async fn empty_subdirectories_are_not_synthesized() {
    let vol = two_days();
    let day = names(
        &vol.list(Path::new("/channels/pricing__C1/2026/08/10"))
            .await
            .unwrap(),
    );
    assert_eq!(day, ["chat.jsonl"], "no threads and no files that day");

    // With a thread and an attachment, both appear.
    let mut root = msg("100", D10 + 200, "let's discuss");
    root.thread = Some(Thread {
        root: None,
        replies: 2,
    });
    root.files = vec![FileRef {
        id: "F1".into(),
        name: "report.pdf".into(),
        size: 3,
        url: "https://example.invalid/f".into(),
    }];
    let mut src = TestSource::new(
        vec![conv("C1", "pricing", ConvKind::Channel)],
        vec![(ConvId("C1".into()), root)],
    );
    src.replies = vec![(
        MsgId("100".into()),
        vec![
            msg("100", D10 + 200, "let's discuss"),
            msg("101", D10 + 300, "sure"),
        ],
    )];
    let vol = MessengerFs::new(src);
    let day = names(
        &vol.list(Path::new("/channels/pricing__C1/2026/08/10"))
            .await
            .unwrap(),
    );
    assert_eq!(day, ["chat.jsonl", "files", "threads"]);

    // A thread is the same shape as a day.
    let t = names(
        &vol.list(Path::new("/channels/pricing__C1/2026/08/10/threads/100"))
            .await
            .unwrap(),
    );
    assert_eq!(t, ["chat.jsonl"]);
    assert_eq!(
        names(
            &vol.list(Path::new("/channels/pricing__C1/2026/08/10/threads"))
                .await
                .unwrap()
        ),
        ["100"]
    );
    // And the attachment is served at its listed length.
    let s = vol
        .stat(Path::new(
            "/channels/pricing__C1/2026/08/10/files/report.pdf__F1",
        ))
        .await
        .unwrap();
    assert_eq!(s.size, 3);
}

/// A conversation this credential cannot read is listed and empty — not a failure. One
/// channel a bot was never invited to must not take the tree down with it.
#[tokio::test]
async fn an_unreadable_conversation_is_empty_rather_than_broken() {
    let vol = MessengerFs::new(
        TestSource::new(
            vec![
                conv("C1", "pricing", ConvKind::Channel),
                conv("C2", "secret", ConvKind::Channel),
            ],
            vec![(ConvId("C1".into()), msg("1", D10 + 100, "hi"))],
        )
        .deny("C2"),
    );
    // Still listed.
    assert_eq!(
        names(&vol.list(Path::new("/channels")).await.unwrap()),
        ["pricing__C1", "secret__C2"]
    );
    // Its date axis is arithmetic, so it is there whether or not the conversation can be read
    // — and reading a day of it answers empty rather than failing.
    assert_eq!(
        names(&vol.list(Path::new("/channels/secret__C2")).await.unwrap()),
        ["2026"]
    );
    let denied = Path::new("/channels/secret__C2/2026/08/10/chat.jsonl");
    assert_eq!(
        vol.stat(denied).await.expect("the day is a day").size,
        0,
        "one unreadable conversation is empty, not broken"
    );
    // The readable one is unaffected, which is the whole point.
    let p = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    assert!(vol.stat(p).await.unwrap().size > 0);
}

/// A path is an address, so it resolves on the id. A name is what a workspace renames, and a
/// path that stopped working because somebody retitled a channel was never an address.
#[tokio::test]
async fn a_conversation_resolves_by_id_not_by_name() {
    let vol = two_days();
    // Any name, the right id.
    assert!(vol.stat(Path::new("/channels/whatever__C1")).await.is_ok());
    // The right name, a wrong id.
    assert!(matches!(
        vol.stat(Path::new("/channels/pricing__C9")).await,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
    ));
}

/// A display name is not a path component until it has been made into one. A `/` in a channel
/// name would otherwise open a directory nobody created.
#[tokio::test]
async fn a_name_with_a_separator_stays_one_component() {
    let vol = MessengerFs::new(TestSource::new(
        vec![conv("C1", "eng/infra", ConvKind::Channel)],
        vec![],
    ));
    let entries = names(&vol.list(Path::new("/channels")).await.unwrap());
    assert_eq!(entries, ["eng-infra__C1"]);
    assert_eq!(entries[0].matches('/').count(), 0);
}

/// Read-only, and it says so with `EROFS` rather than `ENOSYS`: userspace has a path for a
/// filesystem that is protected and none for one that is broken.
#[tokio::test]
async fn every_write_is_refused_as_read_only() {
    let vol = two_days();
    let p = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    assert!(read_only(vol.mkdir(p).await.err()));
    assert!(read_only(vol.unlink(p).await.err()));
    assert!(read_only(vol.rmdir(p).await.err()));
    assert!(read_only(vol.write_at(p, b"x", 0).await.err()));
    assert!(read_only(vol.create(p).await.err()));
    assert!(read_only(vol.truncate(p, 0).await.err()));
}

/// Reading a directory is `EISDIR`, and listing a file is `ENOTDIR`. The three answers agree
/// because one path walker produces all of them.
#[tokio::test]
async fn a_directory_and_a_file_are_not_interchangeable() {
    let vol = two_days();
    let mut buf = [0u8; 8];
    assert_eq!(
        kind_of(vol.read_at(Path::new("/channels"), &mut buf, 0).await.err()),
        Some(std::io::ErrorKind::IsADirectory)
    );
    assert_eq!(
        kind_of(
            vol.list(Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl"))
                .await
                .err()
        ),
        Some(std::io::ErrorKind::NotADirectory)
    );
}

/// A second read of the same day reuses the first one's request. Without that, a `stat`
/// followed by an `open` — which is what every `cat` is — pays twice for one thing.
#[tokio::test]
async fn a_stat_and_an_open_share_one_request() {
    let vol = two_days();
    let p = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    vol.stat(p).await.unwrap();
    let after_stat = vol.source.calls.history.load(Ordering::SeqCst);
    vol.read_at(p, &mut [0u8; 1], 0).await.unwrap();
    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        after_stat,
        "the open re-fetched what the stat had already assembled"
    );
}

/// Author names come from the user directory, resolved once per run rather than per line — a
/// line carrying only an id is one nobody can `grep` for by person.
#[tokio::test]
async fn an_author_id_is_resolved_to_a_name() {
    let mut src = TestSource::new(
        vec![conv("C1", "pricing", ConvKind::Channel)],
        vec![(ConvId("C1".into()), msg("1", D10 + 100, "hi"))],
    );
    src.users = vec![User {
        id: "U1".into(),
        name: "김철수".into(),
        record: serde_json::json!({"id": "U1"}),
    }];
    let vol = MessengerFs::new(src);

    let p = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    let stat = vol.stat(p).await.unwrap();
    let mut buf = vec![0u8; stat.size as usize];
    vol.read_at(p, &mut buf, 0).await.unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(buf.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(v["from"]["name"], "김철수");

    // And the directory itself is served as the platform's own record.
    assert!(vol.stat(Path::new("/users/김철수__U1.json")).await.is_ok());
}

/// A workspace whose single day holds one attachment of `size` bytes.
fn with_attachment(size: u64) -> MessengerFs<TestSource> {
    let mut m = msg("1", D10 + 100, "see attached");
    m.files = vec![FileRef {
        id: "F1".into(),
        name: "doc.bin".into(),
        size,
        url: "https://example.invalid/f".into(),
    }];
    MessengerFs::new(TestSource::new(
        vec![conv("C1", "pricing", ConvKind::Channel)],
        vec![(ConvId("C1".into()), m)],
    ))
}

const ATTACHMENT: &str = "/channels/pricing__C1/2026/08/10/files/doc.bin__F1";

/// Read `len` bytes of the attachment at `offset`.
async fn read_bytes(vol: &MessengerFs<TestSource>, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = vol
        .read_at(Path::new(ATTACHMENT), &mut buf, offset)
        .await
        .expect("reads");
    buf.truncate(n);
    buf
}

/// An attachment that fits is fetched **unranged**, which is the only shape the source can
/// check against the length the listing promised — the check that catches a refused token
/// answering 200 with a login page.
#[tokio::test]
async fn an_attachment_that_fits_is_fetched_whole_and_checkable() {
    let vol = with_attachment(1024);
    let got = read_bytes(&vol, 0, 1024).await;
    assert_eq!(got.len(), 1024);
    assert_eq!(got[0], 0);
    assert_eq!(got[255], 255);

    let asked = vol.source.calls.files.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec![None],
        "a file under the ceiling must be requested whole, or it cannot be verified"
    );
}

/// Past the ceiling nothing is fetched at `open`: the size is already known from the listing,
/// so a reader that only stats the file pays nothing.
#[tokio::test]
async fn a_large_attachment_costs_nothing_to_open() {
    let vol = with_attachment(window() * 3);
    let stat = vol.stat(Path::new(ATTACHMENT)).await.expect("stats");
    assert_eq!(stat.size, window() * 3, "the size comes from the listing");
    assert!(
        vol.source.calls.files.lock().unwrap().is_empty(),
        "opening a large attachment must not fetch it"
    );
}

/// A large attachment reads correctly, and one window serves every read inside it — the whole
/// point of the window, since a reader asks in 128 KiB pieces at most.
#[tokio::test]
async fn one_window_serves_many_reads() {
    let vol = with_attachment(window() * 2);

    // Sixteen 32 KiB reads, the size FUSE-T was measured to ask for.
    for i in 0..16u64 {
        let at = i * 32 * 1024;
        let mut buf = vec![0u8; 32 * 1024];
        let n = vol
            .read_at(Path::new(ATTACHMENT), &mut buf, at)
            .await
            .unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(buf[0], at as u8, "window served from the wrong offset");
    }
    assert_eq!(
        vol.source.calls.files.lock().unwrap().len(),
        1,
        "sixteen reads inside one window must cost one fetch"
    );
}

/// A request the held window only *partly* covers must not stop there. Returning short would
/// tell the caller this was the end of the file — the trait's contract is that a short read is
/// EOF and nothing else — so the loop fetches the remainder and the bytes line up across the
/// seam. Content that says where it came from is what makes a window served from the wrong
/// offset visible rather than merely short.
#[tokio::test]
async fn a_request_the_window_half_covers_is_filled_not_truncated() {
    let vol = with_attachment(window() + 4096);

    // Establish a window at 0. A window starts where the miss was, so this one ends exactly
    // at window() — which is what lets the next read straddle its edge.
    let mut first = vec![0u8; 64];
    assert_eq!(
        vol.read_at(Path::new(ATTACHMENT), &mut first, 0)
            .await
            .unwrap(),
        64
    );
    assert_eq!(vol.source.calls.files.lock().unwrap().len(), 1);

    // Eight bytes inside the held window, eight past it.
    let mut buf = vec![0u8; 16];
    let n = vol
        .read_at(Path::new(ATTACHMENT), &mut buf, window() - 8)
        .await
        .unwrap();
    assert_eq!(
        n, 16,
        "a partly covered request must be filled, not truncated"
    );
    for (i, b) in buf.iter().enumerate() {
        assert_eq!(
            *b,
            (window() - 8 + i as u64) as u8,
            "byte {i} across the seam"
        );
    }

    let asked = vol.source.calls.files.lock().unwrap().clone();
    assert_eq!(
        asked,
        vec![Some(0..window()), Some(window()..(window() + 4096))],
        "the second fetch starts where the held window ended"
    );
}

/// Past the end the listing named, the answer is zero and no request is made: the source would
/// refuse the range, and refusing here costs nothing.
#[tokio::test]
async fn a_read_past_the_end_asks_for_nothing() {
    let vol = with_attachment(window() * 2);
    let mut buf = vec![0u8; 64];
    assert_eq!(
        vol.read_at(Path::new(ATTACHMENT), &mut buf, window() * 2)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        vol.read_at(Path::new(ATTACHMENT), &mut buf, window() * 9)
            .await
            .unwrap(),
        0
    );
    assert!(vol.source.calls.files.lock().unwrap().is_empty());
}

/// A read the file only partly covers is truncated to the file, not refused — and the last
/// window is short rather than reaching past the end.
#[tokio::test]
async fn a_read_is_clipped_to_the_listed_length() {
    let vol = with_attachment(window() + 100);
    let mut buf = vec![0u8; 4096];
    let n = vol
        .read_at(Path::new(ATTACHMENT), &mut buf, window() + 50)
        .await
        .unwrap();
    assert_eq!(n, 50, "only the 50 bytes that exist");
    let asked = vol.source.calls.files.lock().unwrap().clone();
    assert_eq!(asked, vec![Some((window() + 50)..(window() + 100))]);
}

/// A `Range` is a request, not a promise: a server may ignore it and send the file from byte
/// zero, which it reports by answering `false`. Recorded as if it began at the requested
/// offset, that window hands out the head of the file for every read — quietly, and with the
/// right length, so nothing downstream can notice.
#[tokio::test]
async fn a_server_that_ignores_the_range_is_not_served_as_the_wrong_offset() {
    let mut m = msg("1", D10 + 100, "see attached");
    m.files = vec![FileRef {
        id: "F1".into(),
        name: "doc.bin".into(),
        size: window() + 4096,
        url: "https://example.invalid/f".into(),
    }];
    let vol = MessengerFs::new(
        TestSource::new(
            vec![conv("C1", "pricing", ConvKind::Channel)],
            vec![(ConvId("C1".into()), m)],
        )
        .ignores_range(),
    );

    // Well past the first window, where a window mislabelled as starting at `at` would answer
    // with byte 0 of the file instead.
    let at = window() + 1000;
    let mut buf = vec![0u8; 8];
    assert_eq!(
        vol.read_at(Path::new(ATTACHMENT), &mut buf, at)
            .await
            .unwrap(),
        8
    );
    for (i, b) in buf.iter().enumerate() {
        assert_eq!(
            *b,
            (at + i as u64) as u8,
            "byte {i} came from the wrong offset"
        );
    }

    // And the next read is served from the window that was recorded correctly.
    let before = vol.source.calls.files.lock().unwrap().len();
    let mut buf = vec![0u8; 8];
    assert_eq!(
        vol.read_at(Path::new(ATTACHMENT), &mut buf, 64)
            .await
            .unwrap(),
        8
    );
    assert_eq!(buf[0], 64);
    assert_eq!(
        vol.source.calls.files.lock().unwrap().len(),
        before,
        "the whole file was already held; reading elsewhere in it must not refetch"
    );
}

/// A file under the ceiling is fetched once, not once per read.
///
/// A kernel reads a file in chunks of its own choosing — 32 KiB through FUSE-T, 128 KiB through
/// virtio-fs — and each chunk is a separate `read_at`. Fetching the whole file on each of them
/// turns a 1 MiB attachment into megabytes per read and an 8 MiB one into hundreds, which is the
/// cost `windowed` exists to bound and which the files small enough to skip it were paying in
/// full. Counting requests rather than timing them, because the defect is invisible against a
/// fixture that answers instantly.
#[tokio::test]
async fn a_small_file_is_fetched_once_however_the_kernel_chunks_it() {
    const SIZE: u64 = 1 << 20;
    let vol = with_attachment(SIZE);

    let mut buf = vec![0u8; 128 * 1024];
    let mut at = 0u64;
    loop {
        let n = vol
            .read_at(Path::new(ATTACHMENT), &mut buf, at)
            .await
            .unwrap();
        if n == 0 {
            break;
        }
        // The bytes have to stay right across the seams, not merely be cheap.
        for (i, b) in buf[..n].iter().enumerate() {
            assert_eq!(*b, (at + i as u64) as u8, "byte {}", at + i as u64);
        }
        at += n as u64;
    }
    assert_eq!(at, SIZE, "the whole file is read");
    assert_eq!(
        vol.source.calls.files.lock().unwrap().len(),
        1,
        "one fetch for the file, and none for the read that finds EOF"
    );
}

/// Holding a small file must not out-live the file it belongs to: the slot is one, so the next
/// path served replaces it rather than answering with the wrong file's bytes.
#[tokio::test]
async fn a_held_small_file_never_answers_for_another_path() {
    let vol = with_attachment(4096);
    let mut buf = vec![0u8; 4096];
    assert_eq!(
        vol.read_at(Path::new(ATTACHMENT), &mut buf, 0)
            .await
            .unwrap(),
        4096
    );

    // `chat.jsonl` is assembled, not fetched, so a held window that ignored the path would show
    // up here as attachment bytes.
    let mut chat = vec![0u8; 64];
    let n = vol
        .read_at(
            Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl"),
            &mut chat,
            0,
        )
        .await
        .unwrap();
    assert!(n > 0);
    assert_eq!(chat[0], b'{', "the day's own bytes, not the held file's");
}

/// A conversation named past what a path component holds is listed under a name that fits, and
/// that name is the one that opens.
///
/// Both halves matter and they are one test on purpose: a listing whose entries cannot be
/// resolved is worse than an absent one, because the caller can see the name and every attempt
/// to use it fails. Korean, because a byte limit and a character count are the same number
/// until the text stops being ASCII — 200 characters here are 600 bytes.
#[tokio::test]
async fn a_conversation_named_past_the_limit_is_listed_and_opens() {
    let long = "가".repeat(200);
    let vol = MessengerFs::new(TestSource::new(
        vec![conv("C1", &long, ConvKind::Channel)],
        vec![(ConvId("C1".into()), msg("1", D10 + 100, "on the tenth"))],
    ));

    let listed = vol
        .list(Path::new("/channels"))
        .await
        .expect("channels list");
    assert_eq!(listed.len(), 1);
    let name = listed[0].name.clone();
    assert!(name.len() <= 255, "{} bytes", name.len());
    assert!(name.ends_with("__C1"), "the id half survived: {name}");

    // Down the axis from the fitted name: every level has to resolve under it.
    let root = Path::new("/channels").join(&name);
    let years = names(&vol.list(&root).await.expect(
        "the name the listing gave has to be the name that opens",
    ));
    assert_eq!(years, ["2026"]);
    let chat = root.join("2026/08/10/chat.jsonl");
    let mut buf = vec![0u8; 512];
    let n = vol
        .read_at(&chat, &mut buf, 0)
        .await
        .expect("the day reads");
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("on the tenth"),
        "the file behind the fitted name is the conversation's own"
    );
}

// ---- what a mount is allowed to spend --------------------------------------------------

/// Two days of one channel, each with a message long enough to measure.
fn two_fat_days(limits: MessengerLimits) -> MessengerFs<TestSource> {
    let body = "x".repeat(400);
    MessengerFs::with_limits(
        TestSource::new(
            vec![conv("C1", "pricing", ConvKind::Channel)],
            vec![
                (ConvId("C1".into()), msg("1", D10 + 100, &body)),
                (ConvId("C1".into()), msg("2", D10 + 2 * DAY, &body)),
            ],
        ),
        limits,
    )
}

/// The default is what the lane always had, so a consumer that never mentions limits keeps the
/// behaviour every other test in this file asserts.
#[test]
fn the_default_limits_are_the_numbers_this_lane_chose() {
    let d = MessengerLimits::default();
    assert_eq!(d.window, 8 << 20);
    assert_eq!(d.ttl, Duration::from_secs(15));
    assert_eq!(d.text_budget, 32 << 20);
}


/// A budget that fits both days keeps both: coming back to the first costs nothing.
#[tokio::test]
async fn a_day_within_the_budget_is_still_held() {
    let vol = two_fat_days(MessengerLimits::default());
    let ten = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    let twelve = Path::new("/channels/pricing__C1/2026/08/12/chat.jsonl");

    vol.stat(ten).await.unwrap();
    vol.stat(twelve).await.unwrap();
    let before = vol.source.calls.history.load(Ordering::SeqCst);
    vol.stat(ten).await.unwrap();

    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        before,
        "the first day was still held"
    );
}

/// And a budget too small for both drops the one read longest ago, which costs its request
/// again. That is the trade the number buys: memory against round trips.
#[tokio::test]
async fn past_the_text_budget_the_least_recently_used_day_goes() {
    let vol = two_fat_days(MessengerLimits {
        // Room for one of these days and not two.
        text_budget: 600,
        ..Default::default()
    });
    let ten = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    let twelve = Path::new("/channels/pricing__C1/2026/08/12/chat.jsonl");

    vol.stat(ten).await.unwrap();
    vol.stat(twelve).await.unwrap();
    let before = vol.source.calls.history.load(Ordering::SeqCst);
    vol.stat(ten).await.unwrap();

    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        before + 1,
        "the 10th was evicted for the 12th and had to be fetched again"
    );
}

/// Recency and not insertion order: the day a reader keeps returning to outlives the one they
/// passed through once, whichever arrived first.
#[tokio::test]
async fn a_day_that_keeps_being_read_outlives_one_that_does_not() {
    let vol = two_fat_days(MessengerLimits {
        text_budget: 600,
        ..Default::default()
    });
    let ten = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");
    let twelve = Path::new("/channels/pricing__C1/2026/08/12/chat.jsonl");

    vol.stat(ten).await.unwrap();
    vol.stat(ten).await.unwrap(); // touched again: now the most recent
    vol.stat(twelve).await.unwrap(); // evicts the 10th, the only other entry
    let before = vol.source.calls.history.load(Ordering::SeqCst);
    vol.stat(twelve).await.unwrap();

    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        before,
        "the day just read is the one still held"
    );
}

/// A single day larger than the whole budget stays while it is the only thing held — because
/// dropping it would refetch the same day for every chunk a kernel reads, which is the cost the
/// attachment window exists to avoid, paid by text instead.
#[tokio::test]
async fn one_day_bigger_than_the_budget_is_still_held_while_it_is_read() {
    let vol = two_fat_days(MessengerLimits {
        text_budget: 10,
        ..Default::default()
    });
    let ten = Path::new("/channels/pricing__C1/2026/08/10/chat.jsonl");

    vol.stat(ten).await.unwrap();
    let before = vol.source.calls.history.load(Ordering::SeqCst);
    let mut buf = [0u8; 64];
    vol.read_at(ten, &mut buf, 0).await.unwrap();

    assert_eq!(
        vol.source.calls.history.load(Ordering::SeqCst),
        before,
        "the read after the stat came out of what was held"
    );
}
