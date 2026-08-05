//! Demo executables (`cat`, `ls`, `write`) — a copy-me reference and the set
//! `exec-server`/tests use. `write` also implements [`Toolable`], showing the
//! same executable on both front doors. Removable in one step.

use std::path::Path;

use serde_json::{Value, json};

use crate::{
    Bin, ExecOutput, Executable, FileExt, Mountable, OpenOptions, Result, Skillable, Toolable,
    Workspace,
};

/// Read the whole file at `path`.
fn read_all(ws: &Workspace, path: &Path) -> Result<Vec<u8>> {
    // No separate `stat`: `open` answers with the metadata as of the open, which is
    // both one round trip fewer and free of the window a second call would leave for
    // the file to change size.
    let (h, stat) = ws.open(path, OpenOptions::read_only())?;
    let mut buf = vec![0u8; stat.size as usize];
    h.read_exact_at(&mut buf, 0)?;
    Ok(buf)
}

/// Write `data` to `path`, creating or replacing it.
fn write_all(ws: &Workspace, path: &Path, data: &[u8]) -> Result<()> {
    // One call: the backend applies `create` and `truncate` together, so nothing can
    // slip in between them. Reaching the same place by creating, catching
    // `AlreadyExists` and reopening would leave exactly that gap.
    let (h, _) = ws.open(path, OpenOptions::read_write().create(true).truncate(true))?;
    h.write_all_at(data, 0)?;
    Ok(())
}

/// Usage-error output (`code 2`) for a missing required argument.
fn missing_arg(name: &str, usage: &str) -> ExecOutput {
    ExecOutput {
        code: 2,
        stdout: Vec::new(),
        stderr: format!("usage: wsx {name} {usage}\n").into_bytes(),
    }
}

/// Return `args[0]` or short-circuit `exec` with a usage error.
macro_rules! path_arg {
    ($self:ident, $args:ident, $usage:literal) => {
        match $args.first() {
            Some(p) => p.clone(),
            // `Skillable::` disambiguates `name` for `Write` (also `Toolable`).
            None => return Ok(missing_arg(Skillable::name($self), $usage)),
        }
    };
}

/// `cat <path>` — print a file to stdout.
struct Cat;
impl Executable for Cat {
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
        let path = path_arg!(self, args, "<path>");
        Ok(ExecOutput::ok(read_all(ws, Path::new(&path))?))
    }
}
impl Skillable for Cat {
    fn name(&self) -> &str {
        "cat"
    }
    fn summary(&self) -> &str {
        "print a file"
    }
    fn skill(&self) -> String {
        "# cat\n\n`wsx cat <path>` — print the contents of a file to stdout.\n".into()
    }
}

/// `ls <path>` — list a directory, one name per line (sorted).
struct Ls;
impl Executable for Ls {
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
        let path = path_arg!(self, args, "<path>");
        let mut names: Vec<String> = ws
            .list(Path::new(&path))?
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        Ok(ExecOutput::ok(names.join("\n").into_bytes()))
    }
}
impl Skillable for Ls {
    fn name(&self) -> &str {
        "ls"
    }
    fn summary(&self) -> &str {
        "list a directory"
    }
    fn skill(&self) -> String {
        "# ls\n\n`wsx ls <path>` — list directory entries, one name per line.\n".into()
    }
}

/// `write <path> [content]` — write `content` (empty if omitted) to a file.
struct Write;
impl Executable for Write {
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
        let path = path_arg!(self, args, "<path> [content]");
        let content = args.get(1).map(String::as_bytes).unwrap_or(b"");
        write_all(ws, Path::new(&path), content)?;
        Ok(ExecOutput::default())
    }
}
impl Skillable for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn summary(&self) -> &str {
        "write text to a file"
    }
    fn skill(&self) -> String {
        "# write\n\n`wsx write <path> [content]` — write `content` to a file, \
         creating or replacing it. Omit `content` to write an empty file.\n"
            .into()
    }
}
// The same executable as a native tool: `{ path, content }` → argv `[path, content]`.
impl Toolable for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write text to a file, creating or replacing it."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "file to write" },
                "content": { "type": "string", "description": "text to write; empty if omitted" }
            },
            "required": ["path"]
        })
    }
    fn to_argv(&self, args: &Value) -> Vec<String> {
        let mut argv = Vec::new();
        if let Some(p) = args.get("path").and_then(Value::as_str) {
            argv.push(p.to_string());
        }
        if let Some(c) = args.get("content").and_then(Value::as_str) {
            argv.push(c.to_string());
        }
        argv
    }
}

impl Bin {
    /// A `Bin` preloaded with the demo set (`cat`, `ls`, `write`).
    pub fn demo() -> Bin {
        Bin::new().register(Cat).register(Ls).register(Write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemVolume;

    fn ws() -> Workspace {
        Workspace::new()
            .try_with_mount("", InMemVolume::new())
            .unwrap()
    }

    #[test]
    fn demo_set_registered() {
        let bin = Bin::demo();
        let mut names: Vec<&str> = bin.names().collect();
        names.sort();
        assert_eq!(names, ["cat", "ls", "write"]);
    }

    #[test]
    fn write_then_read_back_through_bin() {
        let ws = ws();
        let bin = Bin::demo();
        bin.invoke(&ws, "write", vec!["f".into(), "hi".into()]).unwrap();
        assert_eq!(bin.invoke(&ws, "cat", vec!["f".into()]).unwrap().stdout, b"hi");
        assert_eq!(bin.invoke(&ws, "ls", vec!["".into()]).unwrap().stdout, b"f");
    }

    #[test]
    fn missing_arg_is_usage_error() {
        let out = Bin::demo().invoke(&ws(), "cat", vec![]).unwrap();
        assert_eq!(out.code, 2);
        assert!(out.stderr.starts_with(b"usage: wsx cat"));
    }

    #[test]
    fn write_is_also_a_tool() {
        let tool: &dyn Toolable = &Write;
        assert_eq!(tool.to_argv(&json!({ "path": "f", "content": "hi" })), ["f", "hi"]);
        let ws = ws();
        tool.call(&ws, &json!({ "path": "f", "content": "hi" })).unwrap();
        assert_eq!(read_all(&ws, Path::new("f")).unwrap(), b"hi");
    }
}
