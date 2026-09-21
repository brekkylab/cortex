//! A OneDrive account as a read-only directory tree, over Microsoft Graph.
//!
//! ```text
//! onedrive/
//!   Documents/          the drive's own folders, from `/me/drive/root`
//!     report.docx       a file, served as its own bytes
//!   photo.jpg
//! ```
//!
//! The root is the drive root and nothing else — no virtual sections above it. That is
//! not a simplification for its own sake: Drive needs them because it has no folder that
//! contains everything an account can reach, while a OneDrive account *is* one drive.
//!
//! ## Why this is so much smaller than the Google Drive store
//!
//! Almost everything that one spends its size on exists because Drive cannot tell you how
//! long a Docs-editors file is without producing it. Graph has no such problem, and the
//! difference is structural:
//!
//! | | Google Drive | OneDrive |
//! |---|---|---|
//! | paths | none — one `files.list` per directory to walk | native, `/me/drive/root:/A/B` |
//! | Office files | no bytes at all; `alt=media` answers 403 | real `.docx`/`.xlsx`/`.pptx` |
//! | length | unknowable without rendering | `size`, exact, on every item |
//! | ranges | on the media endpoint | on the download URL, 206 + `Content-Range` |
//!
//! So there is no document rendering here, no placeholder length, no whitespace padding of
//! a file's tail, and no remembered-length map — a listing states every size exactly, and
//! a short read is therefore the end of the file rather than the edge of a guess.
//!
//! What does carry over from that store is the part that is about the kernel rather than
//! the service: reads by span, one listing cache, and comparing names under NFC.
//!
//! ## Why there is no `Shared with me`
//!
//! There is an obvious section to add and it should not be added. `GET
//! /me/drive/sharedWithMe` is deprecated: Microsoft's own reference says it "will operate
//! in a degraded state until November, 2026, after which it will stop returning data", and
//! it is already degraded — a mitigation cut it to a single returned item, which several
//! Microsoft Q&A threads report against. No 1:1 replacement is documented.
//!
//! <https://learn.microsoft.com/en-us/graph/api/drive-sharedwithme>
//!
//! A section built on it would be dead within months of being written, so the tree stops
//! at the drive.
//!
//! ## What a read costs
//!
//! A path resolves through its parent's listing, and everything `stat` reports comes off
//! that listing — so an `ls -l`, which the kernel turns into one `getattr` per name
//! because FUSE-T serves over NFS, costs one request rather than one per entry.
//!
//! Bytes come from a preauthenticated URL the listing already carried. A walk pays one
//! round trip per span rather than one per kernel window, which is 64 KiB or 32 through
//! FUSE-T. Both span sizes are ceilings divided among the files being read at once: a lone
//! walk takes a whole [`READ_SPAN`], two concurrent walks 32 MiB each, and a read that stops
//! after the head takes at most [`FIRST_SPAN`].
//!
//! ## A note on what is duplicated
//!
//! `sanitize_name`, `same_name`, `vpath`, `split_last`, `slice` and the span policy are
//! copies of the Google Drive store's, not shared with it: they were written while neither
//! store was merged, when a common module would have put two branches in each other's way.
//! Drive has landed, so this is the second and last of them, and extracting them is work
//! this store's own landing unblocks rather than work it should carry.

mod accessor;
#[allow(clippy::module_inception)]
mod onedrive;

pub use accessor::{OnedriveConfig, OnedriveOrigins};
pub use onedrive::OnedriveFs;
