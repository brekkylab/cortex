//! A OneDrive account as a read-only directory tree, over Microsoft Graph.
//!
//! ```text
//! onedrive/
//!   Documents/          the drive's own folders, from `/me/drive/root`
//!     report.docx       a file, served as its own bytes
//!   photo.jpg
//! ```
//!
//! The root is the drive root and nothing else — no virtual sections above it: a OneDrive
//! account *is* one drive, so one folder contains everything it can reach.
//!
//! ## What Graph gives
//!
//! | | OneDrive |
//! |---|---|
//! | paths | native, `/me/drive/root:/A/B` |
//! | Office files | real `.docx`/`.xlsx`/`.pptx` bytes |
//! | length | `size`, exact, on every item |
//! | ranges | on the download URL, 206 + `Content-Range` |
//!
//! So there is no document rendering here, no placeholder length, no whitespace padding of
//! a file's tail, and no remembered-length map — a listing states every size exactly, and
//! a short read is therefore the end of the file rather than the edge of a guess. What
//! remains is about the kernel rather than the service: reads by span, one listing cache,
//! and comparing names under NFC.
//!
//! ## Why there is no `Shared with me`
//!
//! `GET /me/drive/sharedWithMe` is deprecated: Microsoft's own reference says it "will
//! operate in a degraded state until November, 2026, after which it will stop returning
//! data", and it is already degraded — a mitigation cut it to a single returned item, as
//! several Microsoft Q&A threads report. No 1:1 replacement is documented.
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
//! walk takes a whole `READ_SPAN`, two concurrent walks 32 MiB each, and a read that stops
//! after the head takes at most `FIRST_SPAN`.
//!
//! ## What is duplicated
//!
//! `sanitize_name`, `same_name`, `vpath`, `split_last`, `slice` and the span policy are
//! copies of the Google Drive store's rather than a shared module, so a fix to one belongs
//! in both.

mod accessor;
#[allow(clippy::module_inception)]
mod onedrive;

pub use accessor::{OnedriveConfig, OnedriveOrigins};
pub use onedrive::OnedriveFs;
