//! A Word document as lines: one paragraph, one line.
//!
//! # Why a paragraph is a line
//!
//! A `.docx` has no lines — what it has is paragraphs, and a paragraph is a unit somebody
//! wrote rather than one a renderer arrived at. Where a line of text is broken at a byte and a
//! line of a PDF is broken wherever an engine guessed the page came apart, a `<w:p>` is
//! authored: it survives being opened in another word processor, at another paper size, at
//! another zoom.
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
//! spelling in which a row reads as a row and the cells stay told apart.
//!
//! Headers, footers, footnotes and comments are not read. Each is a part of its own beside the
//! body, and a caller reading line 12 of a document means the twelfth paragraph of what is
//! written on the page — not a running header that would push every number down by one.

use std::{io::Cursor, path::Path};

use docx_rs::{
    DocumentChild, Insert, InsertChild, Paragraph, ParagraphChild, Run, RunChild, Table,
    TableCellContent, TableChild, TableRowChild, read_docx,
};

use crate::text::{self, Window};

/// What the answer calls these lines.
pub const ORIGIN: &str = "paragraphs of this Word document, not lines in the file";

/// What every zip begins with.
///
/// Asked first, and not only because it is cheap: without it the search below is a search for
/// a string, and a *text* file that happens to contain `word/document.xml` — this file does —
/// would be handed to a zip reader that then says something baffling about it.
const ZIP: &[u8] = b"PK\x03\x04";

/// The entry every `.docx` has and the other zips do not.
///
/// Searched for in the file's own bytes, because a zip stores its entry names uncompressed:
/// the name is there to be found without unpacking anything. This is what tells a Word
/// document from the `.xlsx`, `.pptx` and `.jar` that are the same container — asked of the
/// bytes, like every other question here, and not of the extension.
const BODY: &[u8] = b"word/document.xml";

/// `limit` lines of `path`'s paragraphs from line `offset`, or `None` if this is not a Word
/// document at all.
///
/// `None` and not a refusal, so that the caller can go on to name what the file actually is.
/// Once the body part is known to be in there, every later failure is a refusal about a Word
/// document — which is the truth, and more use than being told it is a zip.
pub fn read(path: &Path, offset: usize, limit: usize) -> Option<std::io::Result<Window>> {
    // A file that will not open at all is not this module's to refuse: whatever is wrong with
    // it — a directory, a permission — the read that follows meets the same thing and says it
    // about the file rather than about a look inside a zip.
    let Ok(bytes) = std::fs::read(path) else {
        return None;
    };
    if !bytes.starts_with(ZIP) || !bytes.windows(BODY.len()).any(|window| window == BODY) {
        return None;
    }
    Some(lines(&bytes).and_then(|text| {
        // Through the same window as every other read, so a number, a cut line and the count
        // at the end mean what they mean everywhere else.
        text::window(Cursor::new(text.into_bytes()), offset, limit, None)
    }))
}

/// The document's text, a paragraph or a table row per line.
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
            // A section break, a bookmark, a structured tag: things a document is made of that
            // hold no text of their own. Passed over rather than turned into blank lines,
            // which would be numbers standing for nothing.
            _ => {}
        }
    }
    Ok(out)
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
                cell.children
                    .iter()
                    .filter_map(|child| match child {
                        TableCellContent::Paragraph(paragraph) => Some(text_of(paragraph)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
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
    /// caller goes on to name it for what it is.
    #[test]
    fn a_zip_that_is_not_a_word_document_is_left_alone() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.zip");
        std::fs::write(&path, b"PK\x03\x04not a word document at all").expect("a zip");

        assert!(read(&path, 1, 10).is_none());
    }
}
