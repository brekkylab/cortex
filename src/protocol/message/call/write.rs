use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// Bytes to put in a file. The `params` of `write`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteCall {
    /// A path in the executor's filesystem, under one of the session's mounts. Parent
    /// directories must already exist; the file need not.
    pub path: String,

    #[serde(default, with = "bytes", skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<u8>,

    /// Where in the file to put them.
    ///
    /// `None` makes the file exactly `data` (created or truncated). `Some(n)` overwrites
    /// from `n`, keeps whatever lies past the written bytes, and zero-fills if `n` is past
    /// the end. To replace a file, send `None`, not `Some(0)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// How big the file is now. The `result` of `write`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResp {
    /// Where a positioned write carries on from; for a whole-file write, confirms the length.
    pub size: u64,
}
