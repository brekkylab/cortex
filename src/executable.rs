//! [`Executable`]: a named unit of work that runs against a [`Workspace`].
//!
//! An `Executable` is designed to run **identically on the Rust host and from
//! inside a microsandbox VM**. In the VM it is not run directly: the guest `wsx`
//! CLI forwards `exec` to the host, the host executes it against the shared
//! `Workspace`, and the [`ExecOutput`] is returned. So an `Executable` is
//! workspace-agnostic — the workspace is passed into [`exec`](Executable::exec)
//! per call — which lets a host [`ExecutableRegistry`](crate::ExecutableRegistry)
//! store many of them as `Box<dyn Executable>` and serve each request against
//! the workspace that request is scoped to.

use crate::error::Result;
use crate::workspace::Workspace;

pub use crate::wire::ExecOutput;

/// Something that runs against a [`Workspace`] and returns process-like output.
///
/// Object-safe so a registry can hold `Box<dyn Executable>`. Alongside [`exec`],
/// each implementor carries the metadata used to expose it to a VM agent: a
/// [`name`](Executable::name) (the registry key / `wsx <name>`) and a
/// [`skill`](Executable::skill) (the `SKILL.md` teaching the agent how to call it).
///
/// ```ignore
/// struct Cat;
/// impl Executable for Cat {
///     fn name(&self) -> &str { "cat" }
///     fn skill(&self) -> String { "# cat\n`wsx cat <path>` — print a file".into() }
///     fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
///         Ok(ExecOutput::ok(ws.read(Path::new(&args[0]))?))
///     }
/// }
/// ```
pub trait Executable {
    /// Stable identifier the VM addresses this executable by — the registry key
    /// and the `<name>` in `wsx <name> [args]`.
    fn name(&self) -> &str;

    /// One-line summary, listed in `AGENT.md`.
    fn summary(&self) -> &str {
        ""
    }

    /// Argument usage, e.g. `"<path>"`, surfaced in the skill / `AGENT.md`.
    fn usage(&self) -> &str {
        ""
    }

    /// The `SKILL.md` body documenting how a VM agent invokes this executable
    /// (via its bash tool + `wsx`). One skill per executable.
    fn skill(&self) -> String;

    /// Run against `ws` with `args`. Returns process-like [`ExecOutput`]; a
    /// non-zero `code` reports the executable's own failure, while `Err` is for
    /// infrastructure failures (bad path, backend error).
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mountable, Workspace};
    use std::path::{Path, PathBuf};

    /// Prints `args[0]` from the workspace to stdout.
    struct Cat;

    impl Executable for Cat {
        fn name(&self) -> &str {
            "cat"
        }
        fn skill(&self) -> String {
            "# cat\n`wsx cat <path>` — print a file".into()
        }
        fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
            Ok(ExecOutput::ok(ws.read(Path::new(&args[0]))?))
        }
    }

    /// Creates an empty file at `args[0]`; returns the default (code 0) output.
    struct Touch;

    impl Executable for Touch {
        fn name(&self) -> &str {
            "touch"
        }
        fn skill(&self) -> String {
            "# touch\n`wsx touch <path>` — create an empty file".into()
        }
        fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
            ws.write(Path::new(&args[0]), b"")?;
            Ok(ExecOutput::default())
        }
    }

    #[test]
    fn run_reads_and_writes_through_workspace() {
        let ws = Workspace::new();
        ws.write(Path::new("greeting"), b"hi").unwrap();

        let out = Cat.exec(&ws, vec!["greeting".into()]).unwrap();
        assert_eq!(out.stdout, b"hi");
        assert_eq!(out.code, 0);

        Touch.exec(&ws, vec!["blank".into()]).unwrap();
        assert_eq!(Cat.exec(&ws, vec!["blank".into()]).unwrap().stdout, b"");
    }

    #[test]
    fn implementor_can_inspect_mounts() {
        let ws = Workspace::new();
        // A fresh workspace has exactly the root mount (empty path).
        let mounts: Vec<_> = ws.mounts().map(|(p, _)| p.to_path_buf()).collect();
        assert_eq!(mounts, vec![PathBuf::new()]);
    }
}
