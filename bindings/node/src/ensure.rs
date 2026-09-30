//! `ensureCortex`: fetch the console server when this host has none.

use napi::{Env, bindgen_prelude::PromiseRaw};
use napi_derive::napi;

use crate::{console::promise, error};

/// Fetch the console server into cortex's cache if missing, and settle with its directory.
///
/// Console builders and `ImageClient` start `cortex-krun` from that cache, which a host that
/// installed only this package lacks, so such a host calls this first. An existing server,
/// fetched or installed by hand, is left alone.
#[napi(ts_return_type = "Promise<string>")]
pub fn ensure_cortex(env: &Env) -> napi::Result<PromiseRaw<'_, String>> {
    promise(env, async move {
        let bin = cortex::ensure_cortex().await.map_err(error::anyhow)?;
        Ok(bin.to_string_lossy().into_owned())
    })
}
