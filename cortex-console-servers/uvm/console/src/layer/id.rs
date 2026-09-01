//! What a layer is called: the digest of what it was made from.
//!
//! A digest and not a name, because the store is content-addressed. Two callers who build
//! the same layer out of the same input land on the same file without having to agree on
//! anything, and a layer that is already there is one nobody has to write again.
//!
//! What the digest is *of* is the caller's business: a tarball, a directory's contents, a
//! manifest. Only the caller knows what made the tree, so only the caller can say what
//! names it.

use std::fmt;

/// A layer's identity, spelled the way OCI spells a digest.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(String);

impl LayerId {
    /// The id of a layer made from `bytes`.
    pub fn of(bytes: &[u8]) -> LayerId {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        LayerId(format!("sha256:{:x}", hasher.finalize()))
    }

    /// Read one back, refusing anything that is not one.
    ///
    /// Checked rather than trusted: an id reaches this from a file name and from an image
    /// reference a client sent, and one that was not checked would become a path.
    ///
    /// Uppercase hex is refused rather than folded. Two spellings of one digest would be
    /// two file names for one layer, and the store's whole premise is that the same content
    /// lands in the same place.
    pub fn parse(spelling: &str) -> anyhow::Result<LayerId> {
        let hex = spelling
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("{spelling:?} does not begin with `sha256:`"))?;
        anyhow::ensure!(
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{spelling:?} is not 64 lowercase hex digits"
        );
        Ok(LayerId(spelling.to_string()))
    }

    /// The name this layer's files take, which is the digest without its algorithm.
    ///
    /// A `:` is legal in a path and confusing in one, and every layer in the store uses the
    /// same algorithm — so it is said in the spelling and left out of the file name.
    pub fn file_stem(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for LayerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_the_digest_of_what_it_names() {
        let a = LayerId::of(b"hello");
        let b = LayerId::of(b"hello");
        let c = LayerId::of(b"hello ");
        assert_eq!(a, b, "the same bytes name the same layer");
        assert_ne!(a, c, "different bytes do not");
        assert_eq!(
            a.to_string(),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn an_id_round_trips_through_its_spelling() {
        let id = LayerId::of(b"hello");
        assert_eq!(LayerId::parse(&id.to_string()).unwrap(), id);
        assert_eq!(id.file_stem(), &id.to_string()["sha256:".len()..]);
    }

    #[test]
    fn a_spelling_that_is_not_one_is_refused() {
        for bad in [
            "",
            "sha256:",
            "2cf24dba",
            "md5:2cf24dba",
            "sha256:zz",
            "sha256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824",
        ] {
            assert!(LayerId::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }
}
