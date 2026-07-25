//! POSIX inode bookkeeping over a path-addressed [`MountableV2`] backend.
//!
//! [`PosixAdapter`] maps the backend's paths onto stable inode numbers and
//! tracks the kernel's per-inode reference counts. It carries no FUSE/`msb_krun`
//! coupling of its own — a concrete filesystem binding (the sibling `krun`
//! module) drives it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::mountable::MountableV2;

/// FUSE's fixed inode number for the root directory.
const ROOT_INODE: u64 = 1;

/// Adapts a path-addressed [`MountableV2`] into stable inode numbers with the
/// reference-counted bookkeeping a FUSE binding needs.
///
/// It owns the backend plus the inode table ([`InodeTable`]) that translates
/// the kernel's inode numbers back into backend paths.
pub struct PosixAdapter<T: MountableV2> {
    pub(super) mountable: T,

    pub(super) inodes: Mutex<InodeTable>,
}

impl<T: MountableV2> PosixAdapter<T> {
    pub fn new(mountable: T) -> Self {
        PosixAdapter {
            mountable,
            inodes: Mutex::new(InodeTable::new()),
        }
    }
}

/// One live inode: the path it maps to and how many outstanding kernel
/// references (successful `lookup`s not yet balanced by `forget`) it holds.
struct InodeData {
    path: PathBuf,

    lookup_count: u64,
}

/// The bidirectional inode<->path map plus a monotonic number allocator.
///
/// `next` only ever increases, so a number is never reused even after its entry
/// is forgotten. That keeps every *live* inode unique within the mount and
/// sidesteps generation churn — u64 won't wrap in any realistic lifetime.
pub(super) struct InodeTable {
    /// inode -> path + reference count. The authority for "what does this inode
    /// mean"; every FUSE call that receives only an inode resolves through it.
    fwd: HashMap<u64, InodeData>,

    /// path -> inode, so a repeated `lookup` of the same path reuses its inode
    /// instead of minting a second one (which would break dedup by `st_ino`).
    rev: HashMap<PathBuf, u64>,

    /// The next number to hand out.
    next: u64,
}

impl InodeTable {
    fn new() -> Self {
        let root = PathBuf::from("/");
        let mut fwd = HashMap::new();
        fwd.insert(
            ROOT_INODE,
            InodeData {
                path: root.clone(),
                lookup_count: 1,
            },
        );
        let mut rev = HashMap::new();
        rev.insert(root, ROOT_INODE);
        InodeTable {
            fwd,
            rev,
            next: ROOT_INODE + 1,
        }
    }

    /// The path an inode maps to, or `None` if we've forgotten it (or never
    /// issued it).
    pub(super) fn path_of(&self, inode: u64) -> Option<PathBuf> {
        self.fwd.get(&inode).map(|data| data.path.clone())
    }

    /// Return the inode for `path`, allocating a fresh number the first time,
    /// and record one more kernel reference. Pairs with [`forget`](Self::forget).
    pub(super) fn intern(&mut self, path: PathBuf) -> u64 {
        if let Some(&inode) = self.rev.get(&path) {
            self.fwd.get_mut(&inode).unwrap().lookup_count += 1;
            return inode;
        }
        let inode = self.next;
        self.next += 1;
        self.fwd.insert(
            inode,
            InodeData {
                path: path.clone(),
                lookup_count: 1,
            },
        );
        self.rev.insert(path, inode);
        inode
    }

    /// Drop `count` kernel references to `inode`, evicting it once none remain.
    /// A no-op for inodes we don't know (already forgotten, or never issued).
    pub(super) fn forget(&mut self, inode: u64, count: u64) {
        if let Some(data) = self.fwd.get_mut(&inode) {
            data.lookup_count = data.lookup_count.saturating_sub(count);
            if data.lookup_count == 0 {
                let path = data.path.clone();
                self.fwd.remove(&inode);
                self.rev.remove(&path);
            }
        }
    }
}
