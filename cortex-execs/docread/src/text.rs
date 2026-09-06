//! A file through a window: the lines that were asked for, numbered, and how many there are.
//!
//! # The file is read whole and held a window at a time
//!
//! Every line is walked, because the count at the end of the answer is the whole file's and
//! there is no way to know how many lines a file has without passing over all of them — a
//! seek can find a byte, and nothing can find the 4210th newline without reading 4209 of
//! them.
//!
//! What is *kept* is the window. Lines outside it are read with nowhere to put them
//! ([`read_line`]'s `cap` of zero), so a window on a gigabyte holds the window, and one line
//! of a file that is one line holds [`MAX_LINE_BYTES`] of it. That is the property worth
//! having: nothing a caller can name makes this program's memory the size of what they named.
//!
//! # What "not text" means here
//!
//! The lines being shown decode as UTF-8 and hold no NUL. Both are asked of the window and
//! not of the file, which is the rule this can actually keep — a log with one corrupt record
//! at line 90000 still answers about line 1, and a caller who windows onto the corruption is
//! told where it is rather than told the file is unreadable.
//!
//! It is not asked by extension. A name is a claim and the bytes are the fact: `notes.txt`
//! full of NULs is not text, and a source file with no extension at all is.
//!
//! What the first bytes *are* used for is the sentence a refusal is written in — see
//! [`format_of`]. A PDF refused as `not text: this is a PDF` tells a caller what they are
//! holding; the same PDF refused as a byte that would not decode tells them the number of a
//! byte they cannot see.

use std::{
    fs::File,
    io::{BufRead, BufReader, ErrorKind},
    path::Path,
};

/// The longest line this prints, in characters.
///
/// A line past this is shown up to here and marked. The cut is in characters and not bytes
/// because it is a line being read: cutting UTF-8 by the byte is how a limit lands in the
/// middle of a character and turns a legible line into a broken one.
pub const MAX_LINE_CHARS: usize = 10_000;

/// What this marks a line it cut with.
pub const TRUNCATED: &str = " ... [line truncated]";

/// The most of one line this will hold, in bytes.
///
/// Four bytes per character is the most UTF-8 spends on one, so a line short enough to print
/// whole is never cut by this — which is what keeps the byte bound off the answer and on the
/// memory it is there to bound.
const MAX_LINE_BYTES: usize = 4 * MAX_LINE_CHARS;

/// A file, as much of it as was asked for.
#[derive(Debug)]
pub struct Window {
    /// The lines, numbered — `cat -n`, and the same spelling every tool that shows a file
    /// with numbers uses, so a caller quoting one back is quoting a line number.
    pub shown: String,
    /// Which line `shown` starts at — the offset that was asked for, whether or not the file
    /// is that long.
    pub first: usize,
    /// How many lines are in `shown`.
    pub count: usize,
    /// How many lines the file has.
    pub total: usize,
}

/// `limit` lines of `path` from line `offset`, and the file's length in lines.
///
/// Both are counted from 1, as the numbers in the answer are: a caller who reads line 2000
/// here and asks for `offset` 2001 next gets line 2001 and not the one after it.
///
/// Refusals are [`std::io::Error`] rather than a type of this module's own, because all but
/// one of them already are — the file that is not there, the one that cannot be opened, the
/// directory — and a caller that has to spell two kinds of failure spells the interesting one
/// worse. The one that is ours carries [`ErrorKind::InvalidData`].
pub fn read(path: &Path, offset: usize, limit: usize) -> std::io::Result<Window> {
    // Asked before opening: a directory opens perfectly well and fails on the first read, with
    // an error about a descriptor rather than about the thing the caller named.
    if std::fs::metadata(path)?.is_dir() {
        return Err(std::io::Error::new(
            ErrorKind::IsADirectory,
            "is a directory, and this answers about files",
        ));
    }

    let mut reader = BufReader::new(File::open(path)?);
    // Peeked and not consumed — `fill_buf` is what a buffered reader already did to answer the
    // first line. It decides nothing: whether this file can be shown is still asked of the
    // window's own bytes below. It is only there so that the refusal is a sentence about the
    // file the caller named rather than about a byte inside it.
    let format = reader.fill_buf().ok().and_then(format_of);
    window(reader, offset, limit, format)
}

/// The same window, over lines that are already in hand rather than on disk.
///
/// What a file is read through and what text extracted from a document is read through are
/// one function, so that a line number, a cut line and the count at the end mean exactly the
/// same thing whichever it was. `format` is what a refusal will name the source as, and is
/// `None` for text that came from something that has no format to name.
pub fn window(
    mut reader: impl BufRead,
    offset: usize,
    limit: usize,
    format: Option<&'static str>,
) -> std::io::Result<Window> {
    let last = offset.saturating_add(limit.saturating_sub(1));
    let mut buf = Vec::new();
    let mut shown = String::new();
    let mut count = 0;
    let mut total = 0;

    loop {
        let number = total + 1;
        let wanted = number >= offset && number <= last;
        buf.clear();
        // A line nobody will see is read into nowhere: `cap` of zero keeps the newline
        // hunting and drops the bytes, which is what makes an offset deep in a large file
        // cost time and not memory.
        let cap = if wanted { MAX_LINE_BYTES } else { 0 };
        let Some(raw) = read_line(&mut reader, &mut buf, cap)? else {
            break;
        };
        total = number;
        if !wanted {
            continue;
        }

        // A `\r` before the newline is the line ending of a file written on another system,
        // not a character of the line. Left in, it prints as nothing and turns every
        // comparison a caller makes against what they read into a comparison that fails for
        // a reason they cannot see. Only when the line was kept whole: a `\r` at the byte
        // bound is in the middle of a line, wherever it came from.
        let kept = match buf.last() {
            Some(b'\r') if !raw.cut(&buf) => &buf[..buf.len() - 1],
            _ => &buf[..],
        };
        let line = text(kept, raw.cut(&buf), number, format)?;

        let mut chars = line.chars();
        let head: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
        // Cut by either bound is cut, and says so the same way: what the mark tells a caller
        // is that the line goes on, which is the same fact whichever limit stopped it.
        let more = raw.cut(&buf) || chars.next().is_some();

        // Right-aligned in six columns and a tab, which is `cat -n`. A line number wider than
        // six columns pushes its own line right rather than being cut: the number is the part
        // of this that has to stay true.
        shown.push_str(&format!(
            "{number:>6}\t{head}{}\n",
            if more { TRUNCATED } else { "" }
        ));
        count += 1;
    }

    Ok(Window {
        shown,
        first: offset,
        count,
        total,
    })
}

/// One line as it was on disk: how long, and how it ended.
struct Raw {
    /// Bytes it took up, the newline included.
    len: u64,
    /// Whether there was a newline — false only for the last line of a file that has none.
    newline: bool,
}

impl Raw {
    /// Whether `kept` is less than the line was.
    ///
    /// Asked of the bytes that were kept rather than remembered as a flag, so that it stays
    /// true of what is about to be printed: the cut is exact, and a line that happens to be
    /// exactly [`MAX_LINE_BYTES`] long is not marked as going on when it does not.
    fn cut(&self, kept: &[u8]) -> bool {
        self.len - u64::from(self.newline) > kept.len() as u64
    }
}

/// The next line out of `reader`, keeping at most `cap` bytes of it in `keep`.
///
/// `None` at the end of the file, which is the one thing the caller's loop asks about. What
/// comes back otherwise is the line's size on disk rather than what was kept, because those
/// differing is exactly what "there is more of this line" means.
///
/// This is [`BufRead::read_until`] with a bound. The bound is the whole reason it is written
/// out: `read_until` grows its buffer to the length of the line, so one file with no newline
/// in it is a caller naming this program's memory.
fn read_line(
    reader: &mut impl BufRead,
    keep: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Option<Raw>> {
    let mut len = 0u64;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            // What `read_until` does with it, for the reason it does: a signal arriving mid
            // read is not the file saying anything.
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(match len {
                0 => None,
                len => Some(Raw {
                    len,
                    newline: false,
                }),
            });
        }

        let (used, newline) = match available.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        let body = &available[..used - usize::from(newline)];
        if keep.len() < cap {
            let room = cap - keep.len();
            keep.extend_from_slice(&body[..room.min(body.len())]);
        }
        len += used as u64;
        reader.consume(used);
        if newline {
            return Ok(Some(Raw { len, newline }));
        }
    }
}

/// `bytes` as the line they spell, or the refusal that says this is not text.
///
/// `cut` is what makes the difference between a file that is not text and a line that was
/// stopped mid-character: the bytes end where a bound put the end, so an incomplete character
/// at the tail of a cut line is this module's doing and gets dropped, where the same bytes in
/// a line that ended by itself are a file that does not decode.
fn text<'a>(
    bytes: &'a [u8],
    cut: bool,
    number: usize,
    format: Option<&'static str>,
) -> std::io::Result<&'a str> {
    let line = match std::str::from_utf8(bytes) {
        Ok(line) => line,
        Err(e) if cut && e.error_len().is_none() => {
            // Valid up to the last whole character. `from_utf8` on a prefix of bytes it has
            // already declared valid cannot fail.
            std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("a valid prefix")
        }
        Err(e) => {
            return Err(not_text(format, || {
                format!(
                    "line {number} is not UTF-8 (at byte {} of it)",
                    e.valid_up_to()
                )
            }));
        }
    };

    // A NUL decodes as a character, so UTF-8 alone lets a binary through whenever its bytes
    // happen to be valid. It is also what nothing that prints lines can print: a caller
    // handed one has a line whose length on screen is not its length.
    if let Some(at) = line.find('\0') {
        return Err(not_text(format, || {
            format!("line {number} has a NUL byte (at byte {at} of it)")
        }));
    }
    Ok(line)
}

/// The one refusal that is this module's rather than the filesystem's.
///
/// What the file is, when its first bytes say so, and where the trouble is when they do not.
/// A caller told `this is a PDF` knows what to do next; the same caller told which byte of
/// which line failed to decode knows only that something went wrong somewhere they cannot
/// see. The byte is still what is said about a file nothing recognises, because then it is
/// the only true thing there is to say.
fn not_text(format: Option<&'static str>, byte: impl FnOnce() -> String) -> std::io::Error {
    let said = match format {
        Some(what) => format!("this is {what}"),
        None => byte(),
    };
    std::io::Error::new(ErrorKind::InvalidData, format!("not text: {said}"))
}

/// What a file's first bytes say it is, in the words a refusal is written in.
///
/// A short table and not a library of magic numbers, because this is not a classifier: it
/// changes no outcome, and every format on it is one somebody plausibly runs this on by
/// mistake. A format missing from it costs the caller a less specific sentence and nothing
/// else, which is what keeps the list from being a thing to maintain.
fn format_of(head: &[u8]) -> Option<&'static str> {
    const KNOWN: &[(&[u8], &str)] = &[
        // A PDF only reaches here in a build with no engine in it — with one it is read
        // rather than named, so this says which of the two the caller is looking at.
        #[cfg(not(feature = "pdfium"))]
        (
            b"%PDF-",
            "a PDF, and this build has no engine to read one with",
        ),
        #[cfg(feature = "pdfium")]
        (b"%PDF-", "a PDF, and it does not open as one"),
        // Likewise a Word document, which is read where the `docx` feature is on. A zip that
        // gets this far is then one of the others that are also zips.
        #[cfg(feature = "docx")]
        (
            b"PK\x03\x04",
            "a zip and not a Word document — .xlsx and .pptx are zips too",
        ),
        #[cfg(not(feature = "docx"))]
        (
            b"PK\x03\x04",
            "a zip — which is also what .docx, .xlsx and .pptx are",
        ),
        (b"\x1f\x8b", "gzip-compressed"),
        (b"\x7fELF", "an ELF executable"),
        (b"\x89PNG", "a PNG"),
        (b"\xff\xd8\xff", "a JPEG"),
        (b"GIF8", "a GIF"),
        // A `mem` or `index` store. Those programs are how one is asked anything, and a
        // caller who got here was reaching for what they answer.
        (
            b"SQLite format 3\0",
            "a SQLite database — `mem` and `index` are what answer about those",
        ),
    ];
    KNOWN
        .iter()
        .find(|(magic, _)| head.starts_with(magic))
        .map(|&(_, name)| name)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A file holding exactly these bytes, and the path to it.
    fn file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut file = File::create(&path).expect("a file");
        file.write_all(bytes).expect("the bytes");
        path
    }

    /// The last line of a file that ends without one is a line, and the count says so —
    /// otherwise a file with no trailing newline is a file whose last line cannot be asked
    /// for.
    #[test]
    fn a_file_that_ends_without_a_newline_still_ends_with_a_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let window = read(&file(&dir, "a", b"one\ntwo"), 1, 10).expect("the file reads");
        assert_eq!(window.total, 2);
        assert_eq!(window.shown, "     1\tone\n     2\ttwo\n");
    }

    /// A trailing newline ends the last line rather than starting another one.
    #[test]
    fn a_trailing_newline_is_not_a_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let window = read(&file(&dir, "a", b"one\ntwo\n"), 1, 10).expect("the file reads");
        assert_eq!(window.total, 2);
    }

    /// An empty line between two others is a line: the numbers have to keep up with the file
    /// or every number after the first blank is wrong.
    #[test]
    fn a_blank_line_is_a_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let window = read(&file(&dir, "a", b"one\n\nthree\n"), 1, 10).expect("the file reads");
        assert_eq!(window.shown, "     1\tone\n     2\t\n     3\tthree\n");
    }

    /// A file written on another system reads as the lines it holds, not as lines with an
    /// invisible character on the end of each.
    #[test]
    fn a_carriage_return_before_the_newline_is_the_line_ending() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let window = read(&file(&dir, "a", b"one\r\ntwo\r\n"), 1, 10).expect("the file reads");
        assert_eq!(window.shown, "     1\tone\n     2\ttwo\n");
    }

    /// The window is what is held: a line outside it is walked past, however long it is, and
    /// the one inside it is cut at the character bound and marked.
    #[test]
    fn a_long_line_is_cut_and_a_long_line_outside_the_window_is_not_held() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let mut bytes = vec![b'x'; MAX_LINE_BYTES * 2];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"second\n");
        let path = file(&dir, "a", &bytes);

        let window = read(&path, 1, 1).expect("the file reads");
        assert_eq!(window.total, 2, "both lines were counted");
        assert_eq!(window.count, 1);
        assert!(window.shown.ends_with(&format!("{TRUNCATED}\n")), "cut");
        assert_eq!(
            window.shown.chars().filter(|&c| c == 'x').count(),
            MAX_LINE_CHARS,
            "cut at the character bound, not the byte one"
        );

        // The window past it holds the short line and none of the long one.
        let window = read(&path, 2, 1).expect("the file reads");
        assert_eq!(window.shown, "     2\tsecond\n");
    }

    /// A character cut in half by the byte bound is dropped rather than making the file
    /// undecodable: the split is this module's, and a caller cannot act on it.
    #[test]
    fn a_character_split_by_the_byte_bound_is_not_a_file_that_is_not_text() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        // Three-byte characters do not divide the bound evenly, so the last one kept
        // straddles it.
        let line: String = std::iter::repeat_n('한', MAX_LINE_BYTES).collect();
        let window = read(&file(&dir, "a", line.as_bytes()), 1, 1).expect("the file reads");
        assert!(window.shown.contains('한'));
        assert!(window.shown.ends_with(&format!("{TRUNCATED}\n")));
    }

    /// The rule the window makes: what is being shown has to be text, and what is not being
    /// shown only has to be counted.
    #[test]
    fn only_the_window_has_to_be_text() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(&dir, "a", b"one\n\xff\xfe\nthree\n");

        let e = read(&path, 1, 10).expect_err("line 2 is in the window");
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("line 2"), "{e}");

        let window = read(&path, 3, 1).expect("line 3 is text");
        assert_eq!(window.shown, "     3\tthree\n");
        assert_eq!(window.total, 3, "the line that is not text is still a line");
    }

    /// A file whose first bytes name it is refused as what it is. The line and the byte are
    /// true and useless here: what a caller holding a PDF has to know is that it is one.
    #[test]
    fn a_format_that_can_be_named_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pdf = b"%PDF-1.4\n%\xc7\xec\x8f\xa2\n1 0 obj\n<<>>\nendobj\n";
        let e = read(&file(&dir, "paper.pdf", pdf), 1, 10).expect_err("a PDF is not text");
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("this is a PDF"), "{e}");

        // A store is the case worth naming most: the caller was reaching for `mem` or
        // `index`, and the refusal says so.
        let e = read(
            &file(&dir, "notes.db", b"SQLite format 3\0the rest is pages"),
            1,
            10,
        )
        .expect_err("a store is not text");
        assert!(e.to_string().contains("SQLite"), "{e}");
    }

    /// A file nothing recognises is still refused, and then the byte is the only true thing
    /// there is to say about it.
    #[test]
    fn a_format_with_no_name_is_refused_by_the_byte() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let e = read(&file(&dir, "a", b"one\n\xff\xfe\n"), 1, 10).expect_err("line 2 is not text");
        assert!(e.to_string().contains("line 2"), "{e}");
    }

    /// A NUL is not text either, whatever UTF-8 makes of it.
    #[test]
    fn a_nul_is_not_text() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let e = read(&file(&dir, "a", b"one\ntw\0o\n"), 1, 10).expect_err("line 2 holds a NUL");
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("NUL"), "{e}");
    }

    /// A directory is refused as what it is, rather than as a read that failed.
    #[test]
    fn a_directory_is_not_a_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let e = read(dir.path(), 1, 10).expect_err("a directory is not a file");
        assert_eq!(e.kind(), ErrorKind::IsADirectory);
    }
}
