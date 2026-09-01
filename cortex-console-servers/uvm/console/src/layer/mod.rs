//! Layers, and the disk several of them make.
//!
//! A **layer** is one EROFS file plus the [`ErofsDataMap`] that was returned when it was
//! written. The map is kept beside it because it cannot be recovered afterwards — see
//! [`map`], which is the whole reason this is a store rather than a directory.
//!
//! **Stitching** turns an ordered set of layers into one disk: the layers' trees are merged
//! — later layers winning, whiteouts deleting, opaque directories emptying — and the merged
//! tree is written as a metadata-only EROFS whose inodes point into the layer files, which a
//! VMDK descriptor concatenates behind it. The guest mounts the result as `erofs` and knows
//! nothing about any of it.
//!
//! # A base layer is an image; an upper layer is not
//!
//! A base carries no whiteouts, so the same file serves both as a stitch extent and as a raw
//! read-only disk — which is what lets a session that names no image attach a layer directly.
//! An upper layer carries character devices standing for deletions and directories marked
//! opaque, and both mean nothing to a kernel mounting it as a root.
//!
//! [`ErofsDataMap`]: microsandbox_image::erofs::ErofsDataMap

pub mod atomic;
mod id;
pub mod map;
pub mod tree;

pub use id::LayerId;
