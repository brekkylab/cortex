//! Who may read what, decided in the tree and not in the agent.
//!
//! [`AclFs`] wraps any [`FileSystem`] and answers for one actor — a department, here. A read of
//! a path the actor may not see is [`PermissionDenied`](io::ErrorKind::PermissionDenied) at the
//! `read_at` the tool would have made, so the model never receives the bytes; there is no prompt
//! to talk it out of a rule the filesystem enforces. A folder the actor may not read is not
//! listed either: its name is visible where it sits, what it holds is not, which is how a shared
//! drive already behaves.
//!
//! The rules are `정책/acl.json` in the workspace: a longest-prefix match over root-relative
//! paths. [`Acl::derive`] is the write side of the same rule — a file the agent writes inherits
//! the *narrowest* readership among the files it cites, which is what the policy document says
//! in prose ("가장 좁은 열람 권한을 그대로 따른다"). That inherited readership is written beside
//! the file as `<name>.acl.json`, and a read under a writable prefix consults the sidecar first:
//! what the prefix rule opens to everyone, the sidecar can close again.

use std::{
    collections::BTreeSet,
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use cortex::{
    BoxFuture,
    fs::{Dirent, FileSystem, Stat},
};
use serde::Deserialize;

/// What `<name>.acl.json` carries that this layer reads; the rest of the sidecar is provenance.
#[derive(Deserialize)]
struct Sidecar {
    readers: Vec<String>,
}

/// The sidecar's name for a written file.
pub fn sidecar_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.acl.json", path.display()))
}

/// Read a whole file through the trait — `stat` for the size, `read_at` until a short return.
pub async fn read_whole(fs: &dyn FileSystem, path: &Path) -> io::Result<Vec<u8>> {
    let st = fs.stat(path).await?;
    let mut out = Vec::with_capacity(st.size as usize);
    let mut buf = vec![0u8; 64 * 1024];
    let mut off = 0u64;
    loop {
        let n = fs.read_at(path, &mut buf, off).await?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
        if n < buf.len() {
            break;
        }
    }
    Ok(out)
}

/// One line of the policy: everything under `prefix` is read by `readers`.
#[derive(Clone, Debug, Deserialize)]
pub struct Rule {
    pub prefix: String,
    pub label: String,
    pub readers: Vec<String>,
    #[serde(default)]
    pub writable: bool,
}

/// The actor every rule opens to: the tree as its administrator sees it, with every file and
/// who may read each. Never an actor the agent runs as.
pub const ADMIN: &str = "관리자";

impl Rule {
    fn allows(&self, actor: &str) -> bool {
        actor == ADMIN || self.readers.iter().any(|r| r == "*" || r == actor)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Acl {
    rules: Vec<Rule>,
}

/// What the tree says about one path for one actor.
#[derive(Clone, Debug)]
pub struct Verdict {
    pub readable: bool,
    pub writable: bool,
    pub label: String,
    pub readers: Vec<String>,
}

impl Acl {
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(text)?)
    }

    /// Longest matching prefix, or a closed default: a path no rule names is nobody's.
    pub fn rule_for(&self, path: &Path) -> Option<&Rule> {
        let key = normalize(path);
        self.rules
            .iter()
            .filter(|r| {
                let p = Path::new(&r.prefix);
                key == p || key.starts_with(p)
            })
            .max_by_key(|r| r.prefix.len())
    }

    pub fn verdict(&self, actor: &str, path: &Path) -> Verdict {
        match self.rule_for(path) {
            Some(rule) => Verdict {
                readable: rule.allows(actor),
                writable: rule.writable && rule.allows(actor),
                label: rule.label.clone(),
                readers: rule.readers.clone(),
            },
            None => Verdict {
                readable: false,
                writable: false,
                label: "규칙 없음".into(),
                readers: Vec::new(),
            },
        }
    }

    /// The readership an output inherits from the files it cites: the intersection of their
    /// readers. `*` stands for everyone and so never narrows; a citation with no rule narrows to
    /// nobody, which is the safe answer for a file the policy forgot.
    pub fn derive(&self, sources: &[PathBuf]) -> BTreeSet<String> {
        let mut acc: Option<BTreeSet<String>> = None;
        for src in sources {
            let readers: BTreeSet<String> = match self.rule_for(src) {
                Some(r) => r.readers.iter().cloned().collect(),
                None => BTreeSet::new(),
            };
            if readers.contains("*") {
                continue;
            }
            acc = Some(match acc {
                None => readers,
                Some(prev) => prev.intersection(&readers).cloned().collect(),
            });
        }
        acc.unwrap_or_else(|| BTreeSet::from(["*".to_string()]))
    }
}

/// `..` and `.` folded, leading root dropped — the same key the mount table routes on, so a rule
/// and a request spell a path the same way.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// A tree seen as one actor.
pub struct AclFs<F: FileSystem> {
    inner: F,
    acl: Arc<Acl>,
    actor: String,
}

impl<F: FileSystem> AclFs<F> {
    pub fn new(inner: F, acl: Arc<Acl>, actor: impl Into<String>) -> Self {
        Self {
            inner,
            acl,
            actor: actor.into(),
        }
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn acl(&self) -> &Acl {
        &self.acl
    }

    pub fn verdict(&self, path: &Path) -> Verdict {
        self.acl.verdict(&self.actor, path)
    }

    fn deny(&self, path: &Path) -> io::Error {
        let v = self.verdict(path);
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{}: {}은(는) {} 열람 — {}에는 닫혀 있음",
                path.display(),
                v.label,
                readers_ko(&v.readers),
                self.actor
            ),
        )
    }

    fn deny_write(&self, path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{}: 쓰기는 산출물/ 아래에서만 — 원본은 Source of Truth 로 남는다",
                path.display()
            ),
        )
    }

    /// The readership a written file carries in its sidecar, if it has one. `None` for a file
    /// outside every writable prefix, for the sidecars themselves, and for a file written
    /// without one.
    async fn inherited(&self, path: &Path) -> Option<Vec<String>> {
        let rule = self.acl.rule_for(path)?;
        if !rule.writable
            || path.extension().is_some_and(|e| e == "json")
                && path.to_string_lossy().ends_with(".acl.json")
        {
            return None;
        }
        let bytes = read_whole(&self.inner, &sidecar_of(path)).await.ok()?;
        let side: Sidecar = serde_json::from_slice(&bytes).ok()?;
        Some(side.readers)
    }

    async fn check_read(&self, path: &Path) -> io::Result<()> {
        if !self.verdict(path).readable {
            return Err(self.deny(path));
        }
        if self.actor != ADMIN
            && let Some(readers) = self.inherited(path).await
            && !readers.iter().any(|r| r == "*" || *r == self.actor)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{}: 인용 자료의 권한에 따라 {} 열람 — {}에는 닫혀 있음",
                    path.display(),
                    readers_ko(&readers),
                    self.actor
                ),
            ));
        }
        Ok(())
    }

    /// [`check_read`](Self::check_read) as a verdict, for callers that annotate rather than
    /// refuse.
    pub async fn may_read(&self, path: &Path) -> bool {
        self.check_read(path).await.is_ok()
    }

    fn check_write(&self, path: &Path) -> io::Result<()> {
        if self.verdict(path).writable {
            Ok(())
        } else {
            Err(self.deny_write(path))
        }
    }
}

pub fn readers_ko(readers: &[String]) -> String {
    if readers.iter().any(|r| r == "*") {
        "전 부서".to_string()
    } else {
        readers.join("·")
    }
}

impl<F: FileSystem> FileSystem for AclFs<F> {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        self.inner.stat(path)
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            // The root is everyone's: it is where the folders one may not open are seen to exist.
            if !normalize(path).as_os_str().is_empty() && !self.verdict(path).readable {
                return Err(self.deny(path));
            }
            self.inner.list(path).await
        })
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            self.check_read(path).await?;
            self.inner.read_at(path, buf, offset).await
        })
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            self.check_write(path)?;
            self.inner.create(path).await
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            self.check_write(path)?;
            self.inner.mkdir(path).await
        })
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.check_write(path)?;
            self.inner.unlink(path).await
        })
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.check_write(path)?;
            self.inner.rmdir(path).await
        })
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            self.check_write(path)?;
            self.inner.write_at(path, buf, offset).await
        })
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.check_write(path)?;
            self.inner.truncate(path, size).await
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.check_write(from)?;
            self.check_write(to)?;
            self.inner.rename(from, to).await
        })
    }

    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        self.inner.flush(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = include_str!("../examples/procurement/정책/acl.json");

    #[test]
    fn longest_prefix_wins() {
        let acl = Acl::from_json(POLICY).unwrap();
        assert_eq!(
            acl.rule_for(Path::new("구매팀/협력사평가/x.csv"))
                .unwrap()
                .label,
            "협력사 신용 정보"
        );
        assert_eq!(
            acl.rule_for(Path::new("구매팀/구매규정-v7.md"))
                .unwrap()
                .label,
            "구매 규정"
        );
    }

    #[test]
    fn derived_readership_is_the_narrowest() {
        let acl = Acl::from_json(POLICY).unwrap();
        let cited = [
            PathBuf::from("회의록/a.md"),
            PathBuf::from("구매팀/협력사평가/b.csv"),
            PathBuf::from("재무팀/c.csv"),
        ];
        let got: Vec<_> = acl.derive(&cited).into_iter().collect();
        assert_eq!(got, vec!["재무팀".to_string()]);
        let public = [PathBuf::from("회의록/a.md")];
        assert_eq!(
            acl.derive(&public).into_iter().collect::<Vec<_>>(),
            vec!["*".to_string()]
        );
    }

    #[tokio::test]
    async fn a_written_file_is_closed_by_its_sidecar() {
        use cortex::fs::{InMemFs, WorkFs};
        let acl = Arc::new(Acl::from_json(POLICY).unwrap());
        let mut work = WorkFs::new();
        work.mount("산출물", InMemFs::new()).unwrap();
        let fs = Arc::new(work);

        // Finance writes a report and its sidecar naming finance as the only reader.
        let finance = AclFs::new(fs.clone(), acl.clone(), "재무팀");
        let report = Path::new("산출물/r.md");
        finance.create(report).await.unwrap();
        finance.write_at(report, b"secret", 0).await.unwrap();
        finance.create(&sidecar_of(report)).await.unwrap();
        finance
            .write_at(
                &sidecar_of(report),
                r#"{"readers":["재무팀"]}"#.as_bytes(),
                0,
            )
            .await
            .unwrap();

        let mut buf = [0u8; 16];
        assert!(finance.read_at(report, &mut buf, 0).await.is_ok());

        // The prefix rule opens 산출물 to everyone; the sidecar closes this file to HR.
        let hr = AclFs::new(fs.clone(), acl.clone(), "인사팀");
        let err = hr.read_at(report, &mut buf, 0).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!hr.may_read(report).await);
        // …and a file written without a sidecar stays open, as the prefix rule says.
        let plain = Path::new("산출물/p.md");
        finance.create(plain).await.unwrap();
        assert!(hr.read_at(plain, &mut buf, 0).await.is_ok());
    }

    #[test]
    fn hr_may_not_read_credit_data_and_the_admin_may() {
        let acl = Acl::from_json(POLICY).unwrap();
        assert!(
            !acl.verdict("인사팀", Path::new("구매팀/협력사평가/b.csv"))
                .readable
        );
        assert!(
            acl.verdict("재무팀", Path::new("구매팀/협력사평가/b.csv"))
                .readable
        );
        assert!(acl.verdict(ADMIN, Path::new("재무팀/x.csv")).readable);
    }

    #[tokio::test]
    async fn a_closed_folder_lists_nothing_but_the_root_lists_it() {
        use cortex::fs::{InMemFs, WorkFs};
        let acl = Arc::new(Acl::from_json(POLICY).unwrap());
        let mut work = WorkFs::new();
        work.mount("재무팀", InMemFs::new()).unwrap();
        let fs = Arc::new(work);
        fs.create(Path::new("재무팀/여신.csv")).await.unwrap();

        let hr = AclFs::new(fs.clone(), acl.clone(), "인사팀");
        let root: Vec<String> = hr
            .list(Path::new(""))
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(root, vec!["재무팀".to_string()]);
        assert_eq!(
            hr.list(Path::new("재무팀"))
                .await
                .map(drop)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let finance = AclFs::new(fs.clone(), acl.clone(), "재무팀");
        assert_eq!(finance.list(Path::new("재무팀")).await.unwrap().len(), 1);
        assert_eq!(
            AclFs::new(fs, acl, ADMIN)
                .list(Path::new("재무팀"))
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
