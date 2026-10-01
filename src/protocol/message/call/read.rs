use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// Part of a file to hand back. The `params` of `read`.
///
/// A path in the executor's own filesystem, under the
/// [`guest_path`](super::MountSpec::guest_path) of one of the session's mounts: the same file a
/// command would open by that name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadCall {
    /// Built by joining onto the path the requester named the mount at, the one name both
    /// ends share.
    ///
    /// A `String`, not bytes, because both ends interpret it; the executor maps it onto
    /// whatever filesystem it has.
    pub path: String,

    /// Where in the file to start. `None` is the beginning.
    ///
    /// Past the end is not an error: [`data`](super::ReadResp::data) comes back empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,

    /// At most this many bytes; `None` for as many as there are.
    ///
    /// The answer must fit one message under [`MAX_PAYLOAD`](crate::protocol::MAX_PAYLOAD),
    /// so fewer may come back; compare with [`size`](super::ReadResp::size) and read on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

/// The bytes a `read` asked for, and how big the file is. The `result` of `read`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResp {
    /// Bytes from the requested [`offset`](super::ReadCall::offset).
    #[serde(with = "bytes")]
    pub data: Vec<u8>,

    /// The whole file's size, not `data`'s length.
    ///
    /// They differ whenever a read was bounded (by `len`, a nonzero `offset`, or message
    /// size); this is the only way to tell a small whole file from the front of a large one.
    pub size: u64,
}
