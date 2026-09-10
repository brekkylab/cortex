//! A Word document as lines: one break the author made, one line.
//!
//! # Why an authored break is a line
//!
//! A `.docx` has no lines — what it has is paragraphs, and a paragraph is a unit somebody
//! wrote rather than one a renderer arrived at. Where a line of text is broken at a byte and a
//! line of a PDF is broken wherever an engine guessed the page came apart, a `<w:p>` is
//! authored: it survives being opened in another word processor, at another paper size, at
//! another zoom.
//!
//! Which is the reason a `<w:br/>` is a line here too. Shift+Enter is a break somebody typed
//! and the file remembers, as stable as the paragraph mark and shown on screen the same way,
//! so the two cannot be worth different amounts to a reader whose rule is that a line is
//! authored. `<w:cr/>` is the same break spelled another way and counts the same — see
//! [`push_run`], where both are one arm. What stays out is the *wrap*: the break a renderer
//! arrived at, which is in no file and is [`crate::pdf`]'s difficulty rather than this one's.
//!
//! So the numbers here are worth more than [`crate::pdf`]'s and still less than a text file's.
//! They are stable against everything except this module's own rules, which is why the answer
//! says where its lines came from rather than letting them pass for a file's.
//!
//! # What is in the document, and what is only in the file
//!
//! A document under revision holds both what it says and what it used to say. Word keeps a
//! deletion as `w:delText` inside `w:del` and an insertion as an ordinary run inside `w:ins`,
//! so the document *as it stands* is: insertions in, deletions out. That is what [`text_of`]
//! does, and it is a decision rather than an accident — a reader that took every run would
//! answer with sentences the author removed, and one that took only plain runs would drop the
//! newest sentence in the document.
//!
//! Tables are rows of cells and there is nothing to guess about them, so a row is a line
//! spelled `| cell | cell |`. It is not markdown and is not trying to be; it is the one
//! spelling in which a row reads as a row and the cells stay told apart. That spelling is
//! also where the rule above stops: a break inside a cell is a space, because a row torn in
//! half is not a row, and the same is already true of a cell holding two paragraphs.
//!
//! Headers, footers, footnotes and comments are not read. Each is a part of its own beside the
//! body, and a caller reading line 12 of a document means the twelfth paragraph of what is
//! written on the page — not a running header that would push every number down by one.

use std::{
    fs::File,
    io::{BufReader, Cursor, Read as _, Seek as _},
    path::Path,
};

use docx_rs::{
    DocumentChild, Insert, InsertChild, Paragraph, ParagraphChild, Run, RunChild, Table,
    TableCellContent, TableChild, TableRowChild, read_docx,
};

use crate::text::{self, Window};

/// What the answer calls these lines.
pub const ORIGIN: &str =
    "paragraphs of this Word document and the breaks inside them, not lines in the file";

/// What every zip begins with.
///
/// Asked first because it is the whole of what most files cost this module. Everything below
/// it is work about an archive, and a file that is not one — the text file, the source file,
/// the log, which is nearly everything this program is pointed at — is four bytes and gone.
/// Handed straight to a zip reader instead, it would be crawled backwards end to end looking
/// for a central directory it does not have.
const ZIP: &[u8] = b"PK\x03\x04";

/// The entry every `.docx` has and the other zips do not.
///
/// Looked up in the archive's own index rather than searched for in the file: a zip ends with
/// a directory of its entry names, so this is a seek and a few kilobytes whatever the archive
/// weighs. That it is exact is the second thing it buys — the name is an entry or it is not,
/// where a search over the bytes also finds it in a `.xlsx` that happens to embed one and in
/// prose about the format. This is what tells a Word document from the `.xlsx`, `.pptx` and
/// `.jar` that are the same container, and it is asked of the file and not of its extension,
/// like every other question here.
const BODY: &str = "word/document.xml";

/// What a zip ends with.
///
/// The end-of-central-directory record is what a zip reader looks for first, and it is asked
/// about here rather than left to [`zip`] because a reader handed a file that has no such
/// record goes looking: backwards, in windows, to the front of the file. That is a gigabyte
/// of reading to answer *no* about a gigabyte of junk that happens to begin `PK`.
///
/// Looking in the only place it can be — see [`TAIL`] — is one read instead. An archive that
/// has it is then handed over and the reader finds it in its own first window; a file that
/// does not is not an archive anything could read, and that is the answer.
const EOCD: &[u8] = b"PK\x05\x06";

/// How much of the end of a file [`EOCD`] can be in.
///
/// The record is 22 bytes and the archive's comment follows it, and the length of that
/// comment is a `u16` — so 65535 bytes and not one more, whatever the archive weighs. Which
/// is what makes the question a bounded one: 64 KiB of the tail, and never a byte of the rest.
const TAIL: u64 = 22 + u16::MAX as u64;

/// `limit` lines of `path`'s paragraphs from line `offset`, or `None` if this is not a Word
/// document at all.
///
/// `None` and not a refusal, so that the caller can go on to name what the file actually is.
/// Once the body part is known to be in there, every later failure is a refusal about a Word
/// document — which is the truth, and more use than being told it is a zip.
///
/// # The file is read whole, and only once it is known to be a document
///
/// [`read_docx`] parses out of memory, so a document costs its own size — there is no reading
/// a `.docx` a window at a time, and little point: the text is spread over compressed XML
/// that has to be unpacked to be counted, and what comes back is the whole document either
/// way.
///
/// What matters is that nothing *else* costs that. This runs before [`text::read`] on every
/// file the program is handed, so a check that read the bytes to look at them would be the
/// text reader's bound — hold the window, walk the rest — undone from underneath by the
/// module that runs first. [`is_document`] is therefore bounded on purpose, and the whole
/// read happens on the other side of it.
pub fn read(path: &Path, offset: usize, limit: usize) -> Option<std::io::Result<Window>> {
    // A file that will not open at all is not this module's to refuse: whatever is wrong with
    // it — a directory, a permission — the read that follows meets the same thing and says it
    // about the file rather than about a look inside a zip.
    let Ok(mut file) = File::open(path) else {
        return None;
    };
    if !is_document(BufReader::new(&file)) {
        return None;
    }
    Some(
        bytes(&mut file)
            .and_then(|bytes| lines(&bytes))
            .and_then(|text| {
                // Through the same window as every other read, so a number, a cut line and the
                // count at the end mean what they mean everywhere else.
                text::window(Cursor::new(text.into_bytes()), offset, limit, None)
            }),
    )
}

/// Whether this is a Word document, out of a peek and an index.
///
/// Two questions, neither of which is the file's length: the four bytes that say it is a zip,
/// and then the archive's directory, which lists what is in it without any of it being
/// unpacked. What that costs is a read of the head and a read of the tail.
///
/// A zip that will not open is a `false` and not a document that failed. It is the honest
/// answer — an archive whose directory is unreadable is one nothing can say the contents of —
/// and it leaves the file to be named as the zip it is, which is what [`text::read`] does with
/// it. The refusal about a document that will not parse is still there for the case it is
/// about: a readable archive holding a body part that the parser then chokes on.
fn is_document(mut reader: impl std::io::Read + std::io::Seek) -> bool {
    let mut head = [0; ZIP.len()];
    // A file shorter than the magic is not a zip, which is what the failure means here.
    if reader.read_exact(&mut head).is_err() || head != *ZIP {
        return false;
    }
    if !ends_with_a_directory(&mut reader) {
        return false;
    }
    // Wherever the reads above left the cursor: an archive is read from its end, and
    // `ZipArchive` seeks there itself.
    zip::ZipArchive::new(reader).is_ok_and(|archive| archive.index_for_name(BODY).is_some())
}

/// Whether the tail of `reader` holds the record a zip ends with. See [`EOCD`].
fn ends_with_a_directory(reader: &mut (impl std::io::Read + std::io::Seek)) -> bool {
    let Ok(len) = reader.seek(std::io::SeekFrom::End(0)) else {
        return false;
    };
    if reader
        .seek(std::io::SeekFrom::Start(len.saturating_sub(TAIL)))
        .is_err()
    {
        return false;
    }
    // Bounded by `TAIL` and by nothing else, which is the point of the function.
    let mut tail = Vec::new();
    reader.take(TAIL).read_to_end(&mut tail).is_ok()
        && tail.windows(EOCD.len()).any(|bytes| bytes == EOCD)
}

/// The file, from the top, all of it.
///
/// Rewound rather than reopened: the file that was decided about is the file that is read,
/// and a second `open` of the same path is a second chance for it to have become another one.
fn bytes(file: &mut File) -> std::io::Result<Vec<u8>> {
    file.rewind()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The document's text, a paragraph — or a piece of one a break cut off — or a row per line.
fn lines(bytes: &[u8]) -> std::io::Result<String> {
    let docx = read_docx(bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("this is a Word document and it will not parse: {e}"),
        )
    })?;

    let mut out = String::new();
    for child in &docx.document.children {
        match child {
            DocumentChild::Paragraph(paragraph) => {
                out.push_str(&text_of(paragraph));
                out.push('\n');
            }
            DocumentChild::Table(table) => rows_of(table, &mut out),
            // A content control, which is where the body of a document made from a template
            // is. Passing over it drops that text *and* shifts the number of every line after
            // it, so what it holds is walked like anything else the body is made of.
            DocumentChild::StructuredDataTag(tag) => tag_of(tag, &mut out),
            // A section break, a bookmark: things a document is made of that hold no text of
            // their own. Passed over rather than turned into blank lines, which would be
            // numbers standing for nothing.
            _ => {}
        }
    }
    Ok(out)
}

/// A content control's contents, as the lines they are in the body.
fn tag_of(tag: &docx_rs::StructuredDataTag, out: &mut String) {
    use docx_rs::StructuredDataTagChild as Child;

    for child in &tag.children {
        match child {
            Child::Paragraph(paragraph) => {
                out.push_str(&text_of(paragraph));
                out.push('\n');
            }
            Child::Table(table) => rows_of(table, out),
            Child::Run(run) => {
                push_run(run, out);
                out.push('\n');
            }
            // One control inside another, which Word writes for a nested field.
            Child::StructuredDataTag(tag) => tag_of(tag, out),
            _ => {}
        }
    }
}

/// One paragraph, as it reads.
fn text_of(paragraph: &Paragraph) -> String {
    let mut out = String::new();
    for child in &paragraph.children {
        match child {
            ParagraphChild::Run(run) => push_run(run, &mut out),
            // The document as it stands includes what was inserted into it.
            ParagraphChild::Insert(insert) => push_insert(insert, &mut out),
            // And excludes what was deleted from it, which is `ParagraphChild::Delete` and is
            // therefore not in this match at all.
            ParagraphChild::Hyperlink(link) => {
                for child in &link.children {
                    match child {
                        ParagraphChild::Run(run) => push_run(run, &mut out),
                        ParagraphChild::Insert(insert) => push_insert(insert, &mut out),
                        _ => {}
                    }
                }
            }
            // A tracked *move*: `w:moveTo` is where the text stands now, so it is in the
            // document for the reason an insertion is. `MoveFrom` is where it used to be and
            // stays out, like `Delete`.
            ParagraphChild::MoveTo(moved) => {
                for child in &moved.children {
                    if let docx_rs::MoveToChild::Run(run) = child {
                        push_run(run, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A table, one line per row.
fn rows_of(table: &Table, out: &mut String) {
    for TableChild::TableRow(row) in &table.rows {
        let cells: Vec<String> = row
            .cells
            .iter()
            .map(|TableRowChild::TableCell(cell)| {
                // A cell holds paragraphs, and a cell that is several of them is still one
                // cell: joined with a space rather than broken across lines, because a row
                // whose cells landed on different lines is not a row any more.
                let cell = cell
                    .children
                    .iter()
                    .filter_map(|child| match child {
                        TableCellContent::Paragraph(paragraph) => Some(text_of(paragraph)),
                        // A nested table is text in this cell too. Flattened into it rather
                        // than dropped: its rows on lines of their own would leave the cells
                        // of the row above no longer lining up.
                        TableCellContent::Table(table) => {
                            let mut rows = String::new();
                            rows_of(table, &mut rows);
                            // Its text, not its row spelling: the `| |` of the inner table
                            // inside a cell of the outer one reads as a column that is not
                            // there.
                            Some(
                                rows.lines()
                                    .map(|row| row.trim_matches('|').trim())
                                    .collect::<Vec<_>>()
                                    .join(" "),
                            )
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                // And a break inside one is a space for the same reason. `push_run` makes a
                // line of it, which is what it is in the body and what would leave half a row
                // on the line below here.
                cell.replace('\n', " ")
            })
            .collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
}

/// The runs inside an insertion.
fn push_insert(insert: &Insert, out: &mut String) {
    for child in &insert.children {
        if let InsertChild::Run(run) = child {
            push_run(run, out);
        }
    }
}

/// One run's text.
///
/// `DeleteText` is deliberately absent: a run inside `w:del` carries its text in that variant,
/// so leaving it out here is the second half of the same rule the paragraph makes.
fn push_run(run: &Run, out: &mut String) {
    for child in &run.children {
        match child {
            RunChild::Text(text) => out.push_str(&text.text),
            RunChild::Tab(_) | RunChild::PTab(_) => out.push('\t'),
            // Shift+Enter, and `<w:cr/>` which is the same break written another way. Without
            // this the two sides of it are one word — `firstsecond` — which is worse than a
            // wrong line count: it is a sentence the document does not contain.
            //
            // Which kind of break it is does not come up. `docx-rs` keeps the type behind a
            // private field, and there is nothing to decide anyway: a page break and a column
            // break also end the line they are in.
            RunChild::Break(_) | RunChild::CarriageReturn(_) => out.push('\n'),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use docx_rs::{Delete, Docx, Paragraph, Run, Table, TableCell, TableRow};

    use super::*;

    /// A document written by the same crate that reads it, so that no `.docx` is checked in
    /// and every fixture is one sentence of Rust rather than a binary nobody can review.
    fn document(dir: &tempfile::TempDir, name: &str, docx: Docx) -> std::path::PathBuf {
        let path = dir.path().join(name);
        docx.build()
            .pack(std::fs::File::create(&path).expect("a file"))
            .expect("a document");
        path
    }

    fn paragraph(text: &str) -> Paragraph {
        Paragraph::new().add_run(Run::new().add_text(text))
    }

    /// The lines are the paragraphs, in order, numbered from one.
    #[test]
    fn a_paragraph_is_a_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = document(
            &dir,
            "a.docx",
            Docx::new()
                .add_paragraph(paragraph("The first paragraph"))
                .add_paragraph(paragraph("The second")),
        );

        let window = read(&path, 1, 10)
            .expect("a Word document")
            .expect("it reads");
        assert_eq!(
            window.shown,
            "     1\tThe first paragraph\n     2\tThe second\n"
        );
    }

    /// The document as it stands: what was inserted is in it, what was deleted is not.
    ///
    /// The half that is easy to get wrong is the insertion — a reader that walks only plain
    /// runs drops the newest sentence in a document under revision and answers as if it were
    /// complete.
    #[test]
    fn insertions_are_in_the_document_and_deletions_are_not() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = document(
            &dir,
            "a.docx",
            Docx::new()
                .add_paragraph(paragraph("Kept"))
                .add_paragraph(
                    Paragraph::new().add_insert(Insert::new(Run::new().add_text("Inserted"))),
                )
                .add_paragraph(
                    Paragraph::new()
                        .add_delete(Delete::new().add_run(Run::new().add_delete_text("Deleted"))),
                ),
        );

        let window = read(&path, 1, 10)
            .expect("a Word document")
            .expect("it reads");
        assert!(window.shown.contains("Kept"), "{}", window.shown);
        assert!(window.shown.contains("Inserted"), "{}", window.shown);
        assert!(
            !window.shown.contains("Deleted"),
            "a sentence the author removed is not in the document: {}",
            window.shown
        );
    }

    /// A break the author typed is a line, both ways of spelling it.
    ///
    /// Shift+Enter is a `<w:br/>` and the file remembers it, so it is as much a line as the
    /// paragraph mark is. Left out of [`push_run`] it is worse than a line miscounted: the
    /// words on either side of it run together into one the document does not contain.
    #[test]
    fn a_break_the_author_made_is_a_line() {
        use docx_rs::BreakType;

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = document(
            &dir,
            "a.docx",
            Docx::new()
                .add_paragraph(
                    Paragraph::new().add_run(
                        Run::new()
                            .add_text("first")
                            .add_break(BreakType::TextWrapping)
                            .add_text("second"),
                    ),
                )
                // `<w:cr/>`: the same break, and the same answer.
                .add_paragraph(
                    Paragraph::new().add_run(
                        Run::new()
                            .add_text("third")
                            .add_carriage_return()
                            .add_text("fourth"),
                    ),
                ),
        );

        let window = read(&path, 1, 10)
            .expect("a Word document")
            .expect("it reads");
        assert_eq!(
            window.shown,
            "     1\tfirst\n     2\tsecond\n     3\tthird\n     4\tfourth\n"
        );
        assert_eq!(window.total, 4, "two paragraphs, and a break in each");
    }

    /// A row is a line, and its cells stay told apart.
    #[test]
    fn a_table_row_is_a_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = document(
            &dir,
            "a.docx",
            Docx::new().add_table(Table::new(vec![TableRow::new(vec![
                TableCell::new().add_paragraph(paragraph("left")),
                TableCell::new().add_paragraph(paragraph("right")),
            ])])),
        );

        let window = read(&path, 1, 10)
            .expect("a Word document")
            .expect("it reads");
        assert_eq!(window.shown, "     1\t| left | right |\n");
    }

    /// Except in a cell, where a break is a space: half a row on the line below is not a row,
    /// and the cells would stop lining up from there down.
    #[test]
    fn a_break_inside_a_cell_does_not_break_the_row() {
        use docx_rs::BreakType;

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = document(
            &dir,
            "a.docx",
            Docx::new().add_table(Table::new(vec![TableRow::new(vec![
                TableCell::new().add_paragraph(
                    Paragraph::new().add_run(
                        Run::new()
                            .add_text("over")
                            .add_break(BreakType::TextWrapping)
                            .add_text("two lines"),
                    ),
                ),
                TableCell::new().add_paragraph(paragraph("right")),
            ])])),
        );

        let window = read(&path, 1, 10)
            .expect("a Word document")
            .expect("it reads");
        assert_eq!(window.shown, "     1\t| over two lines | right |\n");
    }

    /// A window into a document is the window it would be into a file: the same numbers, the
    /// same count, and the same everything after it.
    #[test]
    fn a_document_is_windowed_like_anything_else() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let mut docx = Docx::new();
        for n in 1..=10 {
            docx = docx.add_paragraph(paragraph(&format!("paragraph {n}")));
        }
        let path = document(&dir, "a.docx", docx);

        let window = read(&path, 3, 2)
            .expect("a Word document")
            .expect("it reads");
        assert_eq!(window.shown, "     3\tparagraph 3\n     4\tparagraph 4\n");
        assert_eq!(window.total, 10);
    }

    /// A text file that says `word/document.xml` in it is a text file. This one does, which
    /// is how the check that is only a string search was found to be one.
    #[test]
    fn a_file_that_merely_mentions_the_body_is_not_a_document() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"a docx keeps its body at word/document.xml\n").expect("a file");

        assert!(read(&path, 1, 10).is_none());
    }

    /// A zip that is not a Word document is not this module's to answer about, so that the
    /// caller goes on to name it for what it is. A real archive, with a directory that reads
    /// and no body part in it — which is the shape an `.xlsx` arrives in.
    #[test]
    fn a_zip_that_is_not_a_word_document_is_left_alone() {
        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.xlsx");
        let mut zip = ZipWriter::new(std::fs::File::create(&path).expect("a file"));
        zip.start_file(
            "xl/workbook.xml",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .expect("an entry");
        zip.write_all(b"<workbook/>").expect("the entry");
        zip.finish().expect("an archive");

        assert!(read(&path, 1, 10).is_none());
    }

    /// And so is a file that only begins like one: an archive whose directory cannot be read
    /// is an archive nothing can say the contents of.
    #[test]
    fn a_file_that_only_starts_like_a_zip_is_left_alone() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.zip");
        std::fs::write(&path, b"PK\x03\x04not a word document at all").expect("a zip");

        assert!(read(&path, 1, 10).is_none());
    }

    /// Deciding costs a peek, not the file. This is the property the whole of [`is_document`]
    /// is shaped by: it runs on every file the program is handed, ahead of the reader whose
    /// job is to hold a window and walk the rest, so a decision that read the bytes would be
    /// that bound undone by the module that runs first.
    #[test]
    fn deciding_what_a_file_is_reads_a_peek_of_it() {
        /// A reader that says how much of itself was read.
        struct Counted<'a> {
            bytes: Cursor<Vec<u8>>,
            read: &'a std::cell::Cell<usize>,
        }

        impl std::io::Read for Counted<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let read = self.bytes.read(buf)?;
                self.read.set(self.read.get() + read);
                Ok(read)
            }
        }

        impl std::io::Seek for Counted<'_> {
            fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
                self.bytes.seek(to)
            }
        }

        let read = std::cell::Cell::new(0);
        let large = vec![b'x'; 8 << 20];
        assert!(!is_document(Counted {
            bytes: Cursor::new(large.clone()),
            read: &read,
        }));
        assert!(
            read.get() <= ZIP.len(),
            "a file that is not a zip is answered out of its first bytes, and {} were read",
            read.get()
        );

        // A file that begins like a zip and is not one: the head, the tail, and nothing in
        // between. This is the case [`TAIL`] is there for — left to the zip reader, it is a
        // search backwards over the whole file for a record that is not in it.
        let mut bytes = ZIP.to_vec();
        bytes.extend_from_slice(&large);
        read.set(0);
        assert!(!is_document(Counted {
            bytes: Cursor::new(bytes),
            read: &read,
        }));
        assert!(
            read.get() <= ZIP.len() + TAIL as usize,
            "answering about a zip with no directory read {} of {}",
            read.get(),
            large.len()
        );
    }
}
