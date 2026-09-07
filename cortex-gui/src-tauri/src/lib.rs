//! A window onto one cortex workspace.
//!
//! Two tabs over one piece of state. The first is the workspace itself — a [`WorkFs`] the
//! session starts empty, files dropped into it, and stores connected under it — and the second
//! registers agents against what the first holds. Everything the window shows is read back
//! through `cortex`'s own [`FileSystem`] trait on each call, so a Notion page that changed
//! upstream shows as changed the next time its directory is opened, rather than when some
//! cached copy is invalidated.
//!
//! [`WorkFs`]: cortex::fs::WorkFs
//! [`FileSystem`]: cortex::fs::FileSystem

mod agents;
mod error;
mod fsops;
mod mounts;
mod state;

use state::Workspace;

/// Build the window and run it.
///
/// Separate from `main.rs` because the mobile targets Tauri generates for enter here rather than
/// at a `main`, and because a `lib` is what an integration test could drive.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let workspace = Workspace::empty().expect("an empty workspace mounts an in-memory root");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(workspace)
        .invoke_handler(tauri::generate_handler![
            fsops::fs_list,
            fsops::fs_read,
            fsops::fs_write,
            fsops::fs_touch,
            fsops::fs_mkdir,
            fsops::fs_delete,
            fsops::fs_rename,
            fsops::fs_import,
            mounts::mounts,
            mounts::mount_local,
            mounts::mount_notion,
            mounts::mount_s3,
            mounts::unmount,
            agents::resources,
            agents::add_resource,
            agents::remove_resource,
            agents::agents,
            agents::create_agent,
            agents::delete_agent,
        ])
        .run(tauri::generate_context!())
        .expect("the window could not be created");
}
