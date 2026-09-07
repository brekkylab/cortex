//! Memories, docsets, and the agents that are given them.
//!
//! # A resource is a file in the workspace
//!
//! Registering a memory or a docset writes `<slug>.json` under [`ResourceKind::dir`], and
//! listing them reads that directory back. Nothing is cached: [`resources`] is a listing every
//! time it is called, so the answer is the tree's rather than a copy of what the tree said when
//! something last wrote to it.
//!
//! That is the whole point of putting them there. A registry held beside the workspace would be
//! a second thing to save, a second thing to load, and a second thing to keep in step — and it
//! would be invisible to everything else that can reach a `WorkFs`, which is a mounted tree, a
//! guest, and the `mem`/`index` programs themselves. A directory of manifests is visible to all
//! of them and needs no protocol.
//!
//! What is *not* there yet is the store: `<slug>.sqlite`, which `cortex-execs/mem` and
//! `cortex-execs/index` write and nothing here does. [`Resource::backed`] reports whether it
//! exists, read from the tree like everything else — so a store that arrives by any route shows
//! up as backed without a line changing here.
//!
//! # An agent is not
//!
//! Agents stay in memory, and deliberately: an agent is a thing that *runs*, ailoy is what would
//! run one, and where its definition belongs is a question that runtime gets to answer. Writing
//! them into the tree now would be choosing a format on no evidence — the opposite of the
//! resources above, whose format is already decided by the programs that will open them.

use std::path::Path;

use cortex::fs::{DirentKind, FileSystem};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{
    error::{Error, Result},
    fsops::{mkdir_p, read_all, write_file},
    state::{Agent, AgentSpec, Resource, ResourceKind, Workspace, now_ms, slug},
};

/// The extension a manifest is filed under, and the one a listing looks for.
const MANIFEST_EXT: &str = ".json";

/// The extension of the store that will sit beside a manifest.
const STORE_EXT: &str = ".sqlite";

/// A manifest is small by construction; a file under this name that is not is not one of ours.
const MANIFEST_CAP: u64 = 64 << 10;

/// What a manifest holds. The name as it was typed, because the filename is the slug and a slug
/// does not round-trip.
#[derive(Serialize, Deserialize)]
struct Manifest {
    kind: ResourceKind,
    name: String,
    #[serde(default)]
    note: String,
    created_ms: u64,
}

/// What the "memory 추가" / "docset 추가" forms collect.
#[derive(Deserialize)]
pub struct ResourceForm {
    pub kind: ResourceKind,
    pub name: String,
    #[serde(default)]
    pub note: String,
}

/// Every memory and docset in the workspace.
#[tauri::command]
pub async fn resources(state: State<'_, Workspace>) -> Result<Vec<Resource>> {
    let fs = state.fs.read().await;
    read_resources(&*fs).await
}

/// Register a memory or a docset: create its directory, and write its manifest.
#[tauri::command]
pub async fn add_resource(state: State<'_, Workspace>, form: ResourceForm) -> Result<Resource> {
    let fs = state.fs.read().await;
    add(&*fs, form).await
}

/// [`add_resource`] against a tree rather than against the window's state.
async fn add(fs: &dyn FileSystem, form: ResourceForm) -> Result<Resource> {
    let name = form.name.trim().to_string();
    let slug = slug(&name);
    if slug.is_empty() {
        return Err(Error::msg("이름에 글자나 숫자가 하나는 있어야 합니다"));
    }

    let dir = form.kind.dir();
    let path = format!("{dir}/{slug}{MANIFEST_EXT}");
    let manifest = Manifest {
        kind: form.kind,
        name: name.clone(),
        note: form.note.trim().to_string(),
        created_ms: now_ms(),
    };

    // Exclusive, so two names that slug the same collide here rather than one silently replacing
    // the other. `write_file` overwrites by design — an editor saving is exactly that — so the
    // refusal has to be a `create` of its own.
    match fs.create(Path::new(&path)).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            mkdir_p(fs, Path::new(dir)).await?;
            fs.create(Path::new(&path)).await?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::msg(format!("{path} 가 이미 있습니다")));
        }
        Err(err) => return Err(err.into()),
    }

    let body = serde_json::to_vec_pretty(&manifest).map_err(|err| Error::msg(err.to_string()))?;
    if let Err(err) = write_file(fs, Path::new(&path), &body).await {
        // The name was made a moment ago and the write is what gives it meaning, so an empty
        // file left behind would list as a resource with no manifest to read.
        let _ = fs.unlink(Path::new(&path)).await;
        return Err(err);
    }
    Ok(resource_of(fs, path, manifest).await)
}

/// Delete a resource's manifest, and the store beside it if one was ever written.
///
/// Every agent holding it loses the reference too: an agent naming a resource that is not in the
/// tree is a record that cannot be run, and nothing in the window could tell that apart from a
/// resource that is merely unbacked — which all of them are today.
#[tauri::command]
pub async fn remove_resource(state: State<'_, Workspace>, id: String) -> Result<()> {
    {
        let fs = state.fs.read().await;
        remove(&*fs, &id).await?;
    }
    for agent in state.agents.write().await.iter_mut() {
        agent.resource_ids.retain(|held| held != &id);
    }
    Ok(())
}

/// [`remove_resource`] against a tree rather than against the window's state.
async fn remove(fs: &dyn FileSystem, id: &str) -> Result<()> {
    // `id` is a path, and it arrives from the window. Confining it to the directories resources
    // live in is what keeps this command from being a delete-anything: the window would never
    // send anything else, but the window is not the only thing that can call it.
    let inside = [ResourceKind::Memory, ResourceKind::Docset]
        .iter()
        .any(|kind| id.starts_with(&format!("{}/", kind.dir())));
    if !inside || !id.ends_with(MANIFEST_EXT) {
        return Err(Error::msg(format!("{id} 는 리소스가 아닙니다")));
    }
    fs.unlink(Path::new(id)).await?;
    // Best effort: there is no store today, and when there is, one that failed to delete is not
    // a reason to leave the manifest behind and the resource half-registered.
    let _ = fs.unlink(Path::new(&store_path(id))).await;
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

    // Every id must still name a manifest in the tree. The form only sends back ids it was
    // given, so a mismatch means the workspace moved underneath it — a resource deleted between
    // the form opening and the save — and keeping the id would register an agent nobody can run.
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
/// that kind in — and neither is one file in it that will not parse: a listing that failed whole
/// because of one stray `.json` would hide every resource beside it.
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
            if entry.kind != DirentKind::File || !entry.name.ends_with(MANIFEST_EXT) {
                continue;
            }
            let path = format!("{dir}/{}", entry.name);
            let Ok(bytes) = read_all(fs, Path::new(&path), MANIFEST_CAP).await else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                continue;
            };
            out.push(resource_of(fs, path, manifest).await);
        }
    }
    out.sort_by_key(|resource| resource.created_ms);
    Ok(out)
}

/// A manifest plus the one thing that is not in it: whether its store exists.
async fn resource_of(fs: &dyn FileSystem, path: String, manifest: Manifest) -> Resource {
    let store = store_path(&path);
    Resource {
        backed: fs.stat(Path::new(&store)).await.is_ok(),
        store_path: store,
        id: path,
        kind: manifest.kind,
        name: manifest.name,
        note: manifest.note,
        created_ms: manifest.created_ms,
    }
}

/// The store that belongs to a manifest: the same name, the other extension.
fn store_path(manifest: &str) -> String {
    format!("{}{STORE_EXT}", manifest.trim_end_matches(MANIFEST_EXT))
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

    fn form(kind: ResourceKind, name: &str) -> ResourceForm {
        ResourceForm {
            kind,
            name: name.to_string(),
            note: String::new(),
        }
    }

    #[tokio::test]
    async fn a_resource_is_a_file_in_the_workspace() {
        let fs = workspace();
        let added = add(&fs, form(ResourceKind::Memory, "User 선호"))
            .await
            .unwrap();

        assert_eq!(added.id, "/.cortex/memory/user-선호.json");
        assert_eq!(added.store_path, "/.cortex/memory/user-선호.sqlite");
        // The directories it needed were made on the way, and the manifest is really there.
        assert!(fs.stat(Path::new(&added.id)).await.is_ok());
        assert!(fs.list(Path::new("/.cortex/memory")).await.unwrap().len() == 1);

        // And the listing is the tree's answer, not a copy of what `add` returned.
        let listed = read_resources(&fs).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "User 선호");
        assert_eq!(listed[0].kind, ResourceKind::Memory);
    }

    #[tokio::test]
    async fn backed_is_read_from_the_tree() {
        let fs = workspace();
        let added = add(&fs, form(ResourceKind::Docset, "docs")).await.unwrap();
        assert!(!added.backed, "nothing writes a store yet");

        // The day `index init` does, nothing here has to change for the window to say so.
        fs.create(Path::new(&added.store_path)).await.unwrap();
        assert!(read_resources(&fs).await.unwrap()[0].backed);
    }

    #[tokio::test]
    async fn two_names_that_slug_the_same_collide() {
        let fs = workspace();
        add(&fs, form(ResourceKind::Memory, "team notes"))
            .await
            .unwrap();
        let again = add(&fs, form(ResourceKind::Memory, "Team  Notes!")).await;
        assert!(again.is_err(), "the slug is the identity");
        assert_eq!(read_resources(&fs).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn removing_takes_the_store_with_it() {
        let fs = workspace();
        let added = add(&fs, form(ResourceKind::Docset, "docs")).await.unwrap();
        fs.create(Path::new(&added.store_path)).await.unwrap();

        remove(&fs, &added.id).await.unwrap();
        assert!(fs.stat(Path::new(&added.id)).await.is_err());
        assert!(fs.stat(Path::new(&added.store_path)).await.is_err());
        assert!(read_resources(&fs).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_resource_paths_can_be_removed() {
        let fs = workspace();
        fs.create(Path::new("/secret.json")).await.unwrap();

        assert!(remove(&fs, "/secret.json").await.is_err());
        assert!(remove(&fs, "/.cortex/memory/x.txt").await.is_err());
        assert!(fs.stat(Path::new("/secret.json")).await.is_ok());
    }

    #[tokio::test]
    async fn an_empty_workspace_has_no_resources() {
        // Neither directory exists, which is a workspace nobody has registered one in — not an
        // error, and not something the window should have to special-case.
        assert!(read_resources(&workspace()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_name_with_nothing_to_slug_is_refused() {
        let fs = workspace();
        assert!(
            add(&fs, form(ResourceKind::Memory, "  ///  "))
                .await
                .is_err()
        );
    }
}
