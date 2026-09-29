use serde::{Deserialize, Serialize};

/// Which protocol version the server speaks. The `params` of `version`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionCall {}

/// The protocol version the server speaks. The `result` of `version`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResp {
    /// The version of this protocol, not of the server's own build.
    pub version: String,
}
