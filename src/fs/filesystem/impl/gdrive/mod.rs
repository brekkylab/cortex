//! Google Drive as a read-only backend. [`GdriveFs`] documents the tree it serves.

mod accessor;
mod gdrive;

pub use accessor::GdriveConfig;
pub use gdrive::GdriveFs;
// `GdriveConfig::origins` is public, so whoever builds one has to be able to name its
// type. The accessor beside it is not: nothing outside this module has business
// holding a Drive client that is not a mount.
pub use accessor::GdriveOrigins;
