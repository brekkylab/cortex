//! Glue for serving cortex backends inside a microsandbox VM.
//!
//! [`register_s3_backend`] installs an [`S3Volume`]-backed factory in
//! microsandbox's fs-backend registry under [`S3_BACKEND_TYPE`]. The sandbox
//! process (a custom `msb` binary — see `bin/msb_cortex.rs`) calls this at
//! startup; the runtime then resolves any `FsBackendSpec { backend_type:
//! "cortex-s3", .. }` in the launch config into a live virtio-fs backend during
//! `build_vm`.
//!
//! This is the cortex half of the "minimal ailoy integration": the SDK puts an
//! `FsBackendSpec` in the launch config, and this factory turns it back into a
//! running filesystem on the sandbox side.

use std::io;

use serde::{Deserialize, Serialize};

use crate::{PosixAdapter, S3Config, S3Volume};

/// Registry key for the S3 backend. An SDK puts this in
/// `FsBackendSpec::backend_type`; this crate registers the matching factory.
pub const S3_BACKEND_TYPE: &str = "cortex-s3";

/// Serializable S3 connection settings — the `params` blob an SDK serializes
/// into the launch config and this factory parses back.
///
/// For a real deployment the secret fields should be passed by reference (a
/// path or inherited fd named here), not inlined; inlined for the spike.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Params {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub key_prefix: Option<String>,
}

impl S3Params {
    /// Read settings from the standard AWS environment variables.
    /// `AWS_DEFAULT_REGION` defaults to `us-east-1`; endpoint/prefix optional.
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).map_err(|_| format!("missing env {k}"));
        Ok(S3Params {
            bucket: var("AWS_S3_BUCKET")?,
            region: std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
            access_key_id: var("AWS_ACCESS_KEY_ID")?,
            secret_access_key: var("AWS_SECRET_ACCESS_KEY")?,
            endpoint: std::env::var("AWS_S3_ENDPOINT").ok(),
            key_prefix: std::env::var("AWS_S3_KEY_PREFIX").ok(),
        })
    }
}

impl From<&S3Params> for S3Config {
    fn from(p: &S3Params) -> Self {
        S3Config {
            bucket: p.bucket.clone(),
            region: p.region.clone(),
            access_key_id: p.access_key_id.clone(),
            secret_access_key: p.secret_access_key.clone(),
            endpoint: p.endpoint.clone(),
            key_prefix: p.key_prefix.clone(),
        }
    }
}

/// Register the [`S3_BACKEND_TYPE`] factory in microsandbox's fs-backend
/// registry. Call once at sandbox-process startup, before the VM boots.
///
/// The factory maps `params` (JSON [`S3Params`]) → [`S3Volume`] →
/// [`PosixAdapter`] → a boxed `DynFileSystem` the runtime attaches as
/// virtio-fs.
pub fn register_s3_backend() {
    microsandbox_runtime::fs_backend::register_fs_backend(S3_BACKEND_TYPE, |spec| {
        let params: S3Params = serde_json::from_str(&spec.params)
            .map_err(|e| io::Error::other(format!("bad {S3_BACKEND_TYPE} params: {e}")))?;
        let vol = S3Volume::new(&S3Config::from(&params))
            .map_err(|e| io::Error::other(format!("S3 connect: {e}")))?;
        Ok(Box::new(PosixAdapter::new(vol)))
    });
}
