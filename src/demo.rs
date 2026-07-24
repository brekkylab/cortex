//! Demo executables — **not** the intended production set, just enough to
//! exercise the forwarding channel end-to-end (`cat`, `ls`, `write`) and to
//! serve as a copy-me reference for writing a real [`Executable`].
//!
//! Each wraps a single [`Mountable`] op and carries its own `SKILL.md`. They are
//! deliberately self-contained here so the whole demo set is removable in one
//! step: delete this file, drop `mod demo;` from `lib.rs`, and give
//! [`ExecutableRegistry::demo`] a real replacement.

use std::path::Path;

use crate::executable::{ExecOutput, Executable};
use crate::registry::ExecutableRegistry;
use crate::volume::Mountable;
use crate::workspace::Workspace;

/// Usage-error output (`code 2`) for a missing required argument.
fn missing_arg(exec: &dyn Executable) -> ExecOutput {
    ExecOutput {
        code: 2,
        stdout: Vec::new(),
        stderr: format!("usage: wsx {} {}\n", exec.name(), exec.usage()).into_bytes(),
    }
}

/// Return `args[0]` or, if absent, short-circuit `exec` with a usage error.
macro_rules! path_arg {
    ($self:ident, $args:ident) => {
        match $args.first() {
            Some(p) => p.clone(),
            None => return Ok(missing_arg($self)),
        }
    };
}

/// `cat <path>` — print a file to stdout.
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
        "# cat\n\n`wsx cat <path>` — print the contents of a file to stdout.\n".into()
    }
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> crate::Result<ExecOutput> {
        let path = path_arg!(self, args);
        Ok(ExecOutput::ok(ws.read(Path::new(&path))?))
    }
}

/// `ls <path>` — list a directory, one name per line (sorted).
struct Ls;
impl Executable for Ls {
    fn name(&self) -> &str {
        "ls"
    }
    fn summary(&self) -> &str {
        "list a directory"
    }
    fn usage(&self) -> &str {
        "<path>"
    }
    fn skill(&self) -> String {
        "# ls\n\n`wsx ls <path>` — list directory entries, one name per line.\n".into()
    }
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> crate::Result<ExecOutput> {
        let path = path_arg!(self, args);
        let mut names: Vec<String> = ws
            .list(Path::new(&path))?
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        names.sort();
        Ok(ExecOutput::ok(names.join("\n").into_bytes()))
    }
}

/// `write <path> [content]` — write `content` (empty if omitted) to a file.
struct Write;
impl Executable for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn summary(&self) -> &str {
        "write text to a file"
    }
    fn usage(&self) -> &str {
        "<path> [content]"
    }
    fn skill(&self) -> String {
        "# write\n\n`wsx write <path> [content]` — write `content` to a file, \
         creating or replacing it. Omit `content` to write an empty file.\n"
            .into()
    }
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> crate::Result<ExecOutput> {
        let path = path_arg!(self, args);
        let content = args.get(1).map(String::as_bytes).unwrap_or(b"");
        ws.write(Path::new(&path), content)?;
        Ok(ExecOutput::default())
    }
}

impl ExecutableRegistry {
    /// A registry preloaded with the demo set (`cat`, `ls`, `write`). Stand-in
    /// wiring for `exec-server`/tests until real executables are registered.
    pub fn demo() -> ExecutableRegistry {
        ExecutableRegistry::new()
            .register(Cat)
            .register(Ls)
            .register(Write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_set_registered() {
        let reg = ExecutableRegistry::demo();
        let mut names: Vec<&str> = reg.names().collect();
        names.sort();
        assert_eq!(names, ["cat", "ls", "write"]);
    }

    #[test]
    fn write_then_read_back_through_registry() {
        let ws = Workspace::new();
        let reg = ExecutableRegistry::demo();

        reg.invoke(&ws, "write", vec!["f".into(), "hi".into()]).unwrap();
        assert_eq!(reg.invoke(&ws, "cat", vec!["f".into()]).unwrap().stdout, b"hi");
        assert_eq!(reg.invoke(&ws, "ls", vec!["".into()]).unwrap().stdout, b"f");
    }

    #[test]
    fn missing_arg_is_usage_error() {
        let ws = Workspace::new();
        let out = ExecutableRegistry::demo().invoke(&ws, "cat", vec![]).unwrap();
        assert_eq!(out.code, 2);
        assert!(out.stderr.starts_with(b"usage: wsx cat"));
    }
}
