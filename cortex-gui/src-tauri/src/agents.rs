//! Memories, docsets, and the agents that are given them.
//!
//! # A resource is a store file in the workspace
//!
//! Registering a memory is `mem init` against a path under [`ResourceKind::dir`], and listing
//! them is a directory read. Nothing is cached and nothing is written beside the store:
//! [`resources`] is a listing every time it is called, so the answer is the tree's rather than a
//! copy of what the tree said when something last wrote to it.
//!
//! That the store *is* the record is what keeps this simple. There is no state between "nothing
//! here" and "a store", so there is no half-registered resource to describe, nothing to retry,
//! and nothing that can disagree with anything else. [`ResourceKind`] argues the shape; what
//! matters here is the consequence: an `init` that fails leaves the workspace exactly as it was,
//! because `storebase`'s `create` is an exclusive create that removes the file if the schema
//! does not go in.
//!
//! Creating one needs the workspace mounted, because SQLite opens files rather than paths in a
//! `WorkFs`. That, and why the mount lasts only as long as the command, is [`crate::store`].
//!
//! # An agent is not
//!
//! Agents stay in memory, and deliberately: an agent is a thing that *runs*, ailoy is what would
//! run one, and where its definition belongs is a question that runtime gets to answer. Writing
//! them into the tree now would be choosing a format on no evidence — the opposite of the stores
//! above, whose format is already decided by the programs that open them.

use std::path::Path;

use cortex::fs::{DirentKind, FileSystem};
use serde::Deserialize;
use tauri::State;

use crate::{
    error::{Error, Result},
    fsops::mkdir_p,
    state::{Agent, AgentSpec, Resource, ResourceKind, Workspace, epoch_ms, now_ms},
    store,
};

/// What a store file is called. Also what a listing looks for, so a `-journal` a crashed write
/// left behind is not mistaken for a resource.
const STORE_EXT: &str = ".sqlite";

/// What the "memory 추가" / "docset 추가" forms collect.
#[derive(Deserialize)]
pub struct ResourceForm {
    pub kind: ResourceKind,
    pub name: String,
}

/// Every memory and docset in the workspace.
#[tauri::command]
pub async fn resources(state: State<'_, Workspace>) -> Result<Vec<Resource>> {
    let fs = state.fs.read().await;
    read_resources(&*fs).await
}

/// Register a memory or a docset, which is to say: make its store.
#[tauri::command]
pub async fn add_resource(state: State<'_, Workspace>, form: ResourceForm) -> Result<Resource> {
    let name = store_name(&form.name)?;
    let dir = form.kind.dir();
    let path = format!("{dir}/{name}{STORE_EXT}");

    {
        let fs = state.fs.read().await;
        // The directory `init` will create the file in. A filesystem's `create` resolves only
        // the parent, so nothing else is going to make it.
        mkdir_p(&*fs, Path::new(dir)).await?;
        // Cheap, and not the authority. `storebase`'s `create` is an exclusive `create_new`, so
        // it is what actually decides a race — this only saves mounting the workspace to find
        // out something a `stat` already knew.
        if fs.stat(Path::new(&path)).await.is_ok() {
            return Err(Error::msg(format!("{path} 가 이미 있습니다")));
        }
    }

    // One at a time: there is one mount point and a mount point has to be empty.
    let _one_at_a_time = state.store_init.lock().await;
    store::init_store(state.fs.clone(), form.kind, &path).await?;

    let fs = state.fs.read().await;
    resource_at(&*fs, form.kind, name, path).await
}

/// Delete a resource, which is to say: delete its store.
///
/// Every agent holding it loses the reference too. An agent naming a store that is not in the
/// tree is a record that cannot be run, and nothing in the window could tell that apart from one
/// that can.
#[tauri::command]
pub async fn remove_resource(state: State<'_, Workspace>, id: String) -> Result<()> {
    // `id` is a path, and it arrives from the window. Confining it to the directories resources
    // live in is what keeps this command from being a delete-anything: the window would never
    // send anything else, but the window is not the only thing that can call it.
    if kind_of(&id).is_none() {
        return Err(Error::msg(format!("{id} 는 리소스가 아닙니다")));
    }

    {
        let fs = state.fs.read().await;
        fs.unlink(Path::new(&id)).await?;
    }
    for agent in state.agents.write().await.iter_mut() {
        agent.resource_ids.retain(|held| held != &id);
    }
    Ok(())
}

/// Every agent registered in this session.
#[tauri::command]
pub async fn agents(state: State<'_, Workspace>) -> Result<Vec<Agent>> {
    Ok(state.agents.read().await.clone())
}

/// Register an agent.
#[tauri::command]
pub async fn create_agent(state: State<'_, Workspace>, spec: AgentSpec) -> Result<Agent> {
    let name = spec.name.trim().to_string();
    if name.is_empty() {
        return Err(Error::msg("에이전트 이름을 입력해 주세요"));
    }
    if spec.system_message.trim().is_empty() {
        return Err(Error::msg("시스템 메시지를 입력해 주세요"));
    }

    // Every id must still name a store in the tree. The form only sends back ids it was given,
    // so a mismatch means the workspace moved underneath it — a resource deleted between the
    // form opening and the save — and keeping the id would register an agent nobody can run.
    let fs = state.fs.read().await;
    for id in &spec.resource_ids {
        if fs.stat(Path::new(id)).await.is_err() {
            return Err(Error::msg(format!(
                "{id} 를 워크스페이스에서 찾을 수 없습니다 — 목록을 새로 고쳐 주세요"
            )));
        }
    }
    drop(fs);

    let agent = Agent {
        id: state.next_id("agent"),
        name,
        model: spec.model.trim().to_string(),
        system_message: spec.system_message,
        resource_ids: spec.resource_ids,
        paths: spec.paths,
        created_ms: now_ms(),
    };
    state.agents.write().await.push(agent.clone());
    Ok(agent)
}

/// Forget an agent.
#[tauri::command]
pub async fn delete_agent(state: State<'_, Workspace>, id: String) -> Result<()> {
    state.agents.write().await.retain(|a| a.id != id);
    Ok(())
}

/// Both resource directories, read back from the tree.
///
/// A directory that is not there yet is not an error — it is a workspace nobody has registered
/// that kind in — and neither is a file under it that is not a store: only `*.sqlite` counts, so
/// the `-journal` a killed write leaves behind is passed over rather than listed as a resource.
async fn read_resources(fs: &dyn FileSystem) -> Result<Vec<Resource>> {
    let mut out = Vec::new();
    for kind in [ResourceKind::Memory, ResourceKind::Docset] {
        let dir = kind.dir();
        let entries = match fs.list(Path::new(dir)).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            if entry.kind != DirentKind::File {
                continue;
            }
            let Some(name) = entry.name.strip_suffix(STORE_EXT) else {
                continue;
            };
            let path = format!("{dir}/{}", entry.name);
            out.push(resource_at(fs, kind, name.to_string(), path).await?);
        }
    }
    out.sort_by_key(|resource| resource.created_ms);
    Ok(out)
}

/// One resource, with the timestamp the list is ordered by.
///
/// A `stat` per entry, unlike the file tree — which passes a missing size through rather than
/// pay for it. Two directories of stores is a listing small enough for that not to matter, and
/// the alternative is an unordered list: `Dirent` carries metadata only when it was free, and
/// for a local directory it never is.
async fn resource_at(
    fs: &dyn FileSystem,
    kind: ResourceKind,
    name: String,
    path: String,
) -> Result<Resource> {
    let stat = fs.stat(Path::new(&path)).await?;
    Ok(Resource {
        created_ms: stat
            .created
            .or(stat.mtime)
            .and_then(epoch_ms)
            .unwrap_or_default(),
        id: path,
        kind,
        name,
    })
}

/// `name` as the file a store will be, or a refusal.
///
/// Nothing is slugified: the file's name is the name the window shows, so mangling it here would
/// mean showing back something nobody typed. What is checked is only what would make it not a
/// name — anything that could point somewhere else, and the leading dot that would hide it.
///
/// A trailing `.sqlite` is taken off rather than refused. Somebody who typed the extension meant
/// the store, and `notes.sqlite.sqlite` is nobody's intention.
fn store_name(name: &str) -> Result<String> {
    let name = name.trim();
    let name = name.strip_suffix(STORE_EXT).unwrap_or(name).trim();

    let refusal = if name.is_empty() {
        Some("이름을 입력해 주세요")
    } else if name.contains('/') {
        Some("이름에 / 는 쓸 수 없습니다")
    } else if name.starts_with('.') {
        Some("이름은 . 으로 시작할 수 없습니다")
    } else if name.chars().any(char::is_control) {
        Some("이름에 제어 문자는 쓸 수 없습니다")
    } else {
        None
    };
    match refusal {
        Some(why) => Err(Error::msg(why)),
        None => Ok(name.to_string()),
    }
}

/// The kind a path is a resource of, or `None` for a path that is not one.
fn kind_of(path: &str) -> Option<ResourceKind> {
    if !path.ends_with(STORE_EXT) {
        return None;
    }
    [ResourceKind::Memory, ResourceKind::Docset]
        .into_iter()
        .find(|kind| {
            // The store is directly in the kind's directory, not somewhere below it.
            path.strip_prefix(kind.dir())
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|name| !name.contains('/'))
        })
}

#[cfg(test)]
mod tests {
    use cortex::fs::{InMemFs, WorkFs};

    use super::*;

    /// The tree a window starts with: a writable root and nothing in it.
    fn workspace() -> WorkFs {
        WorkFs::new()
            .try_with_mount("", InMemFs::new())
            .expect("the root is a valid mount path")
    }

    /// A store, without `mem` — the schema inside it is not what a listing reads.
    async fn plant(fs: &WorkFs, path: &str) {
        mkdir_p(fs, Path::new(path).parent().unwrap())
            .await
            .unwrap();
        fs.create(Path::new(path)).await.unwrap();
    }

    #[tokio::test]
    async fn a_listing_is_the_directory() {
        let fs = workspace();
        plant(&fs, "/.cortex/memory/User 선호.sqlite").await;
        plant(&fs, "/.cortex/docset/product docs.sqlite").await;

        let listed = read_resources(&fs).await.unwrap();
        assert_eq!(listed.len(), 2);
        // The name is the file's, exactly as it was typed — no slug in between.
        let names: Vec<_> = listed.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"User 선호"));
        assert!(names.contains(&"product docs"));
        // And which directory it is in is what says which kind it is.
        let memory = listed.iter().find(|r| r.name == "User 선호").unwrap();
        assert_eq!(memory.kind, ResourceKind::Memory);
        assert_eq!(memory.id, "/.cortex/memory/User 선호.sqlite");
    }

    #[tokio::test]
    async fn only_stores_are_listed() {
        let fs = workspace();
        plant(&fs, "/.cortex/memory/notes.sqlite").await;
        // What a killed write leaves behind, and what a store is not.
        plant(&fs, "/.cortex/memory/notes.sqlite-journal").await;
        plant(&fs, "/.cortex/memory/README.md").await;

        let listed = read_resources(&fs).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "notes");
    }

    #[tokio::test]
    async fn an_empty_workspace_has_no_resources() {
        // Neither directory exists, which is a workspace nobody has registered one in — not an
        // error, and not something the window should have to special-case.
        assert!(read_resources(&workspace()).await.unwrap().is_empty());
    }

    #[test]
    fn a_name_is_one_file_name() {
        assert_eq!(store_name("  notes  ").unwrap(), "notes");
        // Typed with the extension: taken off rather than refused.
        assert_eq!(store_name("notes.sqlite").unwrap(), "notes");
        // Case and spaces survive, because the file's name is what the window shows.
        assert_eq!(store_name("Team Notes").unwrap(), "Team Notes");

        assert!(store_name("   ").is_err());
        assert!(store_name("../escape").is_err());
        assert!(store_name("a/b").is_err());
        assert!(store_name(".hidden").is_err());
    }

    #[test]
    fn only_a_store_in_a_resource_directory_is_a_resource() {
        assert_eq!(
            kind_of("/.cortex/memory/notes.sqlite"),
            Some(ResourceKind::Memory)
        );
        assert_eq!(
            kind_of("/.cortex/docset/docs.sqlite"),
            Some(ResourceKind::Docset)
        );
        assert_eq!(kind_of("/secret.sqlite"), None);
        assert_eq!(kind_of("/.cortex/memory/notes.md"), None);
        // Not somewhere below the directory, either.
        assert_eq!(kind_of("/.cortex/memory/deep/notes.sqlite"), None);
    }

    /// What `add_resource` does: `mem init` through a mounted workspace, and a listing that
    /// finds the store it wrote.
    ///
    /// `#[ignore]` because it mounts, so it needs FUSE-T, the workspace's executables, and
    /// `--test-threads=1`; [`crate::store`]'s own test has the invocation.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn registering_makes_the_store() {
        use std::sync::Arc;

        use tokio::sync::RwLock;

        let fs = Arc::new(RwLock::new(workspace()));
        let name = store_name("Team Notes").unwrap();
        let path = format!("{}/{name}{STORE_EXT}", ResourceKind::Memory.dir());
        {
            let guard = fs.read().await;
            mkdir_p(&*guard, Path::new(ResourceKind::Memory.dir()))
                .await
                .unwrap();
        }

        store::init_store(fs.clone(), ResourceKind::Memory, &path)
            .await
            .expect("mem init through a mounted workspace");

        let guard = fs.read().await;
        let listed = read_resources(&*guard).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Team Notes");
        assert!(
            guard.stat(Path::new(&path)).await.unwrap().size > 0,
            "an empty file is a `create`, not an `init`"
        );
    }
}
