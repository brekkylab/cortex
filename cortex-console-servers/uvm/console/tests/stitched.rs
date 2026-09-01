//! A disk cortex stitched itself, booted.
//!
//! Everything under `src/layer` is unit-tested against files on this host, and none of that
//! says a kernel will mount the result. This does: it builds a two-layer image the way a
//! build's `commit` will, hands it to a real console under the reserved-host spelling, and
//! asks the guest what its filesystem looks like.
//!
//! **The control is half the test.** `a_lone_base_layer_has_none_of_it` runs the same
//! assertions against an image with the upper layer left out, and they have to fail there —
//! otherwise they are not testing the layering.
//!
//! `#[ignore]`, like `guest.rs` and for the same reasons: this needs libkrunfw installed, a
//! hypervisor the OS will let the process create, `codesign` on macOS, and — on a cold cache
//! — a rootfs download.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test stitched -- --ignored --test-threads=1
//! ```

use std::path::PathBuf;
use std::process::Stdio;

use cortex::console::{Console, ExecResult, ImageSource};
use cortex_uvm_console::built::BuiltStore;
use cortex_uvm_console::layer::{LayerId, LayerStore};
use microsandbox_image::tree::{
    DeviceNode, DirectoryNode, FileData, FileTree, InodeMetadata, RegularFileId, RegularFileNode,
    TreeNode, Xattr,
};
use tokio::process::Command;

const OPAQUE: &[u8] = b"trusted.overlay.opaque";

/// Distinctive, so that finding it cannot be a coincidence.
const MARKER: &str = "the upper layer was here";

fn meta(mode: u16) -> InodeMetadata {
    InodeMetadata {
        uid: 0,
        gid: 0,
        mode,
        mtime: 0,
        mtime_nsec: 0,
    }
}

/// What a build's `commit` would leave against an Alpine base: a file added, a file deleted,
/// a directory emptied.
fn upper() -> FileTree {
    let mut tree = FileTree::new();
    tree.insert(
        b"marker",
        TreeNode::RegularFile(RegularFileNode {
            id: RegularFileId::new(),
            metadata: meta(0o644),
            xattrs: vec![],
            data: FileData::Memory(format!("{MARKER}\n").into_bytes()),
            nlink: 1,
        }),
    )
    .unwrap();
    tree.insert(b"etc", TreeNode::Directory(DirectoryNode::new(meta(0o755))))
        .unwrap();
    tree.insert(
        b"etc/motd",
        TreeNode::CharDevice(DeviceNode {
            metadata: meta(0o644),
            major: 0,
            minor: 0,
        }),
    )
    .unwrap();
    let mut emptied = DirectoryNode::new(meta(0o755));
    emptied.xattrs.push(Xattr {
        name: OPAQUE.to_vec(),
        value: b"y".to_vec(),
    });
    tree.insert(b"media", TreeNode::Directory(emptied)).unwrap();
    tree
}

/// Where this host keeps what sessions share.
fn home() -> PathBuf {
    std::env::var_os("CORTEX_UVM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".cortex/uvm"))
}

/// The pinned rootfs tarball, provisioning it first if this host has never booted a session.
///
/// Booting one is the cheapest way to ask for it: the console downloads and verifies the
/// tarball on the way to its first command, and reaching in to do that here would be a
/// second copy of the code that decides which rootfs a host gets.
async fn provisioned_rootfs() -> PathBuf {
    let find = || {
        std::fs::read_dir(home().join("rootfs"))
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|e| e == "gz"))
    };

    if let Some(path) = find() {
        return path;
    }

    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    let client = cortex::console::stdio::StdioClient::new(server).expect("starting a server");
    let mut warm = Console::builder()
        .client(client)
        .build()
        .await
        .expect("a session on the pinned rootfs");
    assert_eq!(
        warm.exec(["true"], None)
            .await
            .expect("a first command")
            .code,
        0
    );

    find().expect("the console booted but provisioned no rootfs tarball")
}

/// The layer the pinned rootfs makes, put in the store under the digest of its tarball.
async fn base_layer(store: &LayerStore) -> cortex_uvm_console::layer::Layer {
    let rootfs = provisioned_rootfs().await;
    let id = LayerId::of(&std::fs::read(&rootfs).expect("reading the rootfs tarball"));
    if store.has(&id) {
        return store.get(&id).expect("the base layer");
    }

    let tree = microsandbox_image::tar::ingest_compressed_tar(
        tokio::fs::File::open(&rootfs).await.expect("opening it"),
        microsandbox_image::tar::Compression::Gzip,
        &microsandbox_image::tree::ResourceLimits::default(),
        None,
    )
    .await
    .expect("ingesting the rootfs")
    .tree;
    store.put(&id, &tree).expect("storing the base layer")
}

async fn session_on(image: &LayerId) -> Console {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    let client = cortex::console::stdio::StdioClient::new(server).expect("starting a server");
    Console::builder()
        .client(client)
        .image(ImageSource::new(format!("cortex.local/built@{image}")))
        .build()
        .await
        .expect("a session on the stitched image")
}

async fn out(console: &mut Console, script: &str) -> ExecResult {
    console
        .exec(["sh", "-c", script], None)
        .await
        .expect("running the command")
}

fn say(result: &ExecResult) -> String {
    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_stitched_image_boots_and_is_the_merged_filesystem() {
    let store = LayerStore::open(&home().join("layers")).expect("a layer store");
    let base = base_layer(&store).await;
    let upper_id = LayerId::of(MARKER.as_bytes());
    let over = store
        .put(&upper_id, &upper())
        .expect("storing the upper layer");

    let id = LayerId::of(b"cortex test: a stitched two-layer image");
    let built = BuiltStore::open(&cortex_uvm_console_built_dir()).expect("a built store");
    built
        .keep(&id, &[base, over], Vec::new(), None)
        .expect("keeping the image");
    let _kept = Kept::of(built.disk(&id), Some(upper_id));
    assert!(built.has(&id), "the image was not written");

    let mut console = session_on(&id).await;

    // Layer 1's own file. Nothing below runs if the kernel could not follow the fsmeta's
    // device table into the extents — `/bin/sh` itself lives in layer 0.
    assert_eq!(say(&out(&mut console, "cat /marker").await), MARKER);

    // The deletion, and the emptied directory.
    assert_eq!(
        say(&out(&mut console, "test -e /etc/motd; echo $?").await),
        "1"
    );
    assert_eq!(say(&out(&mut console, "ls -A /media | wc -l").await), "0");

    // Layer 0, read through the device table and executed out of it.
    let release = say(&out(&mut console, "cat /etc/alpine-release").await);
    assert!(release.starts_with("3."), "alpine-release is {release:?}");
    assert_eq!(
        say(&out(&mut console, "busybox true && echo ran").await),
        "ran"
    );

    // The session's own writes still land on top of both.
    assert_eq!(
        say(&out(&mut console, "echo hi > /tmp/w && cat /tmp/w").await),
        "hi"
    );
}

/// The control. Without the upper layer none of the above can hold, and if it does the
/// assertions are not testing the layering.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_lone_base_layer_has_none_of_it() {
    let store = LayerStore::open(&home().join("layers")).expect("a layer store");
    let base = base_layer(&store).await;

    let id = LayerId::of(b"cortex test: the base layer alone");
    let built = BuiltStore::open(&cortex_uvm_console_built_dir()).expect("a built store");
    built
        .keep(&id, &[base], Vec::new(), None)
        .expect("keeping one layer as an image");
    // No upper layer here; the base is the host's and stays.
    let _kept = Kept::of(built.disk(&id), None);

    let mut console = session_on(&id).await;

    assert_eq!(
        say(&out(&mut console, "test -e /marker; echo $?").await),
        "1",
        "/marker is there without the layer that adds it — the other test proves nothing"
    );
    assert_eq!(
        say(&out(&mut console, "test -e /etc/motd; echo $?").await),
        "0",
        "/etc/motd is gone without the whiteout that removes it"
    );
    assert_ne!(
        say(&out(&mut console, "ls -A /media | wc -l").await),
        "0",
        "/media is empty without the opaque directory that empties it"
    );
}

/// Where the server keeps images made here. One line, and stating it here is also what checks
/// that this and `base::built_store` still agree.
fn cortex_uvm_console_built_dir() -> PathBuf {
    home().join("built")
}

/// Leave nothing behind: these images are the test's, not the host's.
///
/// A value and not a call, because a call at the end of a test is a call a failing assertion
/// skips — and what it would have skipped is an image in the host's `built/` that the next
/// run silently writes over. Dropping happens either way.
struct Kept {
    disk: PathBuf,
    /// The upper layer, when the test put one. It is this test's alone, so it goes. The base
    /// layer stays either way: it is the pinned rootfs every session on this host already
    /// shares, and re-ingesting it per run would cost more than it saves.
    upper: Option<LayerId>,
}

impl Kept {
    fn of(disk: PathBuf, upper: Option<LayerId>) -> Kept {
        Kept { disk, upper }
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        for suffix in ["vmdk", "fsmeta.erofs", "manifest"] {
            let _ = std::fs::remove_file(self.disk.with_extension(suffix));
        }
        let Some(upper) = &self.upper else {
            return;
        };
        for suffix in ["erofs", "map"] {
            let _ = std::fs::remove_file(
                home()
                    .join("layers")
                    .join(format!("{}.{suffix}", upper.file_stem())),
            );
        }
    }
}
