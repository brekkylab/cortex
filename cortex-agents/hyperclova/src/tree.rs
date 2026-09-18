//! The tree the agent sees: one root, several sources, none of them copied.
//!
//! Each top-level directory of the workspace is a mount. Most are local folders served through
//! [`PassthroughFs`]; one may instead come from an S3-compatible bucket through [`S3Fs`], which
//! is how Ncloud Object Storage — S3 API, different endpoint — sits next to a file server under
//! the same root. The agent's tools address `재무팀/협력사-여신한도-2026.csv` either way; where
//! the bytes live is the mount table's business, not the model's.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use cortex::fs::{PassthroughFs, S3Config, S3Fs, WorkFs};

/// A bucket to stand in for one department's folder.
#[derive(Clone, Debug)]
pub struct S3Source {
    pub bucket: String,
    pub prefix: Option<String>,
    pub region: String,
    pub endpoint: Option<String>,
}

impl S3Source {
    /// `bucket[/prefix]`, as typed on the command line.
    pub fn parse(spec: &str, region: String, endpoint: Option<String>) -> Self {
        let (bucket, prefix) = match spec.split_once('/') {
            Some((b, p)) if !p.is_empty() => {
                (b.to_string(), Some(p.trim_end_matches('/').to_string()))
            }
            Some((b, _)) => (b.to_string(), None),
            None => (spec.to_string(), None),
        };
        Self {
            bucket,
            prefix,
            region,
            endpoint,
        }
    }

    pub fn describe(&self) -> String {
        match &self.prefix {
            Some(p) => format!("Naver Cloud Storage · {}/{}", self.bucket, p),
            None => format!("Naver Cloud Storage · {}", self.bucket),
        }
    }

    pub fn open(&self) -> anyhow::Result<S3Fs> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").context(
            "AWS_ACCESS_KEY_ID is not set; `eval $(aws configure export-credentials --format env)`",
        )?;
        let secret_access_key =
            std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY is not set")?;
        let cfg = S3Config {
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            access_key_id,
            secret_access_key,
            endpoint: self.endpoint.clone(),
            key_prefix: self.prefix.clone(),
        };
        Ok(S3Fs::new(&cfg)?)
    }
}

/// One line per mount: where each name under the root comes from.
pub struct Mounted {
    pub name: String,
    pub source: String,
}

/// Assemble the workspace. `s3_for` names the top-level directory whose bytes come from the
/// bucket instead of the local folder of the same name.
pub fn build(
    workspace: &Path,
    s3: Option<(&str, &S3Source)>,
) -> anyhow::Result<(WorkFs, Vec<Mounted>)> {
    let mut names: Vec<String> = fs::read_dir(workspace)
        .with_context(|| format!("reading {}", workspace.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| !n.starts_with('.'))
        .collect();
    if let Some((name, _)) = s3
        && !names.iter().any(|n| n == name)
    {
        names.push(name.to_string());
    }
    names.sort();

    let mut workfs = WorkFs::new();
    let mut mounted = Vec::new();
    for name in names {
        match s3 {
            Some((at, src)) if at == name => {
                workfs.mount(&name, src.open()?)?;
                mounted.push(Mounted {
                    name,
                    source: src.describe(),
                });
            }
            _ => {
                let host = workspace.join(&name);
                workfs.mount(&name, PassthroughFs::new(&host))?;
                mounted.push(Mounted {
                    name,
                    source: format!("{}  (로컬 폴더)", host.display()),
                });
            }
        }
    }
    Ok((workfs, mounted))
}

/// The host directory behind `산출물/` — where a report lands so it can be opened after the run.
pub fn output_dir(workspace: &Path) -> io::Result<PathBuf> {
    let dir = workspace.join("산출물");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}
