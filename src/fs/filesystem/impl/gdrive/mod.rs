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
//! alone cannot reach everything: a shared item carries no `parents`, and a shared drive is a
//! root of its own.
//!
//! Takes a refresh token and does not mint one; the consent round trip belongs to whatever
//! set the mount up.
//!
//! **Docs-editors types are served as their own API's JSON.** They hold no bytes
//! (`files.get?alt=media` answers 403), and the JSON is the only form that carries formulas,
//! slide geometry and the character indices an edit addresses. The suffix names the type,
//! since a Drive name has no extension:
//!
//! ```text
//! <name>.gdoc.json     paragraphs, styles, tables, and the character indices an edit
//!                      addresses
//! <name>.gsheet.json   tabs, named ranges, charts, and each tab's cell values under
//!                      `sheets[].values` (`sheets[].valuesOmitted` past the budget)
//! <name>.gslide.json   pages, shapes, transforms, speaker notes
//! ```
//!
//! Their text is split across style runs, so `grep` finds words rather than phrases and
//! `-A`/`-B` shows JSON siblings. Other Google-native types (Forms, Drawings, Maps, Apps
//! Script) are not listed: none can be read, and an unreadable name is worse than none.
//!
//! **Every entry carries a tag off its own Drive id** (`report_a1b2c3d4.pdf`), because one
//! Drive folder can hold two files of a name and a directory cannot. Tagging every entry, not
//! only colliding ones, makes a name depend on its own file alone. The root's sections are
//! not tagged: this store invents them.
//!
//! **The extension tells a blob from a document.** An uploaded `.pptx` keeps its name; a
//! Slides deck is `<name>.gslide.json`.
//!
//! ## Why not the Office export
//!
//! An export has no usable length. `files.export` refuses anything over its size cap, and
//! `exportLinks` declares no length and ignores ranges, so the length exists only after a
//! render that takes seconds. An OOXML reader seeks to the end from the length `stat` gave,
//! and Drive's listed size is too far off to stand in; rendering at `stat` instead would
//! render a whole folder for one `ls -l`. The JSON is read front to back, needs no render,
//! and is much smaller than the export.
//!
//! ## What a read costs
//!
//! Every read is network:
//!
//! * A blob reads by spans rather than kernel windows, because a request costs a round trip
//!   whatever its size: up to `FIRST_SPAN` for a first read and `READ_SPAN` for one carrying
//!   on from the last, both divided among the files being read at once.
//! * A document has no windows: any read produces the whole JSON, and until one does its
//!   size is a **placeholder rather than a length**, so `ls -l` and `find -size` are wrong
//!   about an unread document. After a read the real length is reported at no cost. A
//!   spreadsheet costs two requests, the grid coming separately from the workbook.
//! * A listing is held for `DIR_TTL`, so a change just made in Drive may not show yet.

mod accessor;
mod gdrive;

pub use accessor::GdriveConfig;
pub use gdrive::GdriveFs;
// `GdriveConfig::origins` is public, so whoever builds one has to be able to name its
// type. The accessor beside it is not: nothing outside this module has business
// holding a Drive client that is not a mount.
pub use accessor::GdriveOrigins;
