//! Google Drive as a read-only backend. [`GdriveFs`] documents the tree it serves.

mod accessor;
mod gdrive;

pub use accessor::GdriveConfig;
pub use gdrive::GdriveFs;
// Public because `GdriveConfig::origins` is; `GdriveAccessor` stays private, so outside
// code holds a mount, not a bare client.
pub use accessor::GdriveOrigins;
