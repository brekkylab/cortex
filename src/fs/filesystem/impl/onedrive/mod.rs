//! OneDrive over Microsoft Graph, as a read-only backend. [`OnedriveFs`] documents the
//! tree it serves.

mod accessor;
#[allow(clippy::module_inception)]
mod onedrive;

pub use accessor::{OnedriveConfig, OnedriveOrigins};
pub use onedrive::OnedriveFs;
