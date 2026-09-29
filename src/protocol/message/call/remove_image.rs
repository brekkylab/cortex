use serde::{Deserialize, Serialize};

use crate::image::ImageSource;

/// A built image to forget. The `params` of `remove_image`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveImageCall {
    /// Which image, by ref or by digest.
    pub image: ImageSource,
}

/// The image is gone. The `result` of `remove_image`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveImageResp {}
