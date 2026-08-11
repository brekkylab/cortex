use std::{ffi::OsStr, path::Path, time::Duration};

use super::*;
use crate::volume::InMemVolume;

const CONTENT: &[u8] = b"Hello from cortex!\n";

/// A volume holding `greeting.txt` at the root plus an empty `sub/`.
async fn adapter() -> PosixFs<InMemVolume> {
    let vol = InMemVolume::new();
    let (file, _) = vol
        .open(Path::new("greeting.txt"), OpenOptions::create_new())
        .await
        .unwrap();
    file.write_all_at(CONTENT, 0).await.unwrap();
    vol.mkdir(Path::new("sub")).await.unwrap();
    PosixFs::new(vol)
}

/// The names a directory lists, minus the dots, sorted.
async fn child_names<T: Mountable>(fs: &PosixFs<T>, inode: u64) -> Vec<String> {
    let mut names: Vec<_> = fs
        .dir_entries(inode)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, child)| child.name)
        .filter(|name| name != "." && name != "..")
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn walks_the_root() {
    let fs = adapter().await;

    // The root is pre-interned, so a listing works before any lookup.
    assert_eq!(child_names(&fs, ROOT_INODE).await, ["greeting.txt", "sub"]);

    let entries = fs.dir_entries(ROOT_INODE).await.unwrap();
    // `.` and `..` lead and both point back at this directory.
    assert_eq!(entries[0].0, ROOT_INODE);
    assert_eq!(entries[0].1.name, ".");
    assert_eq!(entries[1].0, ROOT_INODE);
    assert_eq!(entries[1].1.name, "..");
    assert_eq!(entries[0].1.kind, DirentKind::Dir);
    // Kinds survive the round trip.
    let sub = entries.iter().find(|(_, c)| c.name == "sub").unwrap();
    assert_eq!(sub.1.kind, DirentKind::Dir);

    // A number readdir hands out must be the one lookup confirms, or the kernel
    // would see two inodes for one file.
    let (looked_up, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("sub"))
        .await
        .unwrap();
    assert_eq!(sub.0, looked_up);

    // And what the root does not hold resolves to nothing — by name, and by a
    // number we never issued.
    assert!(matches!(
        fs.lookup_child(ROOT_INODE, OsStr::new("nope")).await,
        Err(CortexError::NotFound)
    ));
    assert!(matches!(
        fs.stat_inode(9999).await,
        Err(CortexError::NotFound)
    ));
}

#[tokio::test]
async fn a_created_file_is_reachable_by_every_route() {
    let fs = adapter().await;
    let (inode, stat, fh) = fs
        .create_child(
            ROOT_INODE,
            OsStr::new("fresh.txt"),
            OpenOptions::create_new(),
        )
        .await
        .unwrap();
    assert_eq!(stat.size, 0);

    // The handle, the inode, and the name must all now agree.
    assert_eq!(fs.write_handle(fh, 0, b"hello").await.unwrap(), 5);
    assert_eq!(fs.read_handle(fh, 0, 5).await.unwrap(), b"hello");
    assert_eq!(fs.stat_inode(inode).await.unwrap().size, 5);
    let (looked_up, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("fresh.txt"))
        .await
        .unwrap();
    assert_eq!(looked_up, inode);
    assert!(
        child_names(&fs, ROOT_INODE)
            .await
            .contains(&"fresh.txt".to_string())
    );

    // `create_new` is exclusive, and it is the backend that says so.
    assert!(matches!(
        fs.create_child(
            ROOT_INODE,
            OsStr::new("fresh.txt"),
            OpenOptions::create_new()
        )
        .await,
        Err(CortexError::AlreadyExists)
    ));
}

#[tokio::test]
async fn setattr_resizes_and_swallows_what_it_cannot_store() {
    let fs = adapter().await;
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();

    // The one field that is really applied.
    let stat = fs
        .setattr_inode(
            inode,
            None,
            SetAttr {
                size: Some(4),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stat.size, 4);
    let (fh, _) = fs
        .open_inode(inode, OpenOptions::read_only())
        .await
        .unwrap();
    assert_eq!(fs.read_handle(fh, 0, 16).await.unwrap(), &CONTENT[..4]);

    // Mode and ownership are accepted and dropped rather than refused:
    // failing would break `cp -p` and `tar -x` on every mount, and the
    // caller's own reply shows it what actually stuck.
    let stat = fs
        .setattr_inode(
            inode,
            None,
            SetAttr {
                mode: Some(0o600),
                uid: Some(42),
                gid: Some(42),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stat.size, 4);
    assert_eq!(attr_for(&stat).mode, 0o100000 | 0o644);
}

#[tokio::test]
async fn a_removed_name_does_not_resolve_to_the_old_inode() {
    let fs = adapter().await;
    let (old, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();
    let (fh, _) = fs.open_inode(old, OpenOptions::read_only()).await.unwrap();

    fs.unlink_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();

    // The open handle survives the removal, and so does `getattr` on the old
    // inode. POSIX requires both — an unlinked-but-open file stays readable
    // and `fstat`-able through its descriptor, which is the whole basis of
    // every tempfile implementation.
    assert_eq!(fs.read_handle(fh, 0, 5).await.unwrap(), &CONTENT[..5]);

    // A new file at the same name is a *different* file and gets a different
    // number; reusing the old one would make the kernel conflate the two.
    let (new, _, _) = fs
        .create_child(
            ROOT_INODE,
            OsStr::new("greeting.txt"),
            OpenOptions::create_new(),
        )
        .await
        .unwrap();
    assert_ne!(new, old);
}

#[tokio::test]
async fn rmdir_evicts_the_whole_subtree() {
    let fs = adapter().await;
    let (dir, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("sub"))
        .await
        .unwrap();
    let (_, _, fh) = fs
        .create_child(dir, OsStr::new("inner"), OpenOptions::create_new())
        .await
        .unwrap();
    let (child_before, _) = fs.lookup_child(dir, OsStr::new("inner")).await.unwrap();

    // A directory with children cannot go, and the backend is what says so.
    assert!(matches!(
        fs.rmdir_child(ROOT_INODE, OsStr::new("sub")).await,
        Err(CortexError::NotEmpty)
    ));

    fs.release_handle(fh).await.unwrap();
    fs.unlink_child(dir, OsStr::new("inner")).await.unwrap();
    fs.rmdir_child(ROOT_INODE, OsStr::new("sub")).await.unwrap();

    // Rebuilding the subtree must not reuse the old numbers. Evicting only
    // the directory's own name would leave `sub/inner` interned and hand the
    // stale number straight back.
    fs.mkdir_child(ROOT_INODE, OsStr::new("sub")).await.unwrap();
    let (_, _, fh) = fs
        .create_child(
            fs.lookup_child(ROOT_INODE, OsStr::new("sub"))
                .await
                .unwrap()
                .0,
            OsStr::new("inner"),
            OpenOptions::create_new(),
        )
        .await
        .unwrap();
    fs.release_handle(fh).await.unwrap();
    let (child_after, _) = fs
        .lookup_child(
            fs.lookup_child(ROOT_INODE, OsStr::new("sub"))
                .await
                .unwrap()
                .0,
            OsStr::new("inner"),
        )
        .await
        .unwrap();
    assert_ne!(child_after, child_before);
}

#[tokio::test]
async fn flush_does_not_finalize_but_release_does() {
    let fs = adapter().await;
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();
    let (fh, _) = fs
        .open_inode(inode, OpenOptions::read_write())
        .await
        .unwrap();

    // FLUSH arrives on every `close()` of a descriptor, so it must be
    // repeatable and must leave the handle usable. RELEASE comes once, for
    // the last one, and is where a backend may finalize.
    fs.flush_handle(fh).await.unwrap();
    fs.flush_handle(fh).await.unwrap();
    assert_eq!(fs.write_handle(fh, 0, b"X").await.unwrap(), 1);

    fs.release_handle(fh).await.unwrap();
    assert!(matches!(
        fs.flush_handle(fh).await,
        Err(CortexError::BadHandle)
    ));
    // A release for a handle we never issued is the kernel tidying up.
    fs.release_handle(fh).await.unwrap();
}

/// One policy, because the bindings project it onto different structs and must not
/// disagree — a directory reported with `nlink` 1 by one of them and 2 by another is
/// the kind of divergence a shared source removes.
#[test]
fn attribute_policy_is_shared_by_both_bindings() {
    let dir = attr_for(&Stat::new(DirentKind::Dir, 0));
    assert_eq!(dir.nlink, 1);
    assert_eq!(dir.mode, 0o040000 | 0o755);

    let file = attr_for(&Stat::new(DirentKind::File, 0));
    assert_eq!(file.nlink, 1);
    assert_eq!(file.mode, 0o100000 | 0o644);

    // Rounded up, and denominated in the `blksize` reported alongside.
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 0)).blocks, 0);
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 1)).blocks, 1);
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 512)).blocks, 1);
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 513)).blocks, 2);
    assert_eq!(file.blksize as u64, BLOCK_SIZE);

    // No times known at all lands on the epoch; one known time is followed by the
    // rest. A real `mtime` is functional: a guest that negotiated
    // `AUTO_INVAL_DATA` watches it to decide when to drop cached pages, so one
    // stuck at 0 never invalidates.
    assert_eq!(file.mtime, UNIX_EPOCH);
    assert_eq!(unix_time(file.mtime), (0, 0));
    let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut stat = Stat::new(DirentKind::File, 10);
    stat.mtime = Some(known);
    let attr = attr_for(&stat);
    assert_eq!(
        (attr.mtime, attr.atime, attr.ctime, attr.crtime),
        (known, known, known, known)
    );
    assert_eq!(unix_time(attr.mtime), (1_700_000_000, 0));

    // Pre-epoch clamps rather than wrapping into the future.
    assert_eq!(unix_time(UNIX_EPOCH - Duration::from_secs(1)), (0, 0));
}

/// Drift in an errno table is silent — a wrong number is still a valid number.
/// Never add a `_` arm: the exhaustive match is what makes a new `CortexError`
/// fail to compile until someone picks its number. Mirrors `krun.rs`'s Linux copy.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
#[test]
fn every_backend_error_has_its_own_host_errno() {
    for (err, expected) in [
        (CortexError::NotFound, libc::ENOENT),
        (CortexError::NotADirectory, libc::ENOTDIR),
        (CortexError::IsADirectory, libc::EISDIR),
        (CortexError::AlreadyExists, libc::EEXIST),
        (CortexError::NotEmpty, libc::ENOTEMPTY),
        (CortexError::InvalidName, libc::EINVAL),
        (CortexError::InvalidArgument, libc::EINVAL),
        (CortexError::FileTooLarge, libc::EFBIG),
        (CortexError::BadHandle, libc::EBADF),
        (CortexError::PermissionDenied, libc::EACCES),
        (CortexError::NoSpace, libc::ENOSPC),
        (CortexError::ReadOnly, libc::EROFS),
        (CortexError::CrossDevice, libc::EXDEV),
        (CortexError::Unsupported, libc::ENOSYS),
    ] {
        let got = host_errno(&err);
        assert_eq!(got, expected, "{err:?}");
        // Only a real fault may land on EIO — a caller cannot tell "disk full"
        // from "denied" from it.
        assert_ne!(got, libc::EIO, "{err:?} collapsed onto EIO");
    }

    // `Io` passes its own number through, falling back to EIO only without one.
    assert_eq!(
        host_errno(&CortexError::Io(std::io::Error::from_raw_os_error(
            libc::ENOSPC
        ))),
        libc::ENOSPC
    );
    assert_eq!(
        host_errno(&CortexError::Io(std::io::Error::other("no errno"))),
        libc::EIO
    );
}

#[tokio::test]
async fn a_listing_carries_metadata_when_the_backend_had_it() {
    let fs = adapter().await;
    let entries = fs.dir_entries(ROOT_INODE).await.unwrap();

    // `InMemVolume` already locks each child for its kind, so filling the stat is
    // free — which is what lets a `readdirplus` answer without an N+1.
    let file = entries
        .iter()
        .find(|(_, c)| c.name == "greeting.txt")
        .unwrap();
    let stat = file.1.stat().expect("in-memory listing knows sizes");
    assert_eq!(stat.size, CONTENT.len() as u64);
    assert_eq!(stat.kind, DirentKind::File);

    // The dots are synthesized, not backend entries.
    assert!(entries[0].1.stat().is_none());
}

#[tokio::test]
async fn reads_a_file_through_a_handle() {
    let fs = adapter().await;
    let (inode, stat) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, CONTENT.len() as u64);

    let (fh, _) = fs
        .open_inode(inode, OpenOptions::read_only())
        .await
        .unwrap();
    assert_eq!(
        fs.read_handle(fh, 0, CONTENT.len() as u32).await.unwrap(),
        CONTENT
    );

    // Short rather than an error, which is how the kernel finds the end.
    assert!(fs.read_handle(fh, 0, 4096).await.unwrap().len() == CONTENT.len());
    assert!(
        fs.read_handle(fh, CONTENT.len() as u64, 16)
            .await
            .unwrap()
            .is_empty()
    );

    // Mid-file offsets address bytes, not blocks.
    assert_eq!(fs.read_handle(fh, 6, 4).await.unwrap(), b"from");

    fs.release_handle(fh).await.unwrap();
    // A *bad handle*, not a missing name: told "no such file" for a closed
    // descriptor, a caller retries the open forever.
    assert!(matches!(
        fs.read_handle(fh, 0, 1).await,
        Err(CortexError::BadHandle)
    ));
}

/// A volume holding one file, for standing in as a mounted source.
async fn source() -> InMemVolume {
    let vol = InMemVolume::new();
    let (file, _) = Mountable::open(
        &vol,
        Path::new("greeting.txt"),
        crate::volume::OpenOptions::create_new(),
    )
    .await
    .unwrap();
    crate::volume::FileExt::write_all_at(&file, CONTENT, 0)
        .await
        .unwrap();
    vol
}

/// Sources side by side with nothing at the root, driven as an adapter drives it.
/// Also the only test of `Workspace`'s erased handle (`Box<dyn FileHandle>`) —
/// every other backend's is concrete.
#[tokio::test]
async fn a_multi_source_workspace_walks_from_its_root() {
    let ws = crate::volume::Workspace::new()
        .try_with_mount("s3-like", source().await)
        .unwrap()
        .try_with_mount("notes", source().await)
        .unwrap();
    let fs = PosixFs::new(ws);

    // The kernel's first question about any mount.
    assert_eq!(
        fs.stat_inode(ROOT_INODE).await.unwrap().kind,
        DirentKind::Dir
    );
    assert_eq!(child_names(&fs, ROOT_INODE).await, ["notes", "s3-like"]);

    // readdir's number is the one lookup confirms, and the child is walkable.
    let listed = fs
        .dir_entries(ROOT_INODE)
        .await
        .unwrap()
        .into_iter()
        .find(|(_, entry)| entry.name == "s3-like")
        .unwrap()
        .0;
    let (child, stat) = fs
        .lookup_child(ROOT_INODE, OsStr::new("s3-like"))
        .await
        .unwrap();
    assert_eq!(listed, child);
    assert_eq!(stat.kind, DirentKind::Dir);
    assert_eq!(child_names(&fs, child).await, ["greeting.txt"]);

    // Reading exercises the erased handle.
    let (file, _) = fs
        .lookup_child(child, OsStr::new("greeting.txt"))
        .await
        .unwrap();
    let (handle, _) = fs
        .open_inode(file, crate::volume::OpenOptions::read_only())
        .await
        .unwrap();
    assert_eq!(
        fs.read_handle(handle, 0, CONTENT.len() as u32)
            .await
            .unwrap(),
        CONTENT
    );
}

/// A mount over a backend entry of the same name. The two kinds describe one
/// inode: when they disagree, `find -type f` calls the mount point a file and
/// never descends, while `cd` into it works.
#[tokio::test]
async fn readdir_and_lookup_agree_on_a_shadowed_mount_point() {
    let root = InMemVolume::new();
    let (file, _) = Mountable::open(
        &root,
        Path::new("data"),
        crate::volume::OpenOptions::create_new(),
    )
    .await
    .unwrap();
    drop(file);

    let ws = crate::volume::Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data", source().await)
        .unwrap();
    let fs = PosixFs::new(ws);

    let listed = fs
        .dir_entries(ROOT_INODE)
        .await
        .unwrap()
        .into_iter()
        .find(|(_, entry)| entry.name == "data")
        .unwrap();
    let (inode, stat) = fs
        .lookup_child(ROOT_INODE, OsStr::new("data"))
        .await
        .unwrap();

    assert_eq!(listed.0, inode);
    assert_eq!(
        listed.1.kind, stat.kind,
        "readdir's d_type and getattr's mode describe the same inode"
    );
    assert_eq!(stat.kind, DirentKind::Dir, "the mount wins, not the file");
    assert_eq!(
        child_names(&fs, inode).await,
        ["greeting.txt"],
        "and it is reachable"
    );
}

/// The shape a host mount and a WebDAV handler need to serve the same sources at
/// once. Each `PosixFs` owns its inode table, so the two need not agree on
/// numbering — only on the *store*. Unwritable without `Mountable for Arc<T>`.
#[tokio::test]
async fn one_workspace_can_feed_two_independent_consumers() {
    use std::sync::Arc;

    let ws = Arc::new(
        crate::volume::Workspace::new()
            .try_with_mount("notes", source().await)
            .unwrap(),
    );
    let agent_side = PosixFs::new(Arc::clone(&ws));
    let user_side = PosixFs::new(Arc::clone(&ws));

    assert_eq!(child_names(&agent_side, ROOT_INODE).await, ["notes"]);
    assert_eq!(child_names(&user_side, ROOT_INODE).await, ["notes"]);

    // What one consumer creates, the other sees — neither knows the other exists.
    let (notes, _) = agent_side
        .lookup_child(ROOT_INODE, OsStr::new("notes"))
        .await
        .unwrap();
    agent_side
        .mkdir_child(notes, OsStr::new("added"))
        .await
        .unwrap();

    let (same_notes, _) = user_side
        .lookup_child(ROOT_INODE, OsStr::new("notes"))
        .await
        .unwrap();
    assert_eq!(
        child_names(&user_side, same_notes).await,
        ["added", "greeting.txt"]
    );

    // And the third shape: no inode layer, as a path-addressed binding reaches it.
    assert_eq!(
        Mountable::list(&*ws, Path::new("notes"))
            .await
            .unwrap()
            .len(),
        2
    );
}

/// The number the kernel was given survives the move, so it keeps resolving —
/// the reason this is a rekey and not an eviction.
#[tokio::test]
async fn rename_keeps_the_inode_the_kernel_is_holding() {
    let fs = adapter().await;
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();

    fs.rename_child(
        ROOT_INODE,
        OsStr::new("greeting.txt"),
        ROOT_INODE,
        OsStr::new("moved.txt"),
    )
    .await
    .unwrap();

    // The kernel still quotes `inode`, and the new name resolves to it — two
    // numbers for one file would break its cache.
    assert_eq!(
        fs.stat_inode(inode).await.unwrap().size,
        CONTENT.len() as u64
    );
    let (again, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("moved.txt"))
        .await
        .unwrap();
    assert_eq!(again, inode);
    assert!(matches!(
        fs.lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
            .await,
        Err(CortexError::NotFound)
    ));
}

/// Moving a directory carries every descendant's number with it.
#[tokio::test]
async fn rename_carries_a_whole_subtree() {
    let fs = adapter().await;
    let (sub, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("sub"))
        .await
        .unwrap();
    fs.create_child(sub, OsStr::new("inner.txt"), OpenOptions::create_new())
        .await
        .unwrap();
    let (inner, _) = fs.lookup_child(sub, OsStr::new("inner.txt")).await.unwrap();

    fs.rename_child(
        ROOT_INODE,
        OsStr::new("sub"),
        ROOT_INODE,
        OsStr::new("moved"),
    )
    .await
    .unwrap();

    assert_eq!(fs.stat_inode(sub).await.unwrap().kind, DirentKind::Dir);
    assert!(
        fs.stat_inode(inner).await.is_ok(),
        "a descendant's number must still resolve"
    );
    let (moved_dir, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("moved"))
        .await
        .unwrap();
    assert_eq!(moved_dir, sub);
    assert_eq!(child_names(&fs, moved_dir).await, ["inner.txt"]);
}

/// A refused rename leaves the table as it was; an eager rekey would strand the
/// numbers on paths that never changed.
#[tokio::test]
async fn a_refused_rename_does_not_touch_the_table() {
    let fs = adapter().await;
    let (file, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();
    let (dir, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("sub"))
        .await
        .unwrap();

    // file over directory: EISDIR, straight from the backend.
    assert!(matches!(
        fs.rename_child(
            ROOT_INODE,
            OsStr::new("greeting.txt"),
            ROOT_INODE,
            OsStr::new("sub")
        )
        .await,
        Err(CortexError::IsADirectory)
    ));

    assert_eq!(fs.path_of(file).unwrap(), Path::new("/greeting.txt"));
    assert_eq!(fs.path_of(dir).unwrap(), Path::new("/sub"));
    assert_eq!(child_names(&fs, ROOT_INODE).await, ["greeting.txt", "sub"]);
}

/// A table with `paths` interned, for driving `rekey_subtree` directly.
fn table(paths: &[&str]) -> InodeTable {
    let mut t = InodeTable::new();
    for path in paths {
        t.intern(PathBuf::from(path));
    }
    t
}

fn snapshot(t: &InodeTable) -> Vec<(u64, String)> {
    let mut rows: Vec<_> = t
        .rev
        .iter()
        .map(|(p, &ino)| (ino, p.display().to_string()))
        .collect();
    rows.sort();
    rows
}

/// An overlapping move leaves the table **completely** alone.
///
/// A backend refuses all of these already, so the guard is not what enforces the
/// rule — it is what stops a *half*-rewritten table if one slips through.
/// Measured before it existed: `/x → /x` and `/a/b → /a` each stripped `rev` to
/// the root, after which the next `lookup` mints a second inode for a path `fwd`
/// already knows.
#[test]
fn rekey_refuses_an_overlapping_move() {
    for (from, to, why) in [
        ("/x", "/x", "a path onto itself"),
        ("/a", "/a/b", "a directory into its own descendant"),
        ("/a/b", "/a", "a directory onto its own ancestor"),
        ("/", "/new", "the root, which every path starts with"),
    ] {
        let mut t = table(&["/x", "/x/y", "/a", "/a/b", "/a/b/c"]);
        let before = snapshot(&t);
        let paths_before: Vec<_> = t.fwd.values().map(|d| d.path.clone()).collect();

        t.rekey_subtree(Path::new(from), Path::new(to));

        assert_eq!(snapshot(&t), before, "rev changed for {why}");
        let mut a: Vec<_> = t.fwd.values().map(|d| d.path.clone()).collect();
        let mut b = paths_before;
        a.sort();
        b.sort();
        assert_eq!(a, b, "fwd changed for {why}");
    }
}

/// The numbers survive, which is why this is a rekey and not an eviction: the
/// kernel goes on quoting them, so dropping one means `ESTALE`.
#[test]
fn rekey_keeps_every_number_and_moves_the_whole_subtree() {
    let mut t = InodeTable::new();
    let dir = t.intern(PathBuf::from("/notes"));
    let file = t.intern(PathBuf::from("/notes/a.md"));
    let deep = t.intern(PathBuf::from("/notes/sub/d.md"));

    t.rekey_subtree(Path::new("/notes"), Path::new("/archive"));

    assert_eq!(t.path_of(dir).as_deref(), Some(Path::new("/archive")));
    assert_eq!(t.path_of(file).as_deref(), Some(Path::new("/archive/a.md")));
    assert_eq!(
        t.path_of(deep).as_deref(),
        Some(Path::new("/archive/sub/d.md"))
    );

    // A moved path resolves to the same number, and nothing outside the subtree
    // is disturbed.
    assert_eq!(t.rev.get(Path::new("/archive/a.md")), Some(&file));
    assert!(!t.rev.contains_key(Path::new("/notes/a.md")));
    assert_eq!(t.path_of(ROOT_INODE).as_deref(), Some(Path::new("/")));
}

/// Renaming onto an existing entry: whatever was there is replaced, so the moved
/// inode takes the name over.
#[test]
fn rekey_hands_an_overwritten_destination_to_the_mover() {
    let mut t = InodeTable::new();
    let from = t.intern(PathBuf::from("/from.md"));
    let doomed = t.intern(PathBuf::from("/to.md"));
    assert_ne!(from, doomed);

    t.rekey_subtree(Path::new("/from.md"), Path::new("/to.md"));

    assert_eq!(t.rev.get(Path::new("/to.md")), Some(&from));
    assert!(!t.rev.contains_key(Path::new("/from.md")));
    assert_eq!(t.path_of(from).as_deref(), Some(Path::new("/to.md")));
    // The replaced inode keeps its forward entry, so an already-open handle stays
    // `fstat`-able — the same reason `evict_path` keeps one.
    assert!(t.path_of(doomed).is_some());
}

/// `to.join("")` would append a separator, and the other rekey tests cannot see
/// it: `Path` equality ignores a trailing one, so only the display does.
#[test]
fn rekey_does_not_leave_a_trailing_separator() {
    let mut t = InodeTable::new();
    let ino = t.intern(PathBuf::from("/a"));

    t.rekey_subtree(Path::new("/a"), Path::new("/b"));

    let moved = t.path_of(ino).unwrap();
    assert_eq!(moved.display().to_string(), "/b", "not \"/b/\"");
}

/// A `lookup` takes a reference the kernel balances with a `forget`, so they are
/// counted rather than treated as a flag.
#[tokio::test]
async fn inode_references_are_counted_and_the_root_is_spared() {
    let fs = adapter().await;

    // One lookup, one reference: forgetting it evicts the entry.
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .await
        .unwrap();
    assert!(fs.stat_inode(inode).await.is_ok());
    fs.forget_inode(inode, 1);
    assert!(matches!(
        fs.stat_inode(inode).await,
        Err(CortexError::NotFound)
    ));

    // Looked up twice is one inode with two references, so the first forget has
    // to leave it live.
    let (first, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("sub"))
        .await
        .unwrap();
    let (second, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("sub"))
        .await
        .unwrap();
    assert_eq!(first, second);
    fs.forget_inode(first, 1);
    assert!(fs.stat_inode(first).await.is_ok());
    fs.forget_inode(first, 1);
    assert!(matches!(
        fs.stat_inode(first).await,
        Err(CortexError::NotFound)
    ));

    // The root survives any amount: every path resolves through it.
    fs.forget_inode(ROOT_INODE, 1_000);
    assert!(fs.stat_inode(ROOT_INODE).await.is_ok());
}

/// `evict_path` keeps the forward entry, so one path can have two of them: the
/// unlinked-but-open inode and the one created in its place. A `forget` arriving
/// for the first must not take the second's name with it.
#[test]
fn forgetting_an_unlinked_inode_leaves_a_re_interned_path_mapped() {
    let mut t = InodeTable::new();
    let unlinked = t.intern(PathBuf::from("/a"));
    t.evict_path(Path::new("/a"));
    let replacement = t.intern(PathBuf::from("/a"));

    t.forget(unlinked, 1);

    assert_eq!(t.path_of(replacement).as_deref(), Some(Path::new("/a")));
    assert_eq!(t.number_for(PathBuf::from("/a")), replacement);
}
