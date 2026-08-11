//! [`Executable`] — a unit of work run against a [`Workspace`] — plus two ways
//! to package one for an agent: [`Skillable`] (a `wsx` skill) and [`Toolable`]
//! (a native tool). [`Bin`] is a named set of them; [`Bin::as_dir`] projects
//! their docs as a read-only [`SkillDir`] mount.

use std::{
    collections::BTreeMap,
    io,
    path::{Component, Path},
};

use async_trait::async_trait;
use serde_json::{Value, json};

pub use crate::wire::ExecOutput;
use crate::{
    CortexError, Dirent, DirentKind, FileExt, FileHandle, Mountable, OpenOptions, Result, Stat,
    Workspace,
};

/// Something that runs against a [`Workspace`] and returns process-like output.
/// `Err` is infrastructure failure; a program's own failure is a non-zero
/// `code` in the [`ExecOutput`].
#[async_trait]
pub trait Executable: Send + Sync {
    async fn exec(&self, ws: &Workspace, args: Vec<String>) -> Result<ExecOutput>;
}

/// An [`Executable`] packaged as a `wsx` skill: addressable by `name`, with
/// markdown docs an agent reads.
pub trait Skillable: Executable {
    fn name(&self) -> &str;
    fn summary(&self) -> &str {
        ""
    }
    /// The `SKILL.md` body.
    fn skill(&self) -> String;
}

/// An [`Executable`] packaged as a native tool-calling tool.
#[async_trait]
pub trait Toolable: Executable {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    /// JSON Schema for the tool's arguments.
    fn parameters(&self) -> Value;

    /// Map structured `args` onto exec's positional argv (owned by the tool).
    fn to_argv(&self, args: &Value) -> Vec<String>;

    /// Map output into the tool result. Default: stdout text, or `{error,code}`.
    fn to_result(&self, out: ExecOutput) -> Value {
        if out.code == 0 {
            Value::String(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            json!({ "error": String::from_utf8_lossy(&out.stderr), "code": out.code })
        }
    }

    /// `to_argv` → `exec` → `to_result`. `Err` is infra; a tool failure is in the `Value`.
    async fn call(&self, ws: &Workspace, args: &Value) -> Result<Value> {
        Ok(self.to_result(self.exec(ws, self.to_argv(args)).await?))
    }
}

/// A named, documented executable set — the allowlist a workspace exposes.
#[derive(Default)]
pub struct Bin {
    execs: BTreeMap<String, Box<dyn Skillable>>,
}

impl Bin {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `exec` under its name (builder-style; last write wins).
    pub fn register(mut self, exec: impl Skillable + 'static) -> Self {
        self.execs.insert(exec.name().to_string(), Box::new(exec));
        self
    }

    /// The registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.execs.keys().map(String::as_str)
    }

    fn get(&self, name: &str) -> Option<&dyn Skillable> {
        self.execs.get(name).map(Box::as_ref)
    }

    /// Invoke the named executable. `Err(NotFound)` = unknown name (the allowlist
    /// boundary); a program failure is a non-zero `code`, not an `Err`.
    pub async fn invoke(
        &self,
        ws: &Workspace,
        name: &str,
        args: Vec<String>,
    ) -> Result<ExecOutput> {
        let exec = self.get(name).ok_or(CortexError::NotFound)?;
        Ok(match exec.exec(ws, args).await {
            Ok(out) => out,
            Err(e) => ExecOutput {
                code: 1,
                stdout: Vec::new(),
                stderr: e.to_string().into_bytes(),
            },
        })
    }

    /// Render the set's docs as a read-only [`SkillDir`] to mount into a workspace.
    pub fn as_dir(&self) -> SkillDir {
        SkillDir::render(self.execs.values().map(|e| &**e))
    }
}

/// A read-only [`Mountable`] doc tree — `AGENT.md` + `<name>/SKILL.md`, a
/// rendered snapshot. Every mutation errors [`Unsupported`](CortexError::Unsupported).
pub struct SkillDir {
    agent_md: String,
    skills: BTreeMap<String, String>,
}

impl SkillDir {
    /// Render docs from a [`Skillable`] set ([`Bin::as_dir`] is the usual entry).
    pub fn render<'a>(execs: impl IntoIterator<Item = &'a dyn Skillable>) -> Self {
        let mut skills = BTreeMap::new();
        let mut rows = String::new();
        for e in execs {
            // Links are relative to the mount root, where AGENT.md sits.
            rows.push_str(&format!(
                "| `{}` | {} | `{}/SKILL.md` |\n",
                e.name(),
                e.summary(),
                e.name()
            ));
            skills.insert(e.name().to_string(), e.skill());
        }
        let mut agent_md = String::new();
        agent_md.push_str("# Workspace Executables\n\n");
        agent_md.push_str("Run a predefined executable via your bash tool:\n\n");
        agent_md.push_str("```\nwsx <name> [args...]\n```\n\n");
        agent_md.push_str("It runs on the host against this workspace and returns its output.\n");
        agent_md.push_str("For a specific executable, read its skill first.\n\n");
        agent_md
            .push_str("## Available\n\n| name | summary | skill |\n|------|---------|-------|\n");
        agent_md.push_str(&rows);
        SkillDir { agent_md, skills }
    }

    /// Bytes of the file at `comps`, or `None` if it names no file.
    fn bytes(&self, comps: &[&str]) -> Option<&[u8]> {
        match comps {
            ["AGENT.md"] => Some(self.agent_md.as_bytes()),
            [name, "SKILL.md"] => self.skills.get(*name).map(|s| s.as_bytes()),
            _ => None,
        }
    }
}

/// A read-only handle over a rendered doc's bytes.
pub struct SkillFile {
    data: Vec<u8>,
}

#[async_trait]
impl FileExt for SkillFile {
    async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let off = offset as usize;
        if off >= self.data.len() {
            return Ok(0);
        }
        let n = (self.data.len() - off).min(buf.len());
        buf[..n].copy_from_slice(&self.data[off..off + n]);
        Ok(n)
    }

    async fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
        // `ReadOnlyFilesystem`, not `PermissionDenied`: `From<io::Error>` turns
        // exactly this kind back into `CortexError::ReadOnly`, so userspace hears
        // EROFS. EACCES would claim *this caller* lacks permission, when no caller
        // can write a rendered doc.
        Err(io::Error::from(io::ErrorKind::ReadOnlyFilesystem))
    }
}

#[async_trait]
impl FileHandle for SkillFile {
    async fn truncate(&self, _size: u64) -> Result<()> {
        Err(CortexError::ReadOnly)
    }
}

#[async_trait]
impl Mountable for SkillDir {
    type Handle = SkillFile;

    async fn stat(&self, path: &Path) -> Result<Stat> {
        let comps = comps(path)?;
        match comps
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [] => Ok(Stat::new(DirentKind::Dir, 0)),
            [name] if self.skills.contains_key(*name) => Ok(Stat::new(DirentKind::Dir, 0)),
            slice => self
                .bytes(slice)
                .map(|b| Stat::new(DirentKind::File, b.len() as u64))
                .ok_or(CortexError::NotFound),
        }
    }

    async fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let comps = comps(path)?;
        match comps
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [] => {
                let mut v = vec![Dirent::new("AGENT.md", DirentKind::File)];
                v.extend(
                    self.skills
                        .keys()
                        .map(|name| Dirent::new(name.clone(), DirentKind::Dir)),
                );
                Ok(v)
            }
            [name] if self.skills.contains_key(*name) => {
                Ok(vec![Dirent::new("SKILL.md", DirentKind::File)])
            }
            ["AGENT.md"] => Err(CortexError::NotADirectory),
            [name, "SKILL.md"] if self.skills.contains_key(*name) => {
                Err(CortexError::NotADirectory)
            }
            _ => Err(CortexError::NotFound),
        }
    }

    // Every write answers `ReadOnly`, not `Unsupported`. A rendered view of skills
    // is a filesystem that *will not* write, not one with no notion of the
    // operation — the distinction `S3Volume` draws for the same reason, and why
    // userspace gets EROFS (which `cp`, `rsync` and editors have a path for) rather
    // than ENOSYS.
    async fn mkdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    async fn unlink(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    async fn rmdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }

    async fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        options.validate()?;
        // Refused before the path is resolved, because nothing about the doc changes
        // the answer.
        if options.intends_write() {
            return Err(CortexError::ReadOnly);
        }
        let comps = comps(path)?;
        let slice: Vec<&str> = comps.iter().map(String::as_str).collect();
        match self.bytes(&slice) {
            Some(b) => {
                let stat = Stat::new(DirentKind::File, b.len() as u64);
                Ok((SkillFile { data: b.to_vec() }, stat))
            }
            None => match slice.as_slice() {
                [] => Err(CortexError::IsADirectory),
                [name] if self.skills.contains_key(*name) => Err(CortexError::IsADirectory),
                _ => Err(CortexError::NotFound),
            },
        }
    }
}

/// Normalize a mount-relative path into plain-name components.
fn comps(path: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                out.push(name.to_str().ok_or(CortexError::InvalidName)?.to_string())
            }
            Component::ParentDir | Component::Prefix(_) => return Err(CortexError::InvalidName),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial executable that ignores the workspace and echoes its args —
    /// enough to exercise dispatch and the tool path without touching a backend.
    struct Echo;
    #[async_trait]
    impl Executable for Echo {
        async fn exec(&self, _ws: &Workspace, args: Vec<String>) -> Result<ExecOutput> {
            Ok(ExecOutput::ok(args.join(" ").into_bytes()))
        }
    }
    impl Skillable for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn summary(&self) -> &str {
            "echo args"
        }
        fn skill(&self) -> String {
            "# echo\n`wsx echo <msg>`".into()
        }
    }
    impl Toolable for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo args"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": { "msg": { "type": "string" } } })
        }
        fn to_argv(&self, args: &Value) -> Vec<String> {
            args.get("msg")
                .and_then(Value::as_str)
                .map(|s| vec![s.to_string()])
                .unwrap_or_default()
        }
    }

    async fn names(m: &dyn Mountable<Handle = SkillFile>, path: &str) -> Vec<String> {
        let mut n: Vec<_> = m
            .list(Path::new(path))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        n.sort();
        n
    }

    #[tokio::test]
    async fn dispatch_and_unknown_name() {
        let ws = Workspace::new();
        let bin = Bin::new().register(Echo);
        assert_eq!(
            bin.invoke(&ws, "echo", vec!["hi".into()])
                .await
                .unwrap()
                .stdout,
            b"hi"
        );
        assert!(matches!(
            bin.invoke(&ws, "nope", vec![]).await,
            Err(CortexError::NotFound)
        ));
    }

    #[tokio::test]
    async fn skilldir_tree_and_read() {
        let docs = Bin::new().register(Echo).as_dir();
        assert_eq!(names(&docs, "").await, vec!["AGENT.md", "echo"]);
        assert_eq!(names(&docs, "echo").await, vec!["SKILL.md"]);

        let st = docs.stat(Path::new("echo/SKILL.md")).await.unwrap();
        assert_eq!(st.kind, DirentKind::File);
        let (h, opened) = docs
            .open(Path::new("echo/SKILL.md"), OpenOptions::read_only())
            .await
            .unwrap();
        assert_eq!(opened.size, st.size, "the open must agree with `stat`");
        let mut buf = vec![0u8; opened.size as usize];
        h.read_exact_at(&mut buf, 0).await.unwrap();
        assert_eq!(buf, b"# echo\n`wsx echo <msg>`");

        // The open carries its own metadata, so there is no second `stat` here.
        let (agent, opened) = docs
            .open(Path::new("AGENT.md"), OpenOptions::read_only())
            .await
            .unwrap();
        let mut a = vec![0u8; opened.size as usize];
        agent.read_exact_at(&mut a, 0).await.unwrap();
        assert!(String::from_utf8_lossy(&a).contains("| `echo` | echo args | `echo/SKILL.md` |"));
    }

    #[tokio::test]
    async fn skilldir_is_read_only_and_errors() {
        let docs = Bin::new().register(Echo).as_dir();

        // `ReadOnly`, not `Unsupported`: a rendered view of skills will not write,
        // rather than having no notion of writing — so userspace hears EROFS, which
        // it has a path for, instead of ENOSYS.
        for refused in [
            docs.open(Path::new("x"), OpenOptions::create_new())
                .await
                .map(|_| ()),
            docs.mkdir(Path::new("x")).await,
            docs.unlink(Path::new("x")).await,
            docs.rmdir(Path::new("x")).await,
        ] {
            assert!(
                matches!(&refused, Err(CortexError::ReadOnly)),
                "expected ReadOnly, got {refused:?}"
            );
        }

        assert!(matches!(
            docs.open(Path::new(""), OpenOptions::read_only()).await,
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            docs.open(Path::new("nope/SKILL.md"), OpenOptions::read_only())
                .await,
            Err(CortexError::NotFound)
        ));

        // The opened handle rejects writes, and as EROFS rather than EACCES: this is
        // the one kind `From<io::Error>` turns back into `ReadOnly`.
        let (h, _) = docs
            .open(Path::new("AGENT.md"), OpenOptions::read_only())
            .await
            .unwrap();
        let err = h.write_at(b"x", 0).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ReadOnlyFilesystem);
        assert!(matches!(CortexError::from(err), CortexError::ReadOnly));
        assert!(matches!(h.truncate(0).await, Err(CortexError::ReadOnly)));
    }

    #[tokio::test]
    async fn toolable_call_maps_args_and_result() {
        let ws = Workspace::new();
        let tool: &dyn Toolable = &Echo;
        assert_eq!(tool.to_argv(&json!({ "msg": "hi" })), ["hi"]);
        assert_eq!(
            tool.call(&ws, &json!({ "msg": "hi" })).await.unwrap(),
            json!("hi")
        );
    }
}
