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
//! Two files of one name are numbered before the extension (`report (2).pdf`), because Drive
//! lets a folder hold both and a directory cannot.
//!
//! **What a read costs.** Every one of them is network:
//!
//! * A real file reads by the window, so `head` moves what it asks for and `cat` of a 70 MB
//!   file moves 70 MB.
//! * A document has no windows. Any read of one produces the whole JSON, and until something
//!   does, its listed size is a **placeholder rather than a length** — `ls -l` and `find
//!   -size` are wrong about an unread document, and [`Stat`](crate::fs::Stat) has no field in
//!   which to say so. [`FileSystem::stat`](crate::fs::FileSystem::stat) turns the placeholder
//!   into the real length for a *blob* Drive listed without a size, by probing one ranged
//!   byte; a document's length is only ever learned by producing it.
//! * A listing is held for five minutes, so a change just made in Drive may not show yet.

mod accessor;
mod gdrive;
mod origins;

pub use accessor::GdriveConfig;
pub use gdrive::GdriveFs;
// `GdriveConfig::origins` is public, so whoever builds one has to be able to name its
// type. The accessor beside it is not: nothing outside this module has business
// holding a Drive client that is not a mount.
pub use origins::Origins;
