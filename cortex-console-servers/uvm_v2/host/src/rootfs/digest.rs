use std::fmt;
use std::path::Path;
use std::str::FromStr;

use microsandbox_image::Digest as Oci;
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

    /// The hash on its own, without the algorithm that produced it.
    pub fn hex(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(_, hex)| hex)
    }

    /// The same digest as the image crate spells it.
    ///
    /// Nothing is converted: the two types hold the same two strings, and this exists because
    /// every path in the cache is named by the crate's own `to_path_safe`, which is the one
    /// spelling a file under `layers/` or `vmdk/` can have.
    pub fn oci(&self) -> Oci {
        match self.0.split_once(':') {
            Some((algorithm, hex)) => Oci::new(algorithm, hex),
            None => Oci::new("sha256", &self.0),
        }
    }

    /// What names a file this digest is the name of: `sha256_<hex>`, the cache's spelling.
    pub fn path_safe(&self) -> String {
        self.oci().to_path_safe()
    }
}

impl From<&Oci> for Digest {
    fn from(digest: &Oci) -> Digest {
        Digest(digest.to_string())
    }
}

impl FromStr for Digest {
    type Err = anyhow::Error;

    /// `<algorithm>:<hash>`, and nothing looser: a digest with no algorithm in it names a
    /// file under a name some other digest could also be spelled with.
    fn from_str(text: &str) -> anyhow::Result<Digest> {
        match text.split_once(':') {
            Some((algorithm, hex)) if !algorithm.is_empty() && !hex.is_empty() => {
                Ok(Digest(text.to_string()))
            }
            _ => anyhow::bail!("{text:?} is not a digest, which is spelled <algorithm>:<hash>"),
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
