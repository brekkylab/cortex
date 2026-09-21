//! A PDF as lines, when there is an engine linked in to make them.
//!
//! Only compiled with the `pdfium` feature, which is also what puts `libpdfium.a` in the
//! binary — see `build.rs`. Without it, a PDF is a file this program names and refuses, and
//! this module does not exist.
//!
//! # Whose lines these are
//!
//! A PDF has no lines. It has pages, and on each page a set of glyphs at coordinates; what
//! comes out of [`pdfium`](pdfium_render) is those glyphs put back into reading order by an
//! engine making its best guess at the layout. So the numbers this program prints in front of
//! extracted text are **the extraction's numbers and not the document's**, and two engines —
//! or two versions of this one — can disagree about them.
//!
//! Which is why the answer says so on its own line rather than looking exactly like a text
//! file's. The whole promise of this program is that a line number can be quoted back and mean
//! the same thing to whoever reads it next; here that promise is weaker, and hiding that would
//! be worse than not extracting at all.
//!
//! Page breaks are kept as `[page N]` lines for the same reason. A page is the one address a
//! PDF really has — it is what a person looking at the document can find — so it stays in the
//! text, even though it costs a line number of its own.
//!
//! # The whole document is extracted, then windowed
//!
//! `-o` and `-n` cut the text after the engine has laid it out, because there is nothing to
//! cut before: line 400 of a PDF is not knowable without laying out every page ahead of it.
//! A window is therefore cheap in what it prints and not in what it costs, which is the
//! opposite of the file case and worth knowing before pointing this at a thousand pages.

use std::{io::Cursor, path::Path};

use pdfium_render::prelude::{Pdfium, PdfiumError};

use crate::text::{self, Window};

/// The first bytes every PDF starts with.
pub const MAGIC: &[u8] = b"%PDF-";

/// What the answer calls these lines.
///
/// Named rather than left for a caller to infer, and named as the engine's: two versions of
/// pdfium can put the same page into a different order, so a number quoted out of this is
/// worth what the engine is worth and no more.
pub const ORIGIN: &str = "pdfium's reading of this PDF's pages, not lines in the file";

/// `limit` lines of `path`'s text from line `offset`, as the engine laid it out.
pub fn read(path: &Path, offset: usize, limit: usize) -> std::io::Result<Window> {
    // The bindings are the ones compiled into this binary: no library to find on the machine,
    // no `dlopen`, nothing on `LD_LIBRARY_PATH`. A failure here is not a missing file — it is
    // this binary having been linked without the archive it was supposed to carry.
    let pdfium = Pdfium::new(Pdfium::bind_to_statically_linked_library().map_err(engine)?);
    let document = pdfium.load_pdf_from_file(path, None).map_err(engine)?;

    let mut extracted = String::new();
    for (index, page) in document.pages().iter().enumerate() {
        extracted.push_str(&format!("[page {}]\n", index + 1));
        extracted.push_str(page.text().map_err(engine)?.all().trim_end());
        extracted.push('\n');
    }

    // Through the same window every other read goes through, so that a cut line, a number and
    // the count at the end mean what they mean everywhere else. `None` for the format: what
    // is being read at this point is text, and a refusal about its bytes would be about the
    // extraction rather than about the PDF.
    text::window(Cursor::new(extracted.into_bytes()), offset, limit, None)
}

/// What the engine said, as a refusal about this file.
///
/// [`ErrorKind::InvalidData`](std::io::ErrorKind::InvalidData) for all of it: by the time
/// pdfium is answering, the file was found and opened, and what is left is the document being
/// one this cannot read — encrypted, truncated, or not really a PDF behind its first bytes.
fn engine(e: PdfiumError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}"))
}
