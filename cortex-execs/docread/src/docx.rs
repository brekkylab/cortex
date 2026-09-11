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

/// The most this will unpack to read one document, every part of it together.
///
/// A `.docx` is a compressed archive, so what it weighs on disk says nothing about what
/// reading it costs: a body part that is nothing but repeated `<w:p>` deflates to a
/// three-hundredth of itself, and a file too small on disk to be worth noticing unpacks into
/// more memory than the machine has. That is the one way a caller here can still name this
/// program's memory — [`text::read`] holds a window and walks the rest, and a document holds
/// all of itself — so it is bounded by a number rather than by what the file happens to say.
///
/// The whole archive and not the body alone, because [`read_docx`] reads every part there is
/// into memory beside it: styles, numbering, settings, comments, the headers and footers, and
/// the images in `word/media`. A bound over one entry is a bound the same bomb is moved out
/// of, into a part that is read just as eagerly and named in no check.
const MAX_UNPACKED_BYTES: u64 = 128 << 20;

/// The most any one of its XML parts may weigh.
///
/// Lower than [`MAX_UNPACKED_BYTES`] because what a part weighs is not what it costs to read:
/// [`read_docx`] turns a part into a tree of nodes, and a part made of nothing but `<w:p>` is the worst of that
/// trade — 219 MB of such a body was measured at 5.15 GB resident against a 12.32 GB peak,
/// which is fifty-odd bytes held for every byte unpacked. A bound of 128 MiB spent on one
/// part would be a bound on nothing.
///
/// An image is the other extreme and is kept as the bytes it is, so what holds it is the
/// total above and nothing tighter — which is the reason these are two numbers and not one,
/// and the reason this one is asked of the `.xml` and `.rels` entries rather than of every
/// entry there is. Sixteen mebibytes of body XML is a document of some hundreds of pages:
/// what this bounds is the shape a bomb has, not the size a document has.
const MAX_PART_BYTES: u64 = 16 << 20;

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
/// module that runs first. [`verdict`] is therefore bounded on purpose for every file that
/// is not a document, and the whole read happens on the other side of it.
///
/// And a document's own size is not the file's, which is why the size is measured before any
/// of it is parsed: see [`MAX_UNPACKED_BYTES`] and [`fits`]. A file that unpacks past what
/// this will hold is refused here rather than met halfway through with a failure to allocate,
/// which on the guest is the program being killed rather than answering.
pub fn read(path: &Path, offset: usize, limit: usize) -> Option<std::io::Result<Window>> {
    // A file that will not open at all is not this module's to refuse: whatever is wrong with
    // it — a directory, a permission — the read that follows meets the same thing and says it
    // about the file rather than about a look inside a zip.
    let Ok(mut file) = File::open(path) else {
        return None;
    };
    match verdict(BufReader::new(&file)) {
        Verdict::No => return None,
        // A refusal and not a `None`: the body part is in there, so what the caller is holding
        // is a Word document, and hearing that it is too large to read is worth more than
        // hearing from the text reader that it is a zip. Which bound it is past is said,
        // because the two are different facts about the file — one part that is enormous, or
        // a great many that are not.
        Verdict::TooLarge => {
            return Some(Err(too_large(&format!(
                "it unpacks past the {} MiB this reads at once",
                MAX_UNPACKED_BYTES >> 20
            ))));
        }
        Verdict::PartTooLarge => {
            return Some(Err(too_large(&format!(
                "one part of it unpacks past the {} MiB this reads at once",
                MAX_PART_BYTES >> 20
            ))));
        }
        Verdict::Yes => {}
    }
    // The archive is held whole as well — [`read_docx`] parses out of memory — and what it
    // weighs on disk is not what its entries weigh: a zip is read from its end, so a
    // directory listing a kilobyte can hang off a gigabyte of anything at all. Bounded by the
    // same number, because it is the same memory.
    if file
        .metadata()
        .is_ok_and(|about| about.len() > MAX_UNPACKED_BYTES)
    {
        return Some(Err(too_large(&format!(
            "it is more than the {} MiB of file this reads at once",
            MAX_UNPACKED_BYTES >> 20
        ))));
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

/// What a file turned out to be.
enum Verdict {
    /// Not a Word document: not a zip, or not an archive whose directory reads, or an archive
    /// with no body part in it.
    No,
    /// A Word document, and one that unpacks into what this will hold.
    Yes,
    /// A Word document whose parts unpack past [`MAX_UNPACKED_BYTES`] together.
    TooLarge,
    /// A Word document with one XML part past [`MAX_PART_BYTES`].
    PartTooLarge,
}

/// Which of those `reader` is, out of a peek, an index, and — only for an archive that holds a
/// body part — the unpacking of it.
///
/// The first two questions are not the file's length: the four bytes that say it is a zip, and
/// then the archive's directory, which lists what is in it without any of it being unpacked.
/// What that costs is a read of the head and a read of the tail, and it is what nearly every
/// file this program is handed costs, because nearly every one of them is answered `No` here.
///
/// A zip that will not open is a `No` and not a document that failed. It is the honest answer
/// — an archive whose directory is unreadable is one nothing can say the contents of — and it
/// leaves the file to be named as the zip it is, which is what [`text::read`] does with it.
/// The refusal about a document that will not parse is still there for the case it is about: a
/// readable archive holding a body part that the parser then chokes on.
fn verdict(mut reader: impl std::io::Read + std::io::Seek) -> Verdict {
    let mut head = [0; ZIP.len()];
    // A file shorter than the magic is not a zip, which is what the failure means here.
    if reader.read_exact(&mut head).is_err() || head != *ZIP {
        return Verdict::No;
    }
    if !ends_with_a_directory(&mut reader) {
        return Verdict::No;
    }
    // Wherever the reads above left the cursor: an archive is read from its end, and
    // `ZipArchive` seeks there itself.
    let Ok(mut archive) = zip::ZipArchive::new(reader) else {
        return Verdict::No;
    };
    if archive.index_for_name(BODY).is_none() {
        return Verdict::No;
    }
    fits(&mut archive, MAX_UNPACKED_BYTES, MAX_PART_BYTES)
}

/// [`Verdict::Yes`] if the archive unpacks into `total` with no one XML part of it past
/// `part`, and which of the two it is past otherwise.
///
/// The two bounds are arguments and not the constants read straight off, because a test of
/// which one stopped an archive has to build an archive that goes past them, and a fixture of
/// [`MAX_UNPACKED_BYTES`] is a fixture no test should be making. The caller passes the real
/// ones — there is one caller.
///
/// Unpacked here and not read out of the directory, which is the part worth arguing. Every
/// entry's uncompressed size is written down in the central directory and this could be four
/// lookups instead of a decompression — but that number is one the file's author wrote, and
/// nothing on the way in enforces it: the deflate decoder is handed no bound, and [`read_docx`]
/// reads each part with `read_to_end` into a `Vec` whose *capacity* is the declared size. A
/// bomb that claims a kilobyte is therefore a bomb that passes a check on the claim and blows
/// up on the read. Measuring costs one decompression that is thrown away; believing costs the
/// bound being a bound against honest files only.
///
/// It is measured into [`std::io::sink`] and stopped the moment the total goes past, so this
/// holds a buffer and not a part however large the archive says it is — the property the whole
/// module is shaped by, kept by the one thing here that touches the compressed bytes.
///
/// An entry that will not unpack at all is passed over rather than answered about. Whatever is
/// wrong with it — a compression method that is not in this build, a truncated stream — the
/// parser meets the same thing on the other side of this and refuses about a Word document,
/// which is a better sentence than anything this function knows how to say.
fn fits<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    total: u64,
    part: u64,
) -> Verdict {
    let mut left = total;
    for at in 0..archive.len() {
        // The XML bound is the tighter of the two and applies to the parts [`read_docx`] turns
        // into nodes; `word/media` and anything else in there is bytes, and is held by what is
        // left of the total alone.
        let xml = archive
            .name_for_index(at)
            .is_some_and(|name| name.ends_with(".xml") || name.ends_with(".rels"));
        let cap = if xml { left.min(part) } else { left };
        let Ok(entry) = archive.by_index(at) else {
            continue;
        };
        // One byte past what is allowed, so that reading it is the answer: an entry that fills
        // `cap` exactly is an entry that fits.
        let Ok(unpacked) = std::io::copy(&mut entry.take(cap + 1), &mut std::io::sink()) else {
            continue;
        };
        // Which bound stopped it, and not merely which entry: an XML part that is under `part`
        // and past what the entries before it left of the total is the total's doing, and
        // saying otherwise would send a caller looking for a part that is not there.
        if unpacked > cap {
            return if xml && unpacked > part {
                Verdict::PartTooLarge
            } else {
                Verdict::TooLarge
            };
        }
        left -= unpacked;
    }
    Verdict::Yes
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

/// A refusal about a document that is too large to read, in the words every other refusal
/// here is written in: what the file is, and then what is the matter with it.
fn too_large(said: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("this is a Word document and {said}"),
    )
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

    /// Deciding costs a peek, not the file. This is the property the whole of [`verdict`]
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
        assert!(matches!(
            verdict(Counted {
                bytes: Cursor::new(large.clone()),
                read: &read,
            }),
            Verdict::No
        ));
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
        assert!(matches!(
            verdict(Counted {
                bytes: Cursor::new(bytes),
                read: &read,
            }),
            Verdict::No
        ));
        assert!(
            read.get() <= ZIP.len() + TAIL as usize,
            "answering about a zip with no directory read {} of {}",
            read.get(),
            large.len()
        );
    }
    /// A body part that unpacks past the bound is a refusal about a Word document, not a
    /// `None` that would have the text reader call it a zip — and not a read that finds out by
    /// running out of memory. Deflated, because that is the shape of the thing: this fixture is
    /// a third of a megabyte on disk and is refused for what it weighs unpacked.
    #[test]
    fn a_document_that_unpacks_past_the_bound_is_refused() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("bomb.docx");
        std::fs::write(&path, archive(&[(BODY, MAX_PART_BYTES + 1)], None)).expect("a file");

        let refusal = read(&path, 1, 10)
            .expect("a Word document")
            .expect_err("a refusal");
        assert_eq!(refusal.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            refusal.to_string().contains("one part of it unpacks past"),
            "refused for the wrong reason: {refusal}"
        );
    }

    /// And it is refused when the archive says otherwise. The size in a zip's directory is a
    /// number the file's author wrote, and nothing between there and `read_to_end` enforces
    /// it, so believing it would be a bound that holds against honest files alone. This one
    /// claims a hundred bytes.
    #[test]
    fn a_document_that_lies_about_its_size_is_refused_too() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("liar.docx");
        std::fs::write(&path, archive(&[(BODY, MAX_PART_BYTES + 1)], Some(100))).expect("a file");

        let refusal = read(&path, 1, 10)
            .expect("a Word document")
            .expect_err("a refusal");
        assert!(
            refusal.to_string().contains("unpacks past"),
            "refused for the wrong reason: {refusal}"
        );
    }

    /// The total is a bound of its own and not the largest part's: entries that are each small
    /// enough and together are not are past it, and an entry that is not XML is held by it
    /// alone. Both are asked of [`fits`] at bounds this can make files of — the constants are
    /// megabytes, and a test that built a fixture of them would be paying for a number rather
    /// than for what the walk does with it.
    #[test]
    fn the_bound_on_the_whole_archive_is_not_the_bound_on_one_part() {
        let bytes = archive(&[(BODY, 8 << 10), ("word/media/a.png", 40 << 10)], None);

        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.clone())).expect("an archive");
        // The image is past what is left of the total, and is not a part: the total is what
        // stopped it, and the total is what is said.
        assert!(matches!(
            fits(&mut zip, 32 << 10, 16 << 10),
            Verdict::TooLarge
        ));

        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.clone())).expect("an archive");
        // Room enough in the total for both, and the body alone past the part bound.
        assert!(matches!(
            fits(&mut zip, 1 << 20, 4 << 10),
            Verdict::PartTooLarge
        ));

        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("an archive");
        assert!(matches!(fits(&mut zip, 1 << 20, 16 << 10), Verdict::Yes));
    }

    /// An archive of `entries`, each deflated and as many bytes unpacked as it says — and, if
    /// `claims` is given, with the first entry's size overwritten in the central directory,
    /// which is where a zip reader looks it up and where a file that means harm would put the
    /// lie.
    fn archive(entries: &[(&str, u64)], claims: Option<u32>) -> Vec<u8> {
        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, unpacked) in entries {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .expect("an entry");
            // `<w:p/>` over and over: a part that is all elements and no text, which is the
            // document that costs the most memory per byte unpacked.
            let chunk = "<w:p/>".repeat(1 << 10);
            let mut written = 0;
            while written < *unpacked {
                zip.write_all(chunk.as_bytes()).expect("the entry");
                written += chunk.len() as u64;
            }
        }
        let mut bytes = zip.finish().expect("an archive").into_inner();

        if let Some(claims) = claims {
            // The uncompressed size is the sixth field of a central directory file header, at a
            // fixed offset from the signature that begins it.
            const HEADER: &[u8] = b"PK\x01\x02";
            const SIZE: usize = 24;
            let at = bytes
                .windows(HEADER.len())
                .position(|bytes| bytes == HEADER)
                .expect("a central directory");
            bytes[at + SIZE..at + SIZE + 4].copy_from_slice(&claims.to_le_bytes());
        }
        bytes
    }
}
