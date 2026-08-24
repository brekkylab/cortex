//! Unit tests for the tree's names.
//!
//! Nothing here makes a request: a path is arithmetic over a conversation and an instant, and
//! the point of the file is that one rule serves both the listing and anything pointing back
//! at it.

use chrono::Datelike;
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::fs::{Author, ConvId, MsgId, Thread};

fn conv(name: &str, id: &str, kind: ConvKind) -> Conversation {
    Conversation {
        id: ConvId(id.into()),
        name: name.into(),
        kind,
        created: at(0),
    }
}

/// `2026-08-03T06:17:55Z`, the instant a real workspace's thread was started on.
fn at(secs: u64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn msg(ts: u64, thread: Option<Thread>) -> Message {
    Message {
        id: MsgId(format!("{ts}.000000")),
        ts: at(ts),
        from: Author {
            id: "U1".into(),
            name: None,
            claimed: None,
        },
        text: String::new(),
        thread,
        files: Vec::new(),
        raw: serde_json::Value::Null,
    }
}

#[test]
fn a_conversation_directory_carries_both_halves() {
    assert_eq!(
        conv_dir(&conv("pricing", "C1", ConvKind::Channel)),
        "pricing__C1"
    );
}

#[test]
fn a_name_that_would_leave_its_component_is_sanitized() {
    // A channel named with a separator must not open a directory nobody created, and the id
    // half still has to be findable at the end of the name.
    assert_eq!(
        conv_dir(&conv("a/b", "C1", ConvKind::Channel)),
        "a-b__C1",
        "a slash becomes a dash rather than a path component"
    );
    assert_eq!(
        conv_dir(&conv("  ", "C1", ConvKind::Channel)),
        "unnamed__C1"
    );
    assert_eq!(
        conv_dir(&conv(".hidden", "C1", ConvKind::Channel)),
        "hidden__C1"
    );
}

/// A control character is legal in a POSIX name and breaks every format built on lines.
///
/// A newline splits a listing into two entries, a tab splits a search hit's `<path>\t<record>`
/// in the wrong place, and an escape sequence rewrites what a terminal already drew. Slack's own
/// channel names cannot carry one; a display name and an uploaded filename are whoever typed
/// them, which is where this comes from.
#[test]
fn a_control_character_never_reaches_a_path() {
    for (raw, want) in [
        ("two\nlines", "two-lines__C1"),
        ("a\tb", "a-b__C1"),
        ("re\rwrite", "re-write__C1"),
        ("esc\u{1b}[2Kape", "esc-[2Kape__C1"),
    ] {
        let got = conv_dir(&conv(raw, "C1", ConvKind::Channel));
        assert_eq!(got, want, "{raw:?}");
        assert!(
            !got.chars().any(char::is_control),
            "{got:?} still carries one"
        );
    }
}

#[test]
fn a_kind_picks_its_section() {
    assert_eq!(section_of(ConvKind::Channel), "channels");
    assert_eq!(section_of(ConvKind::Dm), "dms");
}

/// The bucketing rule is UTC, which is a decision and not an accident — the same mount read
/// from two zones has to name a message's day the same way.
#[test]
fn a_day_directory_is_the_utc_day() {
    // 2026-08-03T23:00:00Z — still the 3rd in UTC, already the 4th in Seoul.
    assert_eq!(day_dir(at(1_785_798_000)), "2026/08");
    // One hour later crosses midnight UTC.
    assert_eq!(day_dir(at(1_785_801_600)), "2026/08");
}

#[test]
fn a_message_posted_to_the_conversation_is_in_the_days_own_file() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    assert_eq!(
        chat_path(&c, &msg(1_785_737_875, None), at(1_785_737_875)),
        "channels/pricing__C1/2026/08/2026-08-03.jsonl"
    );
}

/// A root is a conversation message: it has a thread, but no root above it, so its line is in
/// the day's own file — the same place the tree lists it.
#[test]
fn a_thread_root_is_still_in_the_days_own_file() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    let root = msg(
        1_785_737_875,
        Some(Thread {
            root: None,
            replies: 4,
        }),
    );
    assert_eq!(
        chat_path(&c, &root, at(1_785_737_875)),
        "channels/pricing__C1/2026/08/2026-08-03.jsonl"
    );
}

/// The one the caller has to help with: a reply is served under the day its *root* was
/// started, which is why `day` is an argument and not `msg.ts`.
#[test]
fn a_reply_is_under_its_roots_day_and_not_its_own() {
    let c = conv("pricing", "C1", ConvKind::Channel);
    let reply = msg(
        1_786_000_000, // days later
        Some(Thread {
            root: Some(MsgId("1785737875.341929".into())),
            replies: 0,
        }),
    );
    assert_eq!(
        chat_path(&c, &reply, at(1_785_737_875)),
        "channels/pricing__C1/2026/08/threads/1785737875.341929.jsonl",
        "the thread's day, not the reply's"
    );
}

#[test]
fn a_dm_lands_in_the_dms_section() {
    let c = conv("김철수", "D1", ConvKind::Dm);
    assert_eq!(
        chat_path(&c, &msg(1_785_737_875, None), at(1_785_737_875)),
        "dms/김철수__D1/2026/08/2026-08-03.jsonl"
    );
}

/// A name longer than a path component may be is cut to fit, and the id survives whole.
///
/// `NAME_MAX` is a hard limit: over it the entry is not served at all, so the conversation
/// disappears from the tree with nothing saying why. What gives way is the readable half.
#[test]
fn a_name_too_long_for_a_component_is_cut_and_the_id_kept() {
    let long = "a".repeat(400);
    let dir = conv_dir(&conv(&long, "C1", ConvKind::Channel));
    assert_eq!(dir.len(), NAME_MAX, "the component fills the limit exactly");
    assert!(dir.ends_with("__C1"), "the id is the address and is never cut");
}

/// The `.json` a user file ends in spends the same budget, which is why the suffix is passed
/// in rather than concatenated after: a caller appending it would put the component back over
/// the limit having just fitted it.
#[test]
fn a_suffix_is_counted_against_the_same_budget() {
    let long = "a".repeat(400);
    let with = entry(&long, "U1", ".json");
    assert_eq!(with.len(), NAME_MAX);
    assert!(with.ends_with("__U1.json"));
    assert_eq!(
        with.trim_end_matches(".json").len(),
        NAME_MAX - ".json".len()
    );
}

/// Cutting by bytes must land on a character boundary. A Korean name is three bytes a
/// character, so a budget that is not a multiple of three falls inside one — and `truncate`
/// panics there rather than producing the broken string, which is the only reason this is
/// visible at all.
#[test]
fn a_multibyte_name_is_cut_between_characters() {
    let long = "가".repeat(200); // 600 bytes
    let dir = conv_dir(&conv(&long, "C1", ConvKind::Channel));
    assert!(dir.len() <= NAME_MAX);
    // 255 - 4 = 251 bytes of budget, which is 83 whole characters and two bytes over.
    let head = dir.trim_end_matches("__C1");
    assert_eq!(head.chars().count(), 83);
    assert!(head.chars().all(|c| c == '가'), "no partial character");
}

/// Two different long names shorten to the same text. Their components still differ, because
/// the id follows — which is what makes cutting the readable half safe.
#[test]
fn two_long_names_that_shorten_alike_are_still_two_directories() {
    let a = conv_dir(&conv(&"x".repeat(400), "C1", ConvKind::Channel));
    let b = conv_dir(&conv(&"x".repeat(500), "C2", ConvKind::Channel));
    assert_ne!(a, b);
    assert_eq!(a.trim_end_matches("__C1"), b.trim_end_matches("__C2"));
}

/// A cut lands wherever the budget ran out, which can be just after a space. A trailing blank
/// in a directory name is legal and invisible, which is worse than either having it or not.
#[test]
fn a_cut_does_not_leave_a_trailing_blank() {
    let name = format!("{} tail", "a".repeat(246));
    let dir = conv_dir(&conv(&name, "C1", ConvKind::Channel));
    assert!(!dir.trim_end_matches("__C1").ends_with(' '), "{dir:?}");
}

/// Short names are untouched — the limit is a ceiling, not a width.
#[test]
fn a_name_that_fits_is_left_alone() {
    assert_eq!(conv_dir(&conv("pricing", "C1", ConvKind::Channel)), "pricing__C1");
}


/// An instant no calendar has must not take the mount down.
///
/// `DateTime::<Utc>::from(SystemTime)` ends in a `timestamp_opt` that panics out of range, and
/// every instant in this lane came from a service's JSON — Slack's `created` is any `u64` it
/// cares to send. A panic in `readdir` is the whole mount, so an absurd instant has to become
/// an absurd date instead.
#[test]
fn an_instant_off_the_calendar_does_not_panic() {
    // Constructible but far past what a calendar has. `SystemTime` itself refuses a `Duration`
    // that overflows it, so this is the widest a service's `u64` can actually reach.
    for secs in [0, 1, 1_785_735_483, 9_999_999_999_999, 200_000_000_000_000] {
        let d = day_of(at(secs));
        assert!(d.year() >= 1970, "{secs} gave {d}");
    }
    // And before the epoch, which no platform here reports but the type permits.
    let before = UNIX_EPOCH
        .checked_sub(Duration::from_secs(9_999_999_999_999))
        .expect("a pre-epoch instant");
    assert!(day_of(before).year() <= 1970);
}

/// `parse` accepts a leading sign, so a two-byte `+2` would pass a width check and mean
/// February — a second path for one month, and a second cache entry that never sees the first.
#[test]
fn a_signed_month_is_not_a_month() {
    assert_eq!(month_of("2026", "08"), Some((2026, 8)));
    for (y, m) in [
        ("2026", "+8"),
        ("2026", "+2"),
        ("2026", "-2"),
        ("+026", "08"),
        ("-026", "08"),
        ("2026", "8"),
        ("2026", "008"),
        ("26", "08"),
        ("2026", "13"),
        ("2026", "00"),
        ("2026", " 8"),
    ] {
        assert_eq!(month_of(y, m), None, "{y}/{m} was accepted");
    }
}

/// Every month between the two ends, inclusive, and nothing outside them.
#[test]
fn the_month_span_covers_both_ends_and_rolls_the_year() {
    let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
    assert_eq!(months(d(2026, 8, 10), d(2026, 8, 21)), [(2026, 8)]);
    assert_eq!(
        months(d(2026, 11, 30), d(2027, 2, 1)),
        [(2026, 11), (2026, 12), (2027, 1), (2027, 2)],
        "December rolls into January"
    );
    // A conversation created after today has no months at all, which is what keeps a future
    // `created` from inventing an axis.
    assert!(months(d(2027, 1, 1), d(2026, 8, 21)).is_empty());
}


/// A character that rewrites how a line renders, rather than what it says.
///
/// `char::is_control` is category Cc only, so a bidi override slips past it — and a bidi
/// override is exactly the thing the rule exists to stop: `invoice<RLO>fdp.txt` renders as
/// `invoicetxt.pdf` in anything that honours it.
#[test]
fn a_name_cannot_rewrite_how_it_renders() {
    for raw in [
        "invoice\u{202E}fdp.txt",  // RIGHT-TO-LEFT OVERRIDE
        "two\u{2028}lines",        // LINE SEPARATOR
        "a\u{200B}b",              // ZERO WIDTH SPACE
        "\u{FEFF}name",            // BOM
        "iso\u{2066}late",         // LEFT-TO-RIGHT ISOLATE
    ] {
        let got = conv_dir(&conv(raw, "C1", ConvKind::Channel));
        assert!(
            !got.chars().any(|c| c.is_control()
                || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}'
                    | '\u{2028}' | '\u{2029}' | '\u{FEFF}')),
            "{raw:?} became {got:?}"
        );
    }
}

/// The id half is the platform's, so it is sanitized too. An id carrying a `/` would otherwise
/// be one listing entry that names two path components and opens none.
#[test]
fn an_id_cannot_carry_a_separator_into_a_component() {
    assert_eq!(conv_dir(&conv("pricing", "C/1", ConvKind::Channel)), "pricing__C-1");
    assert_eq!(conv_dir(&conv("pricing", "C\t1", ConvKind::Channel)), "pricing__C-1");
    assert_eq!(entry("amy", "U\n1", ".json"), "amy__U-1.json");
}

/// A thread is named for its root and nothing else — no `unnamed__` in front — and the id is
/// still sanitized.
#[test]
fn a_thread_file_is_its_root_and_the_suffix() {
    assert_eq!(thread_file("1785737875.341929"), "1785737875.341929.jsonl");
    assert_eq!(thread_file("a/b"), "a-b.jsonl");
    assert!(thread_file(&"x".repeat(400)).len() <= NAME_MAX);
}

/// A cut can expose a blank `sanitize`'s own `trim` never saw, and it need not be an ASCII one.
#[test]
fn a_cut_does_not_leave_a_non_ascii_blank() {
    let name = format!("{}\u{00A0}tail", "a".repeat(249));
    let dir = conv_dir(&conv(&name, "C1", ConvKind::Channel));
    let head = dir.trim_end_matches("__C1");
    assert!(
        !head.ends_with(char::is_whitespace),
        "{dir:?} ends in a blank"
    );
}


/// An attachment's extension goes past the id, because everything that dispatches on a suffix
/// reads the *end* of a name. With the id last, `ls files/*.pptx` matches nothing.
#[test]
fn an_attachment_keeps_its_extension_at_the_end() {
    assert_eq!(file_entry("deck (3).pptx", "F1"), "deck (3)__F1.pptx");
    assert_eq!(file_entry("식단표.pdf", "F2"), "식단표__F2.pdf");
    // One extension, the last one. `*.gz` matches; `*.tar.gz` does not, and a table of
    // compound suffixes is never finished.
    assert_eq!(file_entry("archive.tar.gz", "F3"), "archive.tar__F3.gz");
}

/// A dot is not always an extension, and splitting the wrong one puts half a word past the id.
#[test]
fn a_dot_that_is_not_an_extension_is_left_alone() {
    // No extension at all.
    assert_eq!(file_entry("Makefile", "F1"), "Makefile__F1");
    // A leading dot is a name, not a suffix.
    assert_eq!(file_entry(".gitignore", "F2"), "gitignore__F2");
    // Words, not a suffix: too long, or not alphanumeric.
    assert_eq!(file_entry("report.final version", "F3"), "report.final version__F3");
    assert_eq!(
        file_entry("notes.averyverylongsuffixindeed", "F4"),
        "notes.averyverylongsuffixindeed__F4"
    );
    // A trailing dot has nothing after it.
    assert_eq!(file_entry("weird.", "F5"), "weird.__F5");
}

/// The suffix spends the same 255 bytes, so a long name still fits and the id still survives.
#[test]
fn a_long_attachment_name_is_cut_and_keeps_both_ends() {
    let name = format!("{}.pdf", "a".repeat(400));
    let got = file_entry(&name, "F1");
    assert_eq!(got.len(), NAME_MAX);
    assert!(got.ends_with("__F1.pdf"), "{got}");
}
