//! [`ExecutableRegistry`]: the host-side set of predefined executables a VM may
//! invoke by name.
//!
//! The registry **is the allowlist** — a name absent from it cannot run, so the
//! VM can only trigger host execution of code that was registered up front. It
//! also emits the VM-facing docs: one `SKILL.md` per executable plus a single
//! `AGENT.md` entry point that tells an agent how to invoke them via `wsx`.

use std::collections::BTreeMap;

use crate::error::{Result, CortexError};
use crate::executable::{ExecOutput, Executable};
use crate::workspace::Workspace;

/// A startup-built set of named [`Executable`]s. Registration order does not
/// matter; entries are keyed and iterated by name.
#[derive(Default)]
pub struct ExecutableRegistry {
    execs: BTreeMap<String, Box<dyn Executable>>,
}

impl ExecutableRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `exec` under its [`name`](Executable::name). Builder-style;
    /// re-registering a name overwrites (setup code owns the final set).
    pub fn register(mut self, exec: impl Executable + 'static) -> Self {
        self.execs.insert(exec.name().to_string(), Box::new(exec));
        self
    }

    /// The registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.execs.keys().map(String::as_str)
    }

    fn get(&self, name: &str) -> Option<&dyn Executable> {
        self.execs.get(name).map(Box::as_ref)
    }

    /// Invoke the named executable against `ws`.
    ///
    /// `Err(NotFound)` means the *name* is not registered — the allowlist
    /// boundary, an infrastructure error. A failure *inside* the executable is
    /// not an `Err`: it is reported the way a program reports failure, as an
    /// [`ExecOutput`] with a non-zero `code` and the message on `stderr`.
    pub fn invoke(&self, ws: &Workspace, name: &str, args: Vec<String>) -> Result<ExecOutput> {
        let executable = self.get(name).ok_or(CortexError::NotFound)?;
        Ok(match executable.exec(ws, args) {
            Ok(out) => out,
            Err(e) => ExecOutput {
                code: 1,
                stdout: Vec::new(),
                stderr: e.to_string().into_bytes(),
            },
        })
    }

    /// Per-executable skill docs as `(guest SKILL.md path, contents)`, planted
    /// under `skills_root` (e.g. `/root/skills`). The delivery mechanism/root is
    /// the caller's choice; this only renders the layout + content.
    pub fn skills(&self, skills_root: &str) -> Vec<(String, String)> {
        let root = skills_root.trim_end_matches('/');
        self.execs
            .values()
            .map(|e| (format!("{root}/{}/SKILL.md", e.name()), e.skill()))
            .collect()
    }

    /// The `AGENT.md` entry point: how to invoke plus the executable catalog,
    /// linking each skill under `skills_root`.
    pub fn agent_md(&self, skills_root: &str) -> String {
        let root = skills_root.trim_end_matches('/');
        let mut s = String::new();
        s.push_str("# Workspace Executables\n\n");
        s.push_str("Run a predefined executable via your bash tool:\n\n");
        s.push_str("```\nwsx <name> [args...]\n```\n\n");
        s.push_str("It runs on the host against this workspace and returns its output.\n");
        s.push_str("For a specific executable, read its skill first.\n\n");
        s.push_str("## Available\n\n");
        s.push_str("| name | summary | usage | skill |\n");
        s.push_str("|------|---------|-------|-------|\n");
        for e in self.execs.values() {
            s.push_str(&format!(
                "| `{}` | {} | `wsx {} {}` | `{root}/{}/SKILL.md` |\n",
                e.name(),
                e.summary(),
                e.name(),
                e.usage(),
                e.name(),
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mountable;
    use std::path::Path;

    struct Cat;
    impl Executable for Cat {
        fn name(&self) -> &str {
            "cat"
        }
        fn summary(&self) -> &str {
            "print a file"
        }
        fn usage(&self) -> &str {
            "<path>"
        }
        fn skill(&self) -> String {
            "# cat\n`wsx cat <path>`".into()
        }
        fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
            Ok(ExecOutput::ok(ws.read(Path::new(&args[0]))?))
        }
    }

    fn registry() -> ExecutableRegistry {
        ExecutableRegistry::new().register(Cat)
    }

    #[test]
    fn dispatch_runs_registered_executable() {
        let ws = Workspace::new();
        ws.write(Path::new("f"), b"hi").unwrap();
        let reg = registry();
        let out = reg.invoke(&ws, "cat", vec!["f".into()]).unwrap();
        assert_eq!(out.stdout, b"hi");
    }

    #[test]
    fn unknown_name_is_not_found() {
        let ws = Workspace::new();
        assert!(matches!(
            registry().invoke(&ws, "nope", vec![]),
            Err(CortexError::NotFound)
        ));
    }

    #[test]
    fn executable_failure_is_nonzero_code_not_err() {
        // Cat on a missing file: the executable fails, but that is program
        // failure (code != 0), not an infra Err.
        let ws = Workspace::new();
        let out = registry().invoke(&ws, "cat", vec!["missing".into()]).unwrap();
        assert_eq!(out.code, 1);
        assert!(!out.stderr.is_empty());
    }

    #[test]
    fn skills_and_agent_md_generated() {
        let reg = registry();
        let skills = reg.skills("/root/skills/");
        assert_eq!(
            skills,
            vec![("/root/skills/cat/SKILL.md".to_string(), "# cat\n`wsx cat <path>`".to_string())]
        );

        let md = reg.agent_md("/root/skills");
        assert!(md.contains("wsx <name> [args...]"));
        assert!(md.contains("| `cat` | print a file | `wsx cat <path>` | `/root/skills/cat/SKILL.md` |"));
    }
}
