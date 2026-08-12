//! [`VolumeSpec`] and [`WorkspaceSpec`] — a namespace described declaratively, so that two
//! processes can build the same one.
//!
//! A live [`Workspace`](crate::volume::Workspace) holds open clients and runtimes that cannot
//! cross a process boundary. What crosses is this description, which each side realizes.
//!
//! # Every kind is on the wire in every build
//!
//! The `#[cfg]`s are on the **realization** below, never on a variant. A build without `s3`
//! still parses `{"type": "s3", …}` and refuses it with
//! [`UnsupportedVolume`](crate::CortexError::UnsupportedVolume) — *this build has no
//! provider*, a different sentence from *that is not a volume*.
//!
//! On a variant instead, the wire schema would depend on how the server was compiled: one
//! document, a valid spec to one binary and a parse failure to another, and no way for a
//! client to tell either apart from its own bug.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::volume::DynMountable;

/// Connection settings for an S3 volume.
///
/// Here rather than beside `S3Volume`, which is behind the `s3` feature, because a build with
/// no provider still has to parse a spec that names one. Pure data, so it needs no feature.
///
/// `Serialize` keeps the secret, because a far-side rebuild needs it. The hand-written `Debug`
/// does not, because a log does not.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Custom endpoint (MinIO / R2 / localstack); `None` for real AWS.
    pub endpoint: Option<String>,
    /// Key prefix every path is rooted under. Composes with whatever mount path a
    /// [`Workspace`](crate::volume::Workspace) puts this volume at: the workspace strips
    /// its mount path first, then this prefix is prepended.
    pub key_prefix: Option<String>,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[redacted]")
            .field("endpoint", &self.endpoint)
            .field("key_prefix", &self.key_prefix)
            .finish()
    }
}

/// Connection settings for a Notion volume. Here for the reason [`S3Config`] is.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotionConfig {
    pub api_key: String,
}

impl std::fmt::Debug for NotionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConfig")
            .field("api_key", &"[redacted]")
            .finish()
    }
}

/// A cortex volume, described declaratively so it can be serialized and rebuilt elsewhere.
/// Each variant names a backend cortex knows how to construct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VolumeSpec {
    /// A host directory served as-is. See [`crate::volume::PassthroughVolume`].
    ///
    /// The one variant naming a path in somebody's filesystem rather than a service, which
    /// works only because both backends run on the same host as the client today.
    Local { host: PathBuf },

    /// An S3 bucket (read path). See [`S3Config`].
    S3(S3Config),

    /// A Notion workspace (read path). See [`NotionConfig`].
    Notion(NotionConfig),
}

impl VolumeSpec {
    /// Realize this spec into a [`DynMountable`] — no interface binding, so a caller that
    /// never boots a VM stays clear of `msb_krun`.
    ///
    /// This is where a build's features are allowed to matter, and the only place.
    pub fn build_mountable(&self) -> Result<Box<dyn DynMountable>> {
        match self {
            VolumeSpec::Local { host } => Ok(Box::new(crate::volume::PassthroughVolume::new(host))),

            #[cfg(feature = "s3")]
            VolumeSpec::S3(cfg) => Ok(Box::new(crate::volume::S3Volume::new(cfg)?)),
            #[cfg(not(feature = "s3"))]
            VolumeSpec::S3(_) => Err(crate::CortexError::UnsupportedVolume("s3")),

            #[cfg(feature = "notion")]
            VolumeSpec::Notion(cfg) => Ok(Box::new(crate::volume::NotionVolume::new(cfg)?)),
            #[cfg(not(feature = "notion"))]
            VolumeSpec::Notion(_) => Err(crate::CortexError::UnsupportedVolume("notion")),
        }
    }
}

/// Where one volume lands in a workspace.
///
/// A named pair and not a tuple, because this is a wire type: a tuple serializes as
/// `{"0": …, "1": …}` in BSON, which reads as nothing to a person and cannot grow a field
/// without breaking every reader. `{"path": …, "volume": …}` can.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Root-relative, and the workspace's own — not a path in anyone's filesystem. Where
    /// the whole tree lands is the backend's business.
    pub path: String,
    pub volume: VolumeSpec,
}

/// A whole namespace, described declaratively.
///
/// The artifact both sides of a console share: each calls
/// [`Workspace::from_spec`](crate::volume::Workspace::from_spec) to build its own live tree.
/// A live `Workspace`'s instance-local state — its `born` timestamp, any hook — stays behind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    /// In insertion order. A mount at the empty path is the root; two at one path is
    /// refused rather than resolved, so order does not decide a winner.
    pub mounts: Vec<Mount>,
}

impl WorkspaceSpec {
    /// Builder-style: append a mount of `volume` at `path`.
    pub fn mount(mut self, path: impl Into<String>, volume: VolumeSpec) -> Self {
        self.mounts.push(Mount {
            path: path.into(),
            volume,
        });
        self
    }

    /// Whether this declares nothing. An empty namespace is still a session, which is why
    /// there is no `Option` around the spec.
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// Refuse what this spec cannot be written down as, before anything tries to write it.
    ///
    /// One thing today: a [`Local`](VolumeSpec::Local) host path with no UTF-8 form. BSON
    /// strings are UTF-8 and a `PathBuf` here need not be, so without this the failure is a
    /// codec complaint naming neither the mount nor the field.
    ///
    /// [`InvalidName`](crate::CortexError::InvalidName) instead — what a server answers for a
    /// mount path it cannot make sense of, and what a backend turns into `INVALID_PARAMS`.
    ///
    /// Called where a namespace is declared, not on every serialization: a reader has nothing
    /// to check, since a document that parsed was UTF-8.
    pub fn check(&self) -> Result<()> {
        for mount in &self.mounts {
            // The other kinds are `String` all the way down, so there is nothing to check.
            let VolumeSpec::Local { host } = &mount.volume else {
                continue;
            };
            if host.to_str().is_none() {
                return Err(crate::CortexError::InvalidName);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
