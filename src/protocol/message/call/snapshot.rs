use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// Take what this session has written so far. The `params` of `snapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCall {}

/// What the snapshot came out as. The `result` of `snapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResp {
    /// The session's writes, in the form [`InitCall::snapshot`](super::InitCall::snapshot)
    /// takes — so that an `init` given this back starts where this session stopped.
    ///
    /// Bounded by what one frame holds, like every other result here. A session that has
    /// written more than that is answered with an error rather than a shortened blob: a
    /// snapshot is read back as a filesystem, and half of one is not a smaller session's
    /// work but a broken tree.
    #[serde(with = "bytes")]
    pub blob: Vec<u8>,
}
