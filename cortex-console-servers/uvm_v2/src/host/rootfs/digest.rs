use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Digest(pub(crate) String);

impl Digest {
    pub fn of(bytes: &[u8]) -> Digest {
        Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    pub fn of_file(path: &Path) -> anyhow::Result<Digest> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(Digest(format!("sha256:{:x}", hasher.finalize())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_stem(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
