use serde::{Deserialize, Serialize};

use crate::image::ImageEntries;

/// Every image this server has built. The `params` of `list_images`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListImagesCall {}

/// Every image this server has built. The `result` of `list_images`.
pub type ListImagesResp = ImageEntries;
