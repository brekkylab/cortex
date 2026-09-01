//! The scratch directory that makes delegated executables real.
//!
//! A delegated executable is virtual only in that its behaviour lives somewhere else —
//! here, on the far side of a hypervisor, in the client's own process. `execvp` does not
//! care where behaviour lives, but it does insist on finding a name on `PATH` backed by
//! something the kernel can exec. So it gets exactly that: a directory of symlinks, one
//! per delegated name, every one of them pointing back at this binary.
//!
//! One binary, N names. The link is what `execvp` resolves; `argv[0]` is what tells the
//! re-executed process which name it was called by. Nothing is built per executable and
//! nothing is shipped alongside — adding one costs a `symlink(2)`.
//!
//! # What the links point at, and why it is not `current_exe`
//!
//! [`GUEST_BIN_PATH`], which is the copy [`init`](crate::init) made after the pivot. The
//! path this process was exec'd from named a file on the boot root, and the boot root is
//! detached by then: `/proc/self/exe` still resolves to the inode, but the *name* it
//! reports is a path that no longer exists, and a symlink is a name.
//!
//! # No sweep for what an earlier run left behind
//!
//! [`cortex-local-console`] needs one, because its scratch directories accumulate in a
//! host's `$TMPDIR` across runs that were killed before they could clean up. There is no
//! equivalent here: this filesystem is one session's writable overlay, there is one agent
//! in it, and the session ending deletes the whole image. A directory this process
//! abandons is deleted along with everything else it wrote.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use crate::contract::GUEST_BIN_PATH;

/// The `bin/` that goes on `PATH`, and nothing else.
///
/// The socket a shim dials is not here. It is bound once for the process rather than per
/// session, so its lifetime is not this directory's — see [`Shims`](super::Shims) and
/// [`ipc`](crate::ipc).
///
/// Owns the directory outright — dropping it removes the tree, so the caller keeps it
/// alive for exactly as long as the names should be callable.
pub struct BinDir {
    root: PathBuf,
}

impl BinDir {
    /// Create the directory and link every name in `names` to this binary.
    pub fn create<'a>(names: impl IntoIterator<Item = &'a str>) -> io::Result<Self> {
        let root = std::env::temp_dir().join(format!("cortex-console-{}", std::process::id()));
        // A previous boot of this session left one behind: the upper persists across the
        // VMs, the pids do not. Start clean.
        let _ = fs::remove_dir_all(&root);

        // 0700: every name in here re-enters this binary as a shim, so the directory
        // permission is what keeps another user in the guest from putting one on our
        // `PATH`.
        DirBuilder::new().mode(0o700).create(&root)?;
        let dir = BinDir { root };
        DirBuilder::new().mode(0o700).create(dir.bin())?;

        for name in names {
            symlink(GUEST_BIN_PATH, dir.bin().join(name))?;
        }
        Ok(dir)
    }

    /// The directory to put on `PATH`.
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// The whole directory, which is what a commit has to leave out.
    ///
    /// `bin()` is only part of it, and the part is not what a session should be committing:
    /// none of this is its work.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for BinDir {
    fn drop(&mut self) {
        // Nothing useful to do about a failure here: we are on the way out, and the
        // session's image goes with us.
        let _ = fs::remove_dir_all(&self.root);
    }
}
