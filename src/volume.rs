//! [`VolumeSpec`] — the serializable, extensible catalog of cortex volumes.
//!
//! A caller (e.g. ailoy) that boots a VM in a separate process carries a
//! `VolumeSpec` across the process boundary as plain data, then calls
//! [`VolumeSpec::build`] on the far side to realize it into a virtio-fs backend.
//! Adding a new volume kind is one variant here — callers that just pass
//! `VolumeSpec` through need no changes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{DynMountable, Result};

/// A cortex volume, described declaratively so it can be serialized and rebuilt
/// elsewhere. Each variant names a backend cortex knows how to construct.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VolumeSpec {
    /// A host directory served as-is (passthrough). See [`crate::PassthroughVolume`].
    Local { host: PathBuf },

    /// An S3 bucket (read path). See [`crate::S3Config`].
    #[cfg(feature = "s3")]
    S3(crate::S3Config),

    /// A Notion workspace (read path). See [`crate::NotionConfig`].
    #[cfg(feature = "notion")]
    Notion(crate::NotionConfig),
}

impl VolumeSpec {
    /// Realize this spec into a [`DynMountable`] — the pure-VFS layer, with no
    /// interface binding. This is what [`Workspace::from_spec`](crate::Workspace::from_spec)
    /// composes; a caller that never boots a VM (a WebDAV or host-FUSE frontend)
    /// stays clear of `msb_krun`.
    pub fn build_mountable(&self) -> Result<Box<dyn DynMountable>> {
        match self {
            VolumeSpec::Local { host } => Ok(Box::new(crate::PassthroughVolume::new(host))),
            #[cfg(feature = "s3")]
            VolumeSpec::S3(cfg) => Ok(Box::new(crate::S3Volume::new(cfg)?)),
            #[cfg(feature = "notion")]
            VolumeSpec::Notion(cfg) => Ok(Box::new(crate::NotionVolume::new(cfg)?)),
        }
    }
}

#[cfg(feature = "krun")]
impl VolumeSpec {
    /// Realize this spec into a boxed [`msb_krun::DynFileSystem`] ready to attach
    /// to a VM's virtio-fs (via `VmBuilder::fs(..).custom(..)`). [`crate::PosixFs`]
    /// binds each concrete [`Mountable`](crate::Mountable) to that interface.
    pub fn build(&self) -> crate::Result<Box<dyn msb_krun::DynFileSystem + Send + Sync>> {
        use crate::PosixFs;
        match self {
            VolumeSpec::Local { host } => {
                Ok(Box::new(PosixFs::new(crate::PassthroughVolume::new(host))))
            }
            #[cfg(feature = "s3")]
            VolumeSpec::S3(cfg) => Ok(Box::new(PosixFs::new(crate::S3Volume::new(cfg)?))),
            #[cfg(feature = "notion")]
            VolumeSpec::Notion(cfg) => Ok(Box::new(PosixFs::new(crate::NotionVolume::new(cfg)?))),
        }
    }
}
