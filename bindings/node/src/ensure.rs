//! `ensureCortex`: the console server, fetched when this host has none.
//!
//! What a console builder and an `ImageClient` start is `cortex-krun` under cortex's cache,
//! and a host that installed only this package has none there -- so this is the one call
//! such a host makes before anything else.

use napi::{Env, bindgen_prelude::PromiseRaw};
use napi_derive::napi;

use crate::{console::promise, error};

/// Fetch the console server into cortex's cache if it is not there, and settle with the
/// directory it is in.
///
/// A server already there is left alone, whether it was fetched or installed by hand.
#[napi(ts_return_type = "Promise<string>")]
pub fn ensure_cortex(env: &Env) -> napi::Result<PromiseRaw<'_, String>> {
    promise(env, async move {
        let bin = cortex::ensure_cortex().await.map_err(error::anyhow)?;
        Ok(bin.to_string_lossy().into_owned())
    })
}
