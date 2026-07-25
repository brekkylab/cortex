//! A throwaway toy [`DynFileSystem`] used to exercise the microsandbox
//! virtio-fs plumbing end to end.
//!
//! This is deliberately *not* wired to [`Workspace`]/[`Mountable`]. It serves a
//! single hard-coded tree so we can prove that a custom Rust filesystem shows up
//! inside a sandbox guest:
//!
//! ```text
//! /               (inode 1, dir)
//! └── hello.txt   (inode 2, file)  -> "Hello from cortex toy FUSE!\n"
//! ```
//!
//! `microsandbox` serves every host mount through an in-process virtio-fs device
//! whose backend is a [`DynFileSystem`] (a FUSE-shaped, object-safe trait). Most
//! of its methods default to `ENOSYS`, so a toy only needs a handful: `lookup`,
//! `getattr`, `readdir`, and `read`. Opening relies on the trait defaults.
//!
//! [`Workspace`]: crate::Workspace
//! [`Mountable`]: crate::Mountable

use std::ffi::CStr;
use std::io::{self, Write};
use std::time::Duration;

use msb_krun::backends::fs::{
    Context, DirEntry, DynFileSystem, Entry, FsOptions, ZeroCopyWriter, stat64,
};

/// FUSE's fixed inode for the root directory.
const ROOT_INODE: u64 = 1;
/// The single file this toy exposes.
const FILE_INODE: u64 = 2;
const FILE_NAME: &[u8] = b"hello.txt";
const FILE_CONTENT: &[u8] = b"Hello from cortex toy FUSE!\n";

/// How long the kernel may cache attributes/lookups. Large is fine: the tree
/// never changes.
const TTL: Duration = Duration::from_secs(60);

/// A read-only filesystem with exactly one file at the root.
pub struct ToyFs;

impl ToyFs {
    pub fn new() -> Self {
        ToyFs
    }
}

impl Default for ToyFs {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a zeroed `stat64` and fill only the fields FUSE actually reads back.
/// `as _` coerces to whatever integer widths the platform's `stat64` uses.
fn attr(inode: u64, mode: u32, size: u64) -> stat64 {
    // SAFETY: `stat64` is a plain repr(C) struct of integers; all-zero is a
    // valid (if meaningless) value, and we set every field FUSE inspects.
    let mut st: stat64 = unsafe { std::mem::zeroed() };
    st.st_ino = inode as _;
    st.st_mode = mode as _;
    st.st_size = size as _;
    st.st_nlink = 1 as _;
    st
}

/// The `stat64` for a given inode, or `ENOENT` if we don't know it.
fn attr_for(inode: u64) -> io::Result<stat64> {
    match inode {
        ROOT_INODE => Ok(attr(ROOT_INODE, libc::S_IFDIR as u32 | 0o755, 0)),
        FILE_INODE => Ok(attr(
            FILE_INODE,
            libc::S_IFREG as u32 | 0o644,
            FILE_CONTENT.len() as u64,
        )),
        _ => Err(io::Error::from_raw_os_error(libc::ENOENT)),
    }
}

fn entry_for(inode: u64) -> io::Result<Entry> {
    Ok(Entry {
        inode,
        generation: 0,
        attr: attr_for(inode)?,
        attr_flags: 0,
        attr_timeout: TTL,
        entry_timeout: TTL,
    })
}

impl DynFileSystem for ToyFs {
    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        Ok(FsOptions::empty())
    }

    fn lookup(&self, _ctx: Context, parent: u64, name: &CStr) -> io::Result<Entry> {
        if parent == ROOT_INODE && name.to_bytes() == FILE_NAME {
            entry_for(FILE_INODE)
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOENT))
        }
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: Option<u64>,
    ) -> io::Result<(stat64, Duration)> {
        Ok((attr_for(inode)?, TTL))
    }

    fn readdir_for_each(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        _size: u32,
        offset: u64,
        add_entry: &mut msb_krun::backends::fs::AddDirEntry<'_>,
    ) -> io::Result<()> {
        if inode != ROOT_INODE {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }

        // Entries are streamed in order; `offset` is where the kernel wants us
        // to resume, so skip anything at or before it. We use 1-based offsets
        // (0 is reserved to mean "from the beginning").
        let entries = [
            DirEntry {
                ino: ROOT_INODE as _,
                offset: 1,
                type_: libc::DT_DIR as u32,
                name: b".",
            },
            DirEntry {
                ino: ROOT_INODE as _,
                offset: 2,
                type_: libc::DT_DIR as u32,
                name: b"..",
            },
            DirEntry {
                ino: FILE_INODE as _,
                offset: 3,
                type_: libc::DT_REG as u32,
                name: FILE_NAME,
            },
        ];

        for entry in entries {
            if entry.offset <= offset {
                continue;
            }
            // A return of 0 means the kernel's buffer is full; stop early.
            if add_entry(entry)? == 0 {
                break;
            }
        }
        Ok(())
    }

    fn read(
        &self,
        _ctx: Context,
        inode: u64,
        _handle: u64,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        if inode != FILE_INODE {
            return Err(io::Error::from_raw_os_error(libc::EISDIR));
        }

        let start = (offset as usize).min(FILE_CONTENT.len());
        let end = (start + size as usize).min(FILE_CONTENT.len());
        let slice = &FILE_CONTENT[start..end];
        if slice.is_empty() {
            return Ok(0);
        }

        // `ZeroCopyWriter` copies out of a file descriptor, not a `&[u8]`, so we
        // stage the bytes in a temp file and hand it that fd. Wasteful, but this
        // is only a toy — a real backend would keep data behind an fd already.
        let mut staging = tempfile::tempfile()?;
        staging.write_all(slice)?;
        staging.flush()?;
        w.write_all_from(&mut staging, slice.len(), 0)?;
        Ok(slice.len())
    }
}
