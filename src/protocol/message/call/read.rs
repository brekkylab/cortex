use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// Part of a file to hand back. The `params` of `read`.
///
/// A path is one in the executor's own filesystem, under the
/// [`guest_path`](MountSpec::guest_path) of one of the session's mounts — so the file this names is the one a command would open by
/// the same name, and reading it is how a requester sees what an execution left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadCall {
    /// UTF-8, and the executor's own: a requester builds it by joining onto the path it
    /// named the mount at, which is the one both ends have a name for.
    pub path: String,

    /// Where in the file to start. `None` is the beginning.
    ///
    /// Past the end is not an error: the answer is empty
    /// [`data`](super::ReadResp::data) and the [`size`](super::ReadResp::size) that says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,

    /// How many bytes to hand back at most, `None` for as many as there are.
    ///
    /// Either way the answer travels in one message under
    /// [`MAX_PAYLOAD`](crate::console::MAX_PAYLOAD), so the executor hands back less than this
    /// asks for when the rest would not fit. Comparing what arrives against
    /// [`size`](super::ReadResp::size) is how a requester knows, and asking again from
    /// further along is how it gets the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

/// The bytes a `read` asked for, and how big the file is. The `result` of `read`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResp {
    /// What was there, starting at the [`offset`](super::ReadCall::offset) that was asked
    /// for.
    #[serde(with = "bytes")]
    pub data: Vec<u8>,

    /// The whole file's size, and not `data`'s length.
    ///
    /// The two differ whenever a read was bounded — by a [`len`](super::ReadCall::len), by
    /// an [`offset`](super::ReadCall::offset) past the beginning, or by what one message
    /// holds — and the difference is the only thing that says there is more to ask for. A
    /// reader that ignores it has no way to tell a whole small file from the front of a
    /// large one.
    pub size: u64,
}
