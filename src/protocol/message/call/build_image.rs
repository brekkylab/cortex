use serde::{Deserialize, Serialize};

use crate::image::Recipe;

/// A recipe to build. The `params` of `build_image`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildImageCall {
    /// What to build.
    pub recipe: Recipe,

    /// The name to store the build under, as `name:tag`. `None` has the server pick one,
    /// which comes back in [`BuildImageResp::reference`].
    ///
    /// A ref already in use is moved to this build.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// What the build came out as. The `result` of `build_image`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildImageResp {
    /// The ref the build is stored under: the one asked for, or the one the server picked.
    #[serde(rename = "ref")]
    pub reference: String,

    /// The build, as `algorithm:hex`. What [`ImageSource::digest`](crate::image::ImageSource::digest)
    /// takes, so an `init` given it runs on exactly this build.
    pub digest: String,
}
