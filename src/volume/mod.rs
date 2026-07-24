mod mem;
mod passthrough;

use crate::error::Result;
use std::path::Path;

pub use mem::InMemVolume;
pub use passthrough::PassthroughVolume;

pub enum Dirent {
    Dir(String),
    File(String),
}

impl Dirent {
    /// The entry's name, regardless of whether it is a directory or a file.
    pub fn name(&self) -> &str {
        match self {
            Dirent::Dir(name) | Dirent::File(name) => name,
        }
    }
}

pub trait Mountable {
    fn list(&self, path: &Path) -> Result<Vec<Dirent>>;

    fn mkdir(&self, path: &Path) -> Result<()>;

    fn unlink(&self, path: &Path) -> Result<()>;

    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    fn write(&self, path: &Path, data: &[u8]) -> Result<()>;
}
