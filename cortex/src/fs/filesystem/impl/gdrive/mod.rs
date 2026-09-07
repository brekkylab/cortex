//! Google Drive as a read-only backend.
//!
//! ```text
//! My Drive/                 the folder tree, from Drive's own `root`
//!   q3.pdf                  a blob, served as its own bytes
//!   plan.gdoc.json          a Docs-editors document, served as its API's JSON
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
//! **The Docs-editors types hold no bytes.** `files.get?alt=media` answers 403 for a Doc, a
//! Sheet or a Slides deck, because Drive can only export a rendering of one. Each is served
//! as its own API's JSON instead, which is the only form that carries formulas, slide
//! geometry, and the character indices an edit has to address. The suffix says which of the
//! three it was, since a Drive name has no extension.
//!
//! ```text
//! <name>.gdoc.json     paragraphs, styles, tables, and the character indices an edit
//!                      addresses
//! <name>.gsheet.json   tabs, named ranges, charts, and each tab's cell values under
//!                      `sheets[].values` (`sheets[].valuesOmitted` past the budget)
//! <name>.gslide.json   pages, shapes, transforms, speaker notes
//! ```
//!
//! Their text is split across style runs, so a `grep` here finds words rather than phrases,
//! and `-A`/`-B` shows JSON siblings rather than the document's next lines. Every *other*
//! Google-native type — Forms, Drawings, Maps, Apps Script — is not listed at all: none of
//! them answers an API or exports to anything, and a name that cannot be read is worse than
//! an absence.
//!
//! **Every entry carries a tag off its own Drive id**, in front of the extension
//! (`report_a1b2c3d4.pdf`), because Drive lets one folder hold two files of a name and a
//! directory cannot. On every entry rather than only on the ones that collide, so a name
//! depends on its own file and on nothing else in the folder: no arrival, departure, rename
//! or move touches anybody else's. The root's sections are not tagged — they are names this
//! store invents rather than names Drive gave.
//!
//! **A listing says which of the two an entry was, by its extension.** An uploaded
//! `.pptx` keeps its name; a Google Slides deck is served as `<name>.gslide.json`, the
//! deck as its own API describes it. That is deliberate — the alternative was an Office
//! export, which reads like a real file and cannot be read at all.
//!
//! ## Why not the Office export
//!
//! It was tried, and reverted. `files.export` caps at 10 MB and refuses past it, not even
//! in pieces — a 5 MiB range and a 1 MiB range both drew `403`, after 48 and 41 seconds
//! spent rendering the export they then refused. The `exportLinks` URL has no such cap,
//! but it declares no length either: no `Content-Length`, `HEAD` answers `0`, ranges are
//! ignored. So the only way to learn how long an export is, is to render it — measured
//! at 1.5-1.8 s for a document, 1.3-2.6 s for a spreadsheet, 8.8-39.6 s for a deck.
//!
//! And that length is what a zip reader needs. An OOXML file keeps its directory at the
//! *end*, so every reader of one — `unzip`, Word, Excel, Keynote, LibreOffice — seeks
//! there from the length `stat` gave. Drive's listed size missed by −4,910,139 to
//! +533,322 across six documents, against a backward-scan window bisected at 70,639
//! bytes, so an estimate cannot be made close enough. `stat` producing the export instead
//! put one `ls -l` of a 129-entry folder at 108 s, because FUSE-T serves over NFS and an
//! NFS client wants an attribute for every name it lists.
//!
//! The JSON has none of that shape. It is read front to back, so a wrong length costs
//! only the bytes past it rather than the whole file; its own API answers in about a
//! second whatever the document's size, because nothing is rendered; and it is 10-30x
//! smaller — a 401 MB workbook is 242,935 bytes of JSON, eight tabs and all, where as an
//! export this tree cannot touch it: `ls` shows 401 MB and any read fails outright,
//! because a document has no windows and the whole 382 MiB export would have to be held
//! to answer one.
//!
//! ## What a read costs
//!
//! Every one of them is network:
//!
//! * A real file reads by spans, not by the window the kernel asked for. The window is
//!   64 KiB, and a request costs a round trip of about 0.9 s whatever its size, so one
//!   request per window put a 641 MB archive at 0.04 MB/s. A first read takes 8 MiB and a
//!   read carrying on from the last takes 64 MiB, which is where the rate tops out at
//!   11-12 MB/s. `head` of that archive moves 8 MiB; `cat` of it moves all 641 MB in 53 s.
//! * A document has no windows. Any read of one produces the whole JSON, and until
//!   something does, its size is a **placeholder rather than a length** — `ls -l` and
//!   `find -size` are wrong about an unread document, and [`Stat`](crate::fs::Stat) has no
//!   field in which to say so. Once one has been read the real length is reported, which
//!   costs no request: the bytes were already produced. A spreadsheet costs two requests
//!   rather than one, the grid coming separately from the workbook.
//! * A listing is held for five minutes, so a change just made in Drive may not show yet.

mod accessor;
mod gdrive;

pub use accessor::GdriveConfig;
pub use gdrive::GdriveFs;
// `GdriveConfig::origins` is public, so whoever builds one has to be able to name its
// type. The accessor beside it is not: nothing outside this module has business
// holding a Drive client that is not a mount.
pub use accessor::GdriveOrigins;
