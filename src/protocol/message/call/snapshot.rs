use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// Take what this session has written so far. The `params` of `snapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCall {}

/// What the snapshot came out as. The `result` of `snapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResp {
    /// The session's writes, in the form [`InitCall::snapshot`](super::InitCall::snapshot)
    /// takes, so an `init` given this starts where this session stopped.
    ///
    /// Must fit one frame. A larger session gets an error rather than a truncated blob,
    /// which would be a broken tree.
    #[serde(with = "bytes")]
    pub blob: Vec<u8>,
}
