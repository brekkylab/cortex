//! What the window starts with: the same tree the HyperCLOVA tab runs over, mounted into the
//! workspace tab so a report the agent writes can be opened there a moment later.

use std::{path::Path, sync::Arc};

use cortex::fs::PassthroughFs;

use crate::{
    error::Result,
    hyperclova::Hcx,
    state::{MountInfo, MountKind, Workspace},
};

pub async fn mount_workspace(hcx: &Hcx, workspace: &Workspace) -> Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(&hcx.workspace)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    for name in names {
        let path = format!("/{name}");
        let host = hcx.workspace.join(&name);
        let (kind, detail, store): (MountKind, String, Arc<dyn cortex::fs::FileSystem>) =
            match &hcx.s3 {
                Some((at, src)) if *at == name => match src_store(src) {
                    Ok(store) => (MountKind::S3, src.describe(), Arc::new(store)),
                    Err(_) => (
                        MountKind::Local,
                        host.display().to_string(),
                        Arc::new(PassthroughFs::new(&host)),
                    ),
                },
                _ => (
                    MountKind::Local,
                    host.display().to_string(),
                    Arc::new(PassthroughFs::new(&host)),
                ),
            };
        let info = MountInfo {
            path: path.clone(),
            kind,
            label: name.clone(),
            detail,
            writable: name == "산출물",
        };
        workspace.remember_mount(info).await?;
        workspace.fs.write().await.mount(Path::new(&path), store)?;
    }
    Ok(())
}

fn src_store(src: &cortex_agent_hyperclova::tree::S3Source) -> anyhow::Result<cortex::fs::S3Fs> {
    src.open()
}
