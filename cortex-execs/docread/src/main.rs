//! `docread` — a file's lines, numbered, a window at a time.
//!
//! ```text
//! docread src/main.rs
//! docread src/main.rs -o 200 -n 50
//! ```
//!
//! # Lines, because a line is what a caller can name
//!
//! The console protocol already reads files, and it reads them by the byte: an offset and a
//! length, which is what moving a file's contents from one side of a session to the other
//! needs and all it needs. Nothing about those bytes has to mean anything to anybody.
//!
//! This is the other kind of read — the one whose answer is about to be *acted on*. What
//! comes back from that has to be quotable: an argument to make about line 412, an edit to
//! ask for at line 87, a second window starting where the first one stopped. Bytes are none
//! of those. A byte offset into a file is a number nobody can say out loud, it moves the
//! moment a character above ASCII appears earlier in the file, and a window cut by it starts
//! and ends mid-line. So this counts lines, prints their numbers, and takes them back.
//!
//! That is the whole of what "semantic" means here, and it is deliberately not more: there is
//! no notion of a function, a section, or a symbol below. Those need a parser per language
//! and are wrong in a way a line number cannot be — a window that claims to be a function and
//! is not is worse than one that claims to be lines 100 to 150.
//!
//! # The window is bounded before the caller says anything
//!
//! [`DEFAULT_LIMIT`] lines, unasked. A file is as long as it is and the thing reading this is
//! working from a fixed amount of room, so an unbounded read is a read that either fits by
//! luck or costs everything downstream. `-n` raises it for the caller who knows what they are
//! asking for.
//!
//! Which makes what was *left out* part of the answer. When the window is not the whole file,
//! the last line says so and says where the rest starts, so a caller reading the output has
//! the next command in front of them:
//!
//! ```text
//! [lines 1-2000 of 4210; the rest starts at --offset 2001]
//! ```
//!
//! And when it is the whole file, there is no such line. Silence is the statement that
//! nothing is missing — the one thing a caller most needs to know and would otherwise have to
//! work out by comparing two numbers.
//!
//! # What it does not do
//!
//! It does not search — that is `index`, which answers with paths, and this is what to run on
//! one of them. It does not write, so there is no file this program can be the reason for
//! losing. And it answers about text: a window on bytes that do not decode is refused at the
//! line that does not, because printing them would mean handing back a line whose length on
//! screen is not its length and whose number is the only true thing on it.
//!
//! # Documents, which hold text without being text
//!
//! Two of them are read rather than refused: a Word document always ([`docx`]), and a PDF in
//! a build that has an engine linked in ([`pdf`], the `pdfium` feature). The rest — a
//! `.pptx`, a spreadsheet — are named and refused, and become readable here the way anything
//! else does: by something turning them into a file that has lines.
//!
//! **What comes back from a document is not the file's lines**, and the answer says so on its
//! own line before the first of them. A paragraph is a unit somebody wrote and a PDF's line is
//! one an engine guessed at, so the two are worth different amounts — but neither is a line of
//! the file, and a number quoted out of one means what it means only as long as the rules that
//! produced it hold. Those rules are written down in each of those modules.

#[cfg(feature = "docx")]
mod docx;
#[cfg(feature = "pdfium")]
mod pdf;
mod text;

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser as _;

use crate::text::Window;

/// The name every message this program writes about itself is spelled with.
///
/// Taken from the target rather than written out, so that it is the name a caller actually
/// typed and the same one clap puts in the usage line.
const NAME: &str = env!("CARGO_BIN_NAME");

/// How many lines a window holds when the caller does not say.
///
/// Long enough that most files are one window and the note at the end never appears; short
/// enough that a file which is not fits in what is reading it anyway.
const DEFAULT_LIMIT: usize = 2000;

/// What `docread` accepts and what it says about it.
#[derive(Debug, clap::Parser)]
#[command(
    about = "docread — a file's lines, numbered, a window at a time",
    after_help = "Lines are counted from 1, and the numbers printed are the ones -o takes back. \
                  Output ends with a bracketed line only when the window is not the whole file; \
                  no such line means there is nothing else in it.",
    // The doc comments here are for whoever reads this file. What a caller of the command sees
    // is `about` and the first line of each item's; nothing below argues a design decision at
    // somebody who typed `--help`.
    long_about = None,
    // Bare `docread` is a line that asked for nothing, and the help is the useful answer
    // to it.
    arg_required_else_help = true
)]
struct Cli {
    /// the file to read
    ///
    /// One file and not several. Two files' lines under one set of numbers would be an answer
    /// in which a line number means nothing without a header saying which file it is of — and
    /// a caller who wants that has `cat`, which is not pretending the result is addressable.
    #[arg(value_name = "FILE", long_help = None)]
    file: PathBuf,

    /// the line to start at
    ///
    /// The line number as printed, so reading the note at the end of one window and typing the
    /// number it names is how the next one is asked for.
    #[arg(
        short = 'o',
        long,
        value_name = "N",
        default_value_t = 1,
        value_parser = line_number,
        long_help = None
    )]
    offset: usize,

    /// how many lines to show
    ///
    /// A count and not an end line, because what bounds the answer is how much of it there is
    /// room for — which is a number the caller knows about themselves and not about the file.
    #[arg(
        short = 'n',
        long,
        value_name = "N",
        default_value_t = DEFAULT_LIMIT,
        value_parser = count,
        long_help = None
    )]
    limit: usize,
}

/// A line number the file could have: one and up.
///
/// Refused at the parser rather than quietly read as 1, which is what a program that took a 0
/// would be doing. Lines here are numbered from 1 everywhere they are printed, and a caller
/// who typed 0 is either counting from somewhere else or arrived here by arithmetic — both
/// worth a `2` and a sentence naming the argument.
fn line_number(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("lines are counted from 1, so there is no line 0".into()),
        Ok(number) => Ok(number),
        Err(e) => Err(e.to_string()),
    }
}

/// A window's size: at least one line.
fn count(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("a window of no lines would answer nothing".into()),
        Ok(count) => Ok(count),
        Err(e) => Err(e.to_string()),
    }
}

/// The line a read that could not be carried out leaves on stderr.
///
/// One place that spells this program's failures, with the file named the way the caller named
/// it: a path they typed is the one thing in the message they can act on.
fn refused(file: &Path, said: impl std::fmt::Display) -> String {
    format!("{NAME}: {}: {said}\n", file.display())
}

/// `n` lines, spelled as a count of them.
fn lines(n: usize) -> String {
    match n {
        1 => "1 line".to_owned(),
        n => format!("{n} lines"),
    }
}

/// What the window is, said: the lines, and what is missing from them.
///
/// Every case where the answer is not simply the file gets a line of its own, because each is
/// a different next command — a window with more after it, an offset past the end, and a file
/// with nothing in it are three things a caller does three different things about, and all
/// three look identical as bare output.
fn said(window: Window, origin: Option<&'static str>) -> String {
    let Window {
        mut shown,
        first,
        count,
        total,
    } = window;

    // Said before the lines rather than after them, and only when the lines are not the
    // file's own. A caller who reads the numbers first and the note last has already quoted
    // one somewhere, and what this warns about is exactly that quoting.
    let origin = match origin {
        Some(origin) => format!("[these lines are {origin}]\n"),
        None => String::new(),
    };
    shown.insert_str(0, &origin);

    // A file with nothing in it. Otherwise this is a command that printed nothing and
    // succeeded, which is what a failure nobody noticed also looks like.
    if total == 0 {
        return format!("{origin}[the file is empty]\n");
    }

    // A window that begins past the end. The file's length is the useful half of saying so:
    // it is what the caller's next offset has to be under.
    if count == 0 {
        return format!(
            "{origin}[the file has {}; there is nothing at line {first}]\n",
            lines(total)
        );
    }

    let last = first + count - 1;
    if first > 1 || last < total {
        shown.push_str(&format!("[lines {first}-{last} of {total}"));
        // Where to pick up. The number rather than the whole command, so that a caller who
        // spelled it `--offset` and a caller who spelled it `-o` both read something true.
        if last < total {
            shown.push_str(&format!("; the rest starts at --offset {}", last + 1));
        }
        shown.push_str("]\n");
    }
    shown
}

/// The command, done: what to say on stdout, or the line to say on stderr instead.
///
/// Every way this can end is decided here rather than on the way out, because what a reader
/// wants to know about a message is which case produced it. A line that was not understood
/// never reaches here, clap having already answered it.
fn run(cli: Cli) -> Result<String, String> {
    let (window, origin) =
        read(&cli.file, cli.offset, cli.limit).map_err(|e| refused(&cli.file, e))?;
    Ok(said(window, origin))
}

/// The file, windowed — and where its lines came from, when they are not the file's own.
///
/// The documents come first and the text reader last, because a document is only a document
/// when its bytes say so: each of these asks the file what it is and steps aside otherwise, so
/// a build with neither feature is [`text::read`] and nothing else. Nothing looks at the
/// extension, for the reason every question about a file's contents is asked of its bytes: the
/// name is a claim.
///
/// **Asking has to be cheaper than reading**, because these questions are asked of every file
/// and answered `no` about nearly all of them. The one below is bounded on purpose ([`docx`],
/// and see [`docx::read`]): a window onto two lines of a gigabyte of text costs a peek here,
/// or else the reader whose whole design is to hold the window and walk the rest is bounded by
/// a module that already read the file.
fn read(
    file: &Path,
    offset: usize,
    limit: usize,
) -> std::io::Result<(text::Window, Option<&'static str>)> {
    #[cfg(feature = "pdfium")]
    if starts_with(file, pdf::MAGIC) {
        return pdf::read(file, offset, limit).map(|window| (window, Some(pdf::ORIGIN)));
    }

    #[cfg(feature = "docx")]
    if let Some(read) = docx::read(file, offset, limit) {
        return read.map(|window| (window, Some(docx::ORIGIN)));
    }

    text::read(file, offset, limit).map(|window| (window, None))
}

/// Whether `file` begins with `magic`.
///
/// A failure to look is a `false` and not a refusal: whatever is wrong with the file, the read
/// that follows meets it too and says so about the file rather than about a peek.
#[cfg(feature = "pdfium")]
fn starts_with(file: &Path, magic: &[u8]) -> bool {
    use std::io::Read as _;

    let mut head = vec![0; magic.len()];
    std::fs::File::open(file)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && head == magic
}

/// A line in, and what it produced out.
///
/// No runtime: the one thing this program waits on is a file, which it waits on in this
/// thread.
fn main() -> ExitCode {
    // `parse` and not a result to inspect: a line that was not understood, and `--help`, are
    // both clap's to answer — help on stdout with a `0`, usage on stderr with a `2`, which is
    // what a caller reading one out of a pipe expects of any program.
    let said = match run(Cli::parse()) {
        Ok(said) => said,
        Err(said) => {
            // Straight out, because this is the last thing that happens.
            std::io::stderr().write_all(said.as_bytes()).ok();
            return ExitCode::FAILURE;
        }
    };

    // Written as bytes: this is program output on its way to whatever the shell pointed at,
    // and a caller piping it into something byte-oriented has to get what was produced.
    match std::io::stdout().write_all(said.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        // A reader that stopped reading — `| head` — is not this program failing.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        // Anything else and the answer is not the answer: a caller cannot be told that by a
        // zero, which is what says the window held everything it was going to.
        Err(e) => {
            std::io::stderr()
                .write_all(format!("{NAME}: writing the answer: {e}\n").as_bytes())
                .ok();
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        io::Write as _,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use clap::error::ErrorKind;
    use tempfile::TempDir;

    use super::*;

    /// The largest single allocation this test binary has made.
    ///
    /// What is being watched is a file's size turning into memory, and that has one shape: a
    /// `Vec` as long as the file, allocated in one go and grown by doubling. So the largest
    /// allocation is the whole of the number worth having, and it needs no allocator that
    /// tracks what is live — a `fetch_max` on the way past.
    ///
    /// It is global because an allocator is: every test here is measured whether it looks or
    /// not, and they run at the same time. Which is why what is asserted below is a fraction
    /// of the file that was written rather than a tight number — everything else running
    /// allocates kilobytes, and what this is here to catch is a megabyte per megabyte of file.
    static LARGEST: AtomicUsize = AtomicUsize::new(0);

    /// [`System`], counted.
    struct Counted;

    #[global_allocator]
    static ALLOCATOR: Counted = Counted;

    unsafe impl GlobalAlloc for Counted {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
            unsafe { System.alloc_zeroed(layout) }
        }

        // Counted too, and not only for tidiness: a `Vec` that was not told how long the file
        // is arrives at the file's length through this and never through `alloc`.
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            LARGEST.fetch_max(size, Ordering::Relaxed);
            unsafe { System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    /// Through the same parser the program runs on, so that what these pin is what runs. The
    /// program's own name in front, because that is what `argv` carries.
    fn parse(line: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once(NAME).chain(line.iter().copied()))
    }

    /// A line, carried out, as typing it would.
    fn run_line(line: &[&str]) -> Result<String, String> {
        run(parse(line).expect("the line parses"))
    }

    /// A file holding `text`, named absolutely: a relative path would resolve against a
    /// working directory the whole test binary shares.
    fn file(dir: &TempDir, name: &str, text: &str) -> String {
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .expect("a file")
            .write_all(text.as_bytes())
            .expect("the text");
        path.to_str()
            .expect("a temporary directory is UTF-8")
            .to_owned()
    }

    /// A file with `n` numbered lines, so that a window's contents say where it landed.
    fn numbered(dir: &TempDir, name: &str, n: usize) -> String {
        let text: String = (1..=n).map(|i| format!("line {i}\n")).collect();
        file(dir, name, &text)
    }

    /// The whole thing, `cat -n`, and nothing after it: a file that fits in one window is
    /// answered with the file, and the absence of a note is what says so.
    #[test]
    fn a_file_that_fits_is_the_whole_answer() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = numbered(&dir, "a.txt", 3);

        assert_eq!(
            run_line(&[&path]).expect("the file reads"),
            "     1\tline 1\n     2\tline 2\n     3\tline 3\n"
        );
    }

    /// A window, and the note that makes the next one obvious: where it starts, how long the
    /// file is, and the offset to type.
    #[test]
    fn a_window_says_what_it_left_out() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = numbered(&dir, "a.txt", 10);

        let said = run_line(&[&path, "-n", "2"]).expect("the file reads");
        assert_eq!(
            said,
            "     1\tline 1\n     2\tline 2\n\
             [lines 1-2 of 10; the rest starts at --offset 3]\n"
        );

        // And typing that offset picks up at the line after the last one shown.
        let said = run_line(&[&path, "-o", "3", "-n", "2"]).expect("the file reads");
        assert!(said.starts_with("     3\tline 3\n"), "{said}");

        // A window that reaches the end still says where it began, and names no next offset:
        // there is nothing to pick up.
        let said = run_line(&[&path, "-o", "9"]).expect("the file reads");
        assert_eq!(
            said,
            "     9\tline 9\n    10\tline 10\n[lines 9-10 of 10]\n"
        );
    }

    /// An offset past the end is an answer about the file rather than an empty one: the length
    /// is what tells the caller which offset would have worked.
    #[test]
    fn an_offset_past_the_end_says_how_long_the_file_is() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = numbered(&dir, "a.txt", 3);

        assert_eq!(
            run_line(&[&path, "-o", "50"]).expect("the file reads"),
            "[the file has 3 lines; there is nothing at line 50]\n"
        );
    }

    /// A file with nothing in it says so, rather than being a command that printed nothing.
    #[test]
    fn an_empty_file_says_it_is_empty() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = file(&dir, "a.txt", "");

        assert_eq!(
            run_line(&[&path]).expect("the file reads"),
            "[the file is empty]\n"
        );
    }

    /// A long line is cut and marked, and the number in front of it is still the line's.
    #[test]
    fn a_long_line_is_cut_where_it_is_marked() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let long: String = std::iter::repeat_n('x', text::MAX_LINE_CHARS + 100).collect();
        let path = file(&dir, "a.txt", &format!("{long}\nshort\n"));

        let said = run_line(&[&path]).expect("the file reads");
        let first = said.lines().next().expect("a line");
        assert!(first.ends_with(text::TRUNCATED), "{first}");
        assert_eq!(
            first.chars().filter(|&c| c == 'x').count(),
            text::MAX_LINE_CHARS
        );
        assert!(said.contains("     2\tshort"), "the file goes on: {said}");
    }

    /// Bytes that are not text end the command and print nothing: half a binary on stdout
    /// would be an answer a caller has no way to read as a failure.
    #[test]
    fn a_file_that_is_not_text_is_refused_and_nothing_is_printed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"\x7fELF\x02\x01\x01\x00\x00\x00").expect("a binary");
        let path = path.to_str().expect("UTF-8").to_owned();

        let said = run_line(&[&path]).expect_err("this is not text");
        assert!(
            said.contains(&path),
            "the file is named as the caller named it: {said}"
        );
        assert!(said.contains("not text"), "{said}");
    }

    /// A file that is not there ends the line, named the way it was typed.
    #[test]
    fn a_file_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("missing.txt");
        let path = path.to_str().expect("UTF-8").to_owned();

        let said = run_line(&[&path]).expect_err("there is no such file");
        assert!(said.contains(&path), "{said}");
        assert!(
            said.starts_with(NAME),
            "the program says which program it is: {said}"
        );
    }

    /// A directory is refused as a directory, which is what a caller who typed one needs to
    /// read.
    #[test]
    fn a_directory_is_not_a_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().to_str().expect("UTF-8").to_owned();

        let said = run_line(&[&path]).expect_err("a directory is not a file");
        assert!(said.contains("is a directory"), "{said}");
    }

    /// The defaults are the whole of what a caller has to know to type this: a path.
    #[test]
    fn a_file_is_all_this_needs_to_be_told() {
        let cli = parse(&["src/main.rs"]).expect("a file is all it needs");
        assert_eq!(cli.file, Path::new("src/main.rs"));
        assert_eq!(cli.offset, 1);
        assert_eq!(cli.limit, DEFAULT_LIMIT);
    }

    /// Counting from 1 is the rule, and a 0 is a line that was not understood rather than a
    /// read that quietly happened somewhere else. A `2` and not a `1`, and clap names the
    /// argument.
    #[test]
    fn there_is_no_line_zero_and_no_empty_window() {
        for line in [["a.txt", "-o", "0"], ["a.txt", "-n", "0"]] {
            let e = parse(&line).expect_err("0 is not a number this takes");
            assert_eq!(e.kind(), ErrorKind::ValueValidation);
            assert_eq!(e.exit_code(), 2);
        }
    }

    /// Help is an answer, not a refusal: a zero, so a caller can pipe it, and the name they
    /// typed in the usage line.
    #[test]
    fn help_is_an_answer() {
        let e = parse(&["--help"]).expect_err("help is not a read");
        assert_eq!(e.kind(), ErrorKind::DisplayHelp);
        assert_eq!(e.exit_code(), 0);

        // The name this was built as, not one written out here: the usage line has to tell a
        // caller a command they actually have.
        let said = e.render().to_string();
        assert!(said.contains(&format!("Usage: {NAME}")), "{said}");
        for flag in ["--offset", "--limit", "FILE"] {
            assert!(said.contains(flag), "{flag} is missing from: {said}");
        }
    }

    /// What came out of a document says so before its first line, so that a caller quoting a
    /// number has already read what the number is a number of.
    #[cfg(feature = "docx")]
    #[test]
    fn a_document_says_that_its_lines_are_its_own() {
        use docx_rs::{Docx, Paragraph, Run};

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.docx");
        Docx::new()
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("A paragraph")))
            .build()
            .pack(std::fs::File::create(&path).expect("a file"))
            .expect("a document");
        let path = path.to_str().expect("UTF-8").to_owned();

        let said = run_line(&[&path]).expect("the document reads");
        assert!(
            said.starts_with(&format!("[these lines are {}]\n", docx::ORIGIN)),
            "{said}"
        );
        assert!(said.contains("     1\tA paragraph\n"), "{said}");
    }

    /// A file is not held in memory because it was looked at.
    ///
    /// The bound belongs to the text reader — the window is what it keeps — and what can take
    /// it away is a question asked *before* it: reading a file to decide what it is costs the
    /// file, whatever the reader would then have done. So this is a read of two lines of a
    /// file far larger than the answer, in the build that also reads Word documents, and what
    /// it asserts is that nothing on the way allocated the file's size.
    ///
    /// 64 MiB and a bound of an eighth of it: large enough that a read of the whole file is
    /// unmistakable next to what a window and a `BufReader` cost, small enough to write in a
    /// test. The number is a fraction of the file rather than a tight bound because [`LARGEST`]
    /// is global — see it for why.
    #[test]
    fn a_large_file_that_is_not_a_document_is_not_held_in_memory() {
        const SIZE: usize = 64 << 20;

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("big.txt");
        let mut file = std::fs::File::create(&path).expect("a file");
        // Written a chunk at a time, so that the fixture is not itself the allocation being
        // looked for.
        let chunk: String = std::iter::repeat_n("line...\n", (1 << 20) / 8).collect();
        for _ in 0..SIZE / chunk.len() {
            file.write_all(chunk.as_bytes()).expect("the text");
        }
        file.sync_all().expect("the file");
        let path = path.to_str().expect("UTF-8").to_owned();

        // From here on the numbers are the program's.
        LARGEST.store(0, Ordering::Relaxed);
        let said = run_line(&[&path, "-o", "5", "-n", "2"]).expect("the file reads");
        let largest = LARGEST.load(Ordering::Relaxed);

        assert!(
            said.starts_with("     5\tline...\n     6\tline...\n"),
            "{said}"
        );
        assert!(
            largest < SIZE / 8,
            "a window of 2 lines allocated {largest} bytes of a {SIZE}-byte file"
        );
    }

    /// Bare `docread` is a line that asked for nothing, and help is the useful answer to it.
    #[test]
    fn asking_for_nothing_is_answered_with_help() {
        let e = parse(&[]).expect_err("nothing was asked for");
        assert_eq!(
            e.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }
}
