//! A OneDrive account as a read-only directory tree, over Microsoft Graph.
//!
//! ```text
//! onedrive/
//!   Documents/          the drive's own folders, from `/me/drive/root`
//!     report.docx       a file, served as its own bytes
//!   photo.jpg
//! ```
//!
//! The root is the drive root, with no virtual sections above it: an account is one drive.
//!
//! Graph addresses items by path, serves Office files as their real bytes, states an exact
//! `size` on every item, and honours ranges on the download URL. So a short read is the end
//! of the file: nothing is rendered, and no length is guessed or padded.
//!
//! ## What a read costs
//!
//! A path resolves through its parent's listing, and everything `stat` reports comes off
//! that listing, so an `ls -l` costs one request rather than one per entry.
//!
//! Bytes come from a preauthenticated URL the listing already carried, fetched by spans
//! rather than kernel windows: up to `FIRST_SPAN` for a first read and `READ_SPAN` for a
//! walk, both divided among the files being read at once.

mod accessor;
#[allow(clippy::module_inception)]
mod onedrive;

pub use accessor::{OnedriveConfig, OnedriveOrigins};
pub use onedrive::OnedriveFs;
