//! [`VolumeSpec`] — the serializable, extensible catalog of cortex volumes.
//!
//! A caller (e.g. ailoy) that boots a VM in a separate process carries a
//! `VolumeSpec` across the process boundary as plain data, then calls
//! [`VolumeSpec::build`] on the far side to realize it into a virtio-fs backend.
//! Adding a new volume kind is one variant here — callers that just pass
//! `VolumeSpec` through need no changes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::volume::DynMountable;

/// A cortex volume, described declaratively so it can be serialized and rebuilt
/// elsewhere. Each variant names a backend cortex knows how to construct.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VolumeSpec {
    /// A host directory served as-is (passthrough). See [`crate::volume::PassthroughVolume`].
    Local { host: PathBuf },

    /// An S3 bucket (read path). See [`crate::volume::S3Config`].
    #[cfg(feature = "s3")]
    S3(crate::volume::S3Config),

    /// A Notion workspace (read path). See [`crate::volume::NotionConfig`].
    #[cfg(feature = "notion")]
    Notion(crate::volume::NotionConfig),
}

impl VolumeSpec {
    /// Realize this spec into a [`DynMountable`] — the pure-VFS layer, with no
    /// interface binding. This is what [`Workspace::from_spec`](crate::volume::Workspace::from_spec)
    /// composes; a caller that never boots a VM (a WebDAV or host-FUSE frontend)
    /// stays clear of `msb_krun`.
    pub fn build_mountable(&self) -> Result<Box<dyn DynMountable>> {
        match self {
            VolumeSpec::Local { host } => Ok(Box::new(crate::volume::PassthroughVolume::new(host))),
            #[cfg(feature = "s3")]
            VolumeSpec::S3(cfg) => Ok(Box::new(crate::volume::S3Volume::new(cfg)?)),
            #[cfg(feature = "notion")]
            VolumeSpec::Notion(cfg) => Ok(Box::new(crate::volume::NotionVolume::new(cfg)?)),
        }
    }
}

#[cfg(feature = "krun")]
impl VolumeSpec {
    /// Realize this spec into a boxed [`msb_krun::DynFileSystem`] ready to attach
    /// to a VM's virtio-fs (via `VmBuilder::fs(..).custom(..)`). [`crate::volume::PosixFs`]
    /// binds each concrete [`Mountable`](crate::volume::Mountable) to that interface.
    pub fn build(&self) -> crate::Result<Box<dyn msb_krun::DynFileSystem + Send + Sync>> {
        use crate::volume::PosixFs;
        match self {
            VolumeSpec::Local { host } => Ok(Box::new(PosixFs::new(
                crate::volume::PassthroughVolume::new(host),
            ))),
            #[cfg(feature = "s3")]
            VolumeSpec::S3(cfg) => Ok(Box::new(PosixFs::new(crate::volume::S3Volume::new(cfg)?))),
            #[cfg(feature = "notion")]
            VolumeSpec::Notion(cfg) => Ok(Box::new(PosixFs::new(
                crate::volume::NotionVolume::new(cfg)?,
            ))),
        }
    }
}
