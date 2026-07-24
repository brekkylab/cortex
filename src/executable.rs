//! [`Executable`]: a unit of work that runs against a [`Workspace`].

use crate::error::Result;

/// Something that runs against a [`Workspace`](crate::Workspace).
///
/// An implementor holds a borrow of one workspace and, in [`run`](Executable::run),
/// inspects the volumes mounted in it (via
/// [`Workspace::mounts`](crate::Workspace::mounts)) and drives them through the
/// [`Mountable`](crate::Mountable) interface. Because `Mountable` takes `&self`, `run`
/// needs only `&self` even when it writes.
///
/// ```ignore
/// struct Cat<'a> { ws: &'a Workspace }
///
/// impl Executable for Cat<'_> {
///     type Output = Vec<u8>;
///     fn run(&self, args: Vec<String>) -> Result<Vec<u8>> {
///         self.ws.read(Path::new(&args[0]))
///     }
/// }
/// ```
pub trait Executable {
    /// What a successful run produces: an exit code, captured output, `()`, …
    /// Each implementor picks its own.
    type Output;

    /// Run with `args`, operating on the workspace the implementor holds.
    fn run(&self, args: Vec<String>) -> Result<Self::Output>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mountable, Workspace};
    use std::path::{Path, PathBuf};

    /// Reads `args[0]` from the workspace and returns its bytes.
    struct Cat<'a> {
        ws: &'a Workspace,
    }

    impl Executable for Cat<'_> {
        type Output = Vec<u8>;
        fn run(&self, args: Vec<String>) -> Result<Vec<u8>> {
            self.ws.read(Path::new(&args[0]))
        }
    }

    /// Creates an empty file at `args[0]`; `Output = ()` proves the associated
    /// type is per-implementor, and `&self` proves a run may mutate volumes.
    struct Touch<'a> {
        ws: &'a Workspace,
    }

    impl Executable for Touch<'_> {
        type Output = ();
        fn run(&self, args: Vec<String>) -> Result<()> {
            self.ws.write(Path::new(&args[0]), b"")
        }
    }

    #[test]
    fn run_reads_and_writes_through_workspace() {
        let ws = Workspace::new();
        ws.write(Path::new("greeting"), b"hi").unwrap();

        let cat = Cat { ws: &ws };
        assert_eq!(cat.run(vec!["greeting".into()]).unwrap(), b"hi");

        Touch { ws: &ws }.run(vec!["blank".into()]).unwrap();
        assert_eq!(cat.run(vec!["blank".into()]).unwrap(), b"");
    }

    #[test]
    fn implementor_can_inspect_mounts() {
        let ws = Workspace::new();
        // A fresh workspace has exactly the root mount (empty path).
        let mounts: Vec<_> = ws.mounts().map(|(p, _)| p.to_path_buf()).collect();
        assert_eq!(mounts, vec![PathBuf::new()]);
    }
}
