//! Google Drive as a read-only backend.
//!
//! ```text
//! My Drive/                 the folder tree, from Drive's own `root`
//!   q3.pdf                  a blob, served as its own bytes
//!   plan.docx              a Docs-editors document, served as its Office export
//! Shared with me/           what this account was given
//! <shared drive>/           one directory per shared drive this account can see
//! ```
//!
//! The root mirrors Drive's sidebar rather than its folder tree, because the folder tree
//! alone cannot reach everything: a shared item carries no `parents`, so no walk from a
//! folder ever arrives at one, and a shared drive is a root of its own.
//!
//! A refresh token is what this backend takes, and getting one is not its job — the
//! consent round trip belongs to whatever set the mount up, the same way the Slack lane
//! takes a token it did not mint.
//!
//! **The Docs-editors types hold no bytes.** `files.get?alt=media` refuses them
//! outright — *"Only files with binary content can be downloaded. Use Export with Docs
//! Editors files."* — so each is served as its Office export, which is the form a reader
//! can open. The extension is the export's own, and it is also what says which of the
//! three the entry came from, since a Drive name carries none.
//!
//! ```text
//! <name>.docx    a Google Doc
//! <name>.xlsx    a Google Sheet
//! <name>.pptx    a Google Slides deck
//! ```
//!
//! Every *other* Google-native type — Forms, Drawings, Maps, Apps Script — is not listed
//! at all: none of them exports to anything, and a name that cannot be read is worse
//! than an absence.
//!
//! Two files of one name are numbered before the extension (`report (2).pdf`), because
//! Drive lets a folder hold both and a directory cannot.
//!
//! **A listing no longer says which of the two an entry was.** An uploaded `.pptx` and
//! an exported Google Slides deck read the same, and that is what serving the export
//! buys: a reader opens either without knowing. What separates them is what a read
//! costs — a real file reads by the window, an export does not — so the difference
//! survives where it matters and is invisible where it does not.
//!
//! ## Exporting through the link the listing hands over
//!
//! `files.export` is not the path taken. It caps at 10 MB and refuses past it with no
//! partial download to work around it, and of four real documents measured **three were
//! over the cap** — the capped endpoint is the exception, not the rule. Drive answers
//! instead with `exportLinks`, a per-MIME map on the file resource, which the listing
//! carries for free.
//!
//! That link answers `307` to a `googleusercontent.com` host whose query holds its own
//! signed grant, so the two legs are issued separately: the token rides the first and
//! nothing rides the second, which is measured to need none. The redirect's host is
//! checked, the answer's `Content-Type` is checked (every failure this endpoint produces
//! is an HTML page — `401` unreadable, `404` unknown), and the redirect URL is never
//! logged, being a credential for that object.
//!
//! ## What a read costs
//!
//! Every one of them is network:
//!
//! * A real file reads by the window, so `head` moves what it asks for and `cat` of a
//!   70 MB file moves 70 MB.
//! * **A document has no windows.** An export honours no range (`bytes=0-0` answers
//!   `200` with the whole object; `HEAD` answers a `content-length` of `0`), so any read
//!   produces all of it, and it is held for the listing TTL rather than produced again
//!   per chunk. Drive's own `size` is what bounds this: it estimates the export within
//!   0.3%, so an oversized document is refused before a byte moves.
//! * A listing is held for five minutes, so a change just made in Drive may not show yet.
//!
//! ## What an export loses
//!
//! **A spreadsheet can come back wrong, and nothing here can repair it.** Google exports
//! an in-cell image as a picture floating over the sheet rather than as the cell's value,
//! so a `VLOOKUP` that returned that image in Sheets returns `#N/A` in Excel; named
//! ranges can arrive as `#REF!` in the same file. Measured on a real workbook and
//! reproduced by downloading it from Drive's own UI, so it is Google's export rather than
//! this path — and the cell has no cached value to fall back on. Documents and decks
//! carry neither formulas nor in-cell images, and do not have the problem.
//!
//! And an Office file is a zip, so `grep` finds nothing inside one. What a reader wants
//! from a document here is to open it, which is the trade this makes.

mod accessor;
mod gdrive;
mod origins;

pub use accessor::GdriveConfig;
pub use gdrive::GdriveFs;
// `GdriveConfig::origins` is public, so whoever builds one has to be able to name its
// type. The accessor beside it is not: nothing outside this module has business
// holding a Drive client that is not a mount.
pub use origins::Origins;
