use super::*;
use crate::InMemVolume;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

const CONTENT: &[u8] = b"Hello from cortex!\n";

/// A volume holding `greeting.txt` at the root plus an empty `sub/`.
fn adapter() -> PosixFs<InMemVolume> {
    let vol = InMemVolume::new();
    let (file, _) = vol
        .open(
            Path::new("greeting.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    file.write_all_at(CONTENT, 0).unwrap();
    vol.mkdir(Path::new("sub")).unwrap();
    PosixFs::new(vol)
}

/// The names a directory lists, minus the dots, sorted.
fn child_names<T: Mountable>(fs: &PosixFs<T>, inode: u64) -> Vec<String> {
    let mut names: Vec<_> = fs
        .dir_entries(inode)
        .unwrap()
        .into_iter()
        .map(|(_, child)| child.name)
        .filter(|name| name != "." && name != "..")
        .collect();
    names.sort();
    names
}

#[test]
fn walks_the_root() {
    let fs = adapter();

    // The root is pre-interned, so a listing works before any lookup.
    assert_eq!(child_names(&fs, ROOT_INODE), ["greeting.txt", "sub"]);

    let entries = fs.dir_entries(ROOT_INODE).unwrap();
    // `.` and `..` lead and both point back at this directory.
    assert_eq!(entries[0].0, ROOT_INODE);
    assert_eq!(entries[0].1.name, ".");
    assert_eq!(entries[1].0, ROOT_INODE);
    assert_eq!(entries[1].1.name, "..");
    assert_eq!(entries[0].1.kind, DirentKind::Dir);
    // Kinds survive the round trip.
    let sub = entries.iter().find(|(_, c)| c.name == "sub").unwrap();
    assert_eq!(sub.1.kind, DirentKind::Dir);
}

#[test]
fn a_created_file_is_reachable_by_every_route() {
    let fs = adapter();
    let (inode, stat, fh) = fs
        .create_child(
            ROOT_INODE,
            OsStr::new("fresh.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    assert_eq!(stat.size, 0);

    // The handle, the inode, and the name must all now agree.
    assert_eq!(fs.write_handle(fh, 0, b"hello").unwrap(), 5);
    assert_eq!(fs.read_handle(fh, 0, 5).unwrap(), b"hello");
    assert_eq!(fs.stat_inode(inode).unwrap().size, 5);
    let (looked_up, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("fresh.txt"))
        .unwrap();
    assert_eq!(looked_up, inode);
    assert!(child_names(&fs, ROOT_INODE).contains(&"fresh.txt".to_string()));

    // `create_new` is exclusive, and it is the backend that says so.
    assert!(matches!(
        fs.create_child(
            ROOT_INODE,
            OsStr::new("fresh.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            }
        ),
        Err(CortexError::AlreadyExists)
    ));
}

#[test]
fn setattr_resizes_and_swallows_what_it_cannot_store() {
    let fs = adapter();
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
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
        .unwrap();
    assert_eq!(stat.size, 4);
    let (fh, _) = fs.open_inode(inode, OpenOptions::read_only()).unwrap();
    assert_eq!(fs.read_handle(fh, 0, 16).unwrap(), &CONTENT[..4]);

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
        .unwrap();
    assert_eq!(stat.size, 4);
    assert_eq!(attr_for(&stat).mode, 0o100000 | 0o644);
}

#[test]
fn a_removed_name_does_not_resolve_to_the_old_inode() {
    let fs = adapter();
    let (old, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();
    let (fh, _) = fs.open_inode(old, OpenOptions::read_only()).unwrap();

    fs.unlink_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();

    // The open handle survives the removal, and so does `getattr` on the old
    // inode. POSIX requires both — an unlinked-but-open file stays readable
    // and `fstat`-able through its descriptor, which is the whole basis of
    // every tempfile implementation.
    assert_eq!(fs.read_handle(fh, 0, 5).unwrap(), &CONTENT[..5]);

    // A new file at the same name is a *different* file and gets a different
    // number; reusing the old one would make the kernel conflate the two.
    let (new, _, _) = fs
        .create_child(
            ROOT_INODE,
            OsStr::new("greeting.txt"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    assert_ne!(new, old);
}

#[test]
fn rmdir_evicts_the_whole_subtree() {
    let fs = adapter();
    let (dir, _) = fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap();
    let (_, _, fh) = fs
        .create_child(
            dir,
            OsStr::new("inner"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    let (child_before, _) = fs.lookup_child(dir, OsStr::new("inner")).unwrap();

    // A directory with children cannot go, and the backend is what says so.
    assert!(matches!(
        fs.rmdir_child(ROOT_INODE, OsStr::new("sub")),
        Err(CortexError::NotEmpty)
    ));

    fs.release_handle(fh).unwrap();
    fs.unlink_child(dir, OsStr::new("inner")).unwrap();
    fs.rmdir_child(ROOT_INODE, OsStr::new("sub")).unwrap();

    // Rebuilding the subtree must not reuse the old numbers. Evicting only
    // the directory's own name would leave `sub/inner` interned and hand the
    // stale number straight back.
    fs.mkdir_child(ROOT_INODE, OsStr::new("sub")).unwrap();
    let (_, _, fh) = fs
        .create_child(
            fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap().0,
            OsStr::new("inner"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    fs.release_handle(fh).unwrap();
    let (child_after, _) = fs
        .lookup_child(
            fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap().0,
            OsStr::new("inner"),
        )
        .unwrap();
    assert_ne!(child_after, child_before);
}

#[test]
fn flush_does_not_finalize_but_release_does() {
    let fs = adapter();
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();
    let (fh, _) = fs.open_inode(inode, OpenOptions::read_write()).unwrap();

    // FLUSH arrives on every `close()` of a descriptor, so it must be
    // repeatable and must leave the handle usable. RELEASE comes once, for
    // the last one, and is where a backend may finalize.
    fs.flush_handle(fh).unwrap();
    fs.flush_handle(fh).unwrap();
    assert_eq!(fs.write_handle(fh, 0, b"X").unwrap(), 1);

    fs.release_handle(fh).unwrap();
    assert!(matches!(fs.flush_handle(fh), Err(CortexError::BadHandle)));
    // A release for a handle we never issued is the kernel tidying up.
    fs.release_handle(fh).unwrap();
}

#[test]
fn attribute_policy_is_shared_by_both_bindings() {
    // A directory's link count is 2 even with no subdirectories, because `.`
    // and `..` are links to it. The two bindings disagreed here — one said 1
    // — which is the drift this shared policy exists to prevent.
    let dir = attr_for(&Stat::new(DirentKind::Dir, 0));
    assert_eq!(dir.nlink, 2);
    assert_eq!(dir.mode, 0o040000 | 0o755);

    let file = attr_for(&Stat::new(DirentKind::File, 0));
    assert_eq!(file.nlink, 1);
    assert_eq!(file.mode, 0o100000 | 0o644);

    // Block counts round up: a 1-byte file still occupies a block, and the
    // reported `blksize` is what that count is denominated in.
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 0)).blocks, 0);
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 1)).blocks, 1);
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 512)).blocks, 1);
    assert_eq!(attr_for(&Stat::new(DirentKind::File, 513)).blocks, 2);
    assert_eq!(file.blksize as u64, BLOCK_SIZE);
}

/// The host errno table, pinned the way the guest one already is
/// (`krun.rs`'s `every_backend_error_has_its_own_linux_errno`).
///
/// It had no test at all, which is the worst shape for a lookup table: drift is
/// silent because a wrong errno is still a valid errno. This is a second copy
/// that has to agree with the first, so editing one and not the other shows up.
/// Never add a `_` arm to either — an exhaustive match is what makes a new
/// `CortexError` variant fail to compile until someone chooses its number.
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
        // Nothing but a real fault may land on EIO: a caller cannot tell "disk
        // full" from "denied" from a genuine I/O error.
        assert_ne!(got, libc::EIO, "{err:?} collapsed onto EIO");
    }

    // `Io` carries the underlying number through when it has one, and only
    // falls back to EIO when it does not.
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

#[test]
fn timestamps_fall_back_rather_than_reporting_zero() {
    // A backend that knows no times at all lands on the epoch.
    let unknown = attr_for(&Stat::new(DirentKind::File, 0));
    assert_eq!(unknown.mtime, UNIX_EPOCH);
    assert_eq!(unix_time(unknown.mtime), (0, 0));

    // One that knows only a modification time has the others follow it,
    // rather than each independently reading as 1970. Reporting a real
    // `mtime` is functional, not cosmetic: the guest negotiates
    // `AUTO_INVAL_DATA` and watches `mtime` to decide when to drop cached
    // pages, so a filesystem stuck at 0 never gets its cache invalidated.
    let known = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut stat = Stat::new(DirentKind::File, 10);
    stat.mtime = Some(known);
    let attr = attr_for(&stat);
    assert_eq!(attr.mtime, known);
    assert_eq!(attr.atime, known);
    assert_eq!(attr.ctime, known);
    assert_eq!(attr.crtime, known);
    assert_eq!(unix_time(attr.mtime), (1_700_000_000, 0));

    // Pre-epoch times clamp instead of wrapping, which would otherwise be
    // reported to the kernel as a time in the future.
    assert_eq!(unix_time(UNIX_EPOCH - Duration::from_secs(1)), (0, 0));
}

#[test]
fn a_listing_carries_metadata_when_the_backend_had_it() {
    let fs = adapter();
    let entries = fs.dir_entries(ROOT_INODE).unwrap();

    // `InMemVolume` already locks each child to learn its kind, so it fills
    // the metadata in — this is what lets a `readdirplus` answer without an
    // extra round trip per entry.
    let file = entries
        .iter()
        .find(|(_, c)| c.name == "greeting.txt")
        .unwrap();
    let stat = file.1.stat.as_ref().expect("in-memory listing knows sizes");
    assert_eq!(stat.size, CONTENT.len() as u64);
    assert_eq!(stat.kind, DirentKind::File);

    // The synthesized dots carry no metadata: they are not backend entries,
    // and a caller that wants the directory's own attributes has the inode.
    assert!(entries[0].1.stat.is_none());
}

#[test]
fn reads_a_file_through_a_handle() {
    let fs = adapter();
    let (inode, stat) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, CONTENT.len() as u64);

    let (fh, _) = fs.open_inode(inode, OpenOptions::read_only()).unwrap();
    assert_eq!(
        fs.read_handle(fh, 0, CONTENT.len() as u32).unwrap(),
        CONTENT
    );

    // A window past EOF comes back short rather than erroring, which is how
    // the kernel learns where the file ends.
    assert!(fs.read_handle(fh, 0, 4096).unwrap().len() == CONTENT.len());
    assert!(
        fs.read_handle(fh, CONTENT.len() as u64, 16)
            .unwrap()
            .is_empty()
    );

    // Mid-file offsets address bytes, not blocks.
    assert_eq!(fs.read_handle(fh, 6, 4).unwrap(), b"from");

    fs.release_handle(fh).unwrap();
    // The handle is gone once released — and reported as a *bad handle*, not
    // as a missing name. A caller told "no such file" for a closed
    // descriptor retries the open forever; told "bad descriptor", it fixes
    // its own bookkeeping.
    assert!(matches!(
        fs.read_handle(fh, 0, 1),
        Err(CortexError::BadHandle)
    ));
}

#[test]
fn readdir_and_lookup_agree_on_inode_numbers() {
    let fs = adapter();
    let listed = fs
        .dir_entries(ROOT_INODE)
        .unwrap()
        .into_iter()
        .find(|(_, child)| child.name == "greeting.txt")
        .unwrap()
        .0;
    let (looked_up, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();
    // A number handed out by readdir must be the one lookup confirms —
    // otherwise the kernel would see two inodes for one file.
    assert_eq!(listed, looked_up);
}

/// A volume holding one file, for standing in as a mounted source.
fn source() -> InMemVolume {
    let vol = InMemVolume::new();
    let (file, _) = Mountable::open(
        &vol,
        Path::new("greeting.txt"),
        crate::OpenOptions {
            create_new: true,
            ..crate::OpenOptions::read_write()
        },
    )
    .unwrap();
    crate::FileExt::write_all_at(&file, CONTENT, 0).unwrap();
    vol
}

/// A workspace with sources side by side and nothing at the root, driven
/// exactly as an adapter drives it — no kernel, so this runs everywhere.
///
/// This is also the first execution of `Workspace`'s erased handle
/// (`Handle = Box<dyn FileHandle>`): every other test drives a backend whose
/// handle is concrete.
#[test]
fn a_multi_source_workspace_walks_from_its_root() {
    let ws = crate::Workspace::new()
        .try_with_mount("s3-like", source())
        .unwrap()
        .try_with_mount("notes", source())
        .unwrap();
    let fs = PosixFs::new(ws);

    // The kernel's first question about any mount.
    assert_eq!(fs.stat_inode(ROOT_INODE).unwrap().kind, DirentKind::Dir);
    assert_eq!(child_names(&fs, ROOT_INODE), ["notes", "s3-like"]);

    // A number readdir handed out is the one lookup confirms, and the child is
    // itself walkable.
    let listed = fs
        .dir_entries(ROOT_INODE)
        .unwrap()
        .into_iter()
        .find(|(_, entry)| entry.name == "s3-like")
        .unwrap()
        .0;
    let (child, stat) = fs.lookup_child(ROOT_INODE, OsStr::new("s3-like")).unwrap();
    assert_eq!(listed, child);
    assert_eq!(stat.kind, DirentKind::Dir);
    assert_eq!(child_names(&fs, child), ["greeting.txt"]);

    // ...and reading through it exercises the erased handle.
    let (file, _) = fs.lookup_child(child, OsStr::new("greeting.txt")).unwrap();
    let (handle, _) = fs
        .open_inode(file, crate::OpenOptions::read_only())
        .unwrap();
    assert_eq!(
        fs.read_handle(handle, 0, CONTENT.len() as u32).unwrap(),
        CONTENT
    );
}

/// A mount over a backend entry of the same name: `readdir`'s kind and
/// `lookup`'s kind describe one inode, so they must not disagree. When they do,
/// `find -type f` reports the mount point as a file and never descends, while
/// `cd` into it works.
#[test]
fn readdir_and_lookup_agree_on_a_shadowed_mount_point() {
    let root = InMemVolume::new();
    let (file, _) = Mountable::open(
        &root,
        Path::new("data"),
        crate::OpenOptions {
            create_new: true,
            ..crate::OpenOptions::read_write()
        },
    )
    .unwrap();
    drop(file);

    let ws = crate::Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data", source())
        .unwrap();
    let fs = PosixFs::new(ws);

    let listed = fs
        .dir_entries(ROOT_INODE)
        .unwrap()
        .into_iter()
        .find(|(_, entry)| entry.name == "data")
        .unwrap();
    let (inode, stat) = fs.lookup_child(ROOT_INODE, OsStr::new("data")).unwrap();

    assert_eq!(listed.0, inode);
    assert_eq!(
        listed.1.kind, stat.kind,
        "readdir's d_type and getattr's mode describe the same inode"
    );
    assert_eq!(stat.kind, DirentKind::Dir, "the mount wins, not the file");
    // And the mount is actually reachable through that inode.
    assert_eq!(child_names(&fs, inode), ["greeting.txt"]);
}

/// One workspace behind an `Arc`, feeding two independent consumers — the shape
/// a host mount and a WebDAV handler need when they serve the same sources at
/// once.
///
/// Each `PosixFs` keeps its own inode and handle tables, so the two need not
/// agree on numbering; what has to hold is that they see the same *store*.
/// Without `impl Mountable for Arc<T>` this cannot be written at all —
/// `PosixFs::new` consumes its backend and never gives it back.
#[test]
fn one_workspace_can_feed_two_independent_consumers() {
    use std::sync::Arc;

    let ws = Arc::new(
        crate::Workspace::new()
            .try_with_mount("notes", source())
            .unwrap(),
    );
    let agent_side = PosixFs::new(Arc::clone(&ws));
    let user_side = PosixFs::new(Arc::clone(&ws));

    // Both start from the same picture.
    assert_eq!(child_names(&agent_side, ROOT_INODE), ["notes"]);
    assert_eq!(child_names(&user_side, ROOT_INODE), ["notes"]);

    // A directory created through one consumer is visible through the other,
    // even though neither knows the other exists.
    let (notes, _) = agent_side
        .lookup_child(ROOT_INODE, OsStr::new("notes"))
        .unwrap();
    agent_side.mkdir_child(notes, OsStr::new("added")).unwrap();

    let (same_notes, _) = user_side
        .lookup_child(ROOT_INODE, OsStr::new("notes"))
        .unwrap();
    assert_eq!(
        child_names(&user_side, same_notes),
        ["added", "greeting.txt"]
    );

    // The third consumer shape: no inode layer at all, the way a path-addressed
    // binding (WebDAV) would reach the same store.
    assert_eq!(Mountable::list(&*ws, Path::new("notes")).unwrap().len(), 2);
}

/// The number the kernel was given survives the move, so it keeps resolving —
/// the reason this is a rekey and not an eviction.
#[test]
fn rename_keeps_the_inode_the_kernel_is_holding() {
    let fs = adapter();
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();

    fs.rename_child(
        ROOT_INODE,
        OsStr::new("greeting.txt"),
        ROOT_INODE,
        OsStr::new("moved.txt"),
    )
    .unwrap();

    // The kernel still quotes `inode`; it has to keep working.
    assert_eq!(fs.stat_inode(inode).unwrap().size, CONTENT.len() as u64);
    // And a lookup of the new name lands on the same number, or the kernel would
    // see two inodes for one file.
    let (again, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("moved.txt"))
        .unwrap();
    assert_eq!(again, inode);
    assert!(matches!(
        fs.lookup_child(ROOT_INODE, OsStr::new("greeting.txt")),
        Err(CortexError::NotFound)
    ));
}

/// Moving a directory carries every descendant's number with it.
#[test]
fn rename_carries_a_whole_subtree() {
    let fs = adapter();
    let (sub, _) = fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap();
    fs.create_child(
        sub,
        OsStr::new("inner.txt"),
        OpenOptions {
            create_new: true,
            ..OpenOptions::read_write()
        },
    )
    .unwrap();
    let (inner, _) = fs.lookup_child(sub, OsStr::new("inner.txt")).unwrap();

    fs.rename_child(
        ROOT_INODE,
        OsStr::new("sub"),
        ROOT_INODE,
        OsStr::new("moved"),
    )
    .unwrap();

    assert_eq!(fs.stat_inode(sub).unwrap().kind, DirentKind::Dir);
    assert!(
        fs.stat_inode(inner).is_ok(),
        "a descendant's number must still resolve"
    );
    let (moved_dir, _) = fs.lookup_child(ROOT_INODE, OsStr::new("moved")).unwrap();
    assert_eq!(moved_dir, sub);
    assert_eq!(child_names(&fs, moved_dir), ["inner.txt"]);
}

/// A rename the backend refuses must leave the table exactly as it was — an
/// eager rekey would strand the numbers on paths that never changed.
#[test]
fn a_refused_rename_does_not_touch_the_table() {
    let fs = adapter();
    let (file, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();
    let (dir, _) = fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap();

    // file over directory: EISDIR, straight from the backend.
    assert!(matches!(
        fs.rename_child(
            ROOT_INODE,
            OsStr::new("greeting.txt"),
            ROOT_INODE,
            OsStr::new("sub")
        ),
        Err(CortexError::IsADirectory)
    ));

    assert_eq!(fs.path_of(file).unwrap(), Path::new("/greeting.txt"));
    assert_eq!(fs.path_of(dir).unwrap(), Path::new("/sub"));
    assert_eq!(child_names(&fs, ROOT_INODE), ["greeting.txt", "sub"]);
}

/// A table with `entries` interned, for driving `rekey_subtree` directly.
fn table(entries: &[(&str, u64)]) -> InodeTable {
    let mut t = InodeTable::new();
    for &(path, _) in entries {
        t.intern(PathBuf::from(path));
    }
    // Reading back the numbers it actually assigned; the fixtures' second
    // column is only there to name them in a failure message.
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
/// A backend refuses these anyway (`EINVAL` for a directory into its own
/// descendant, `ENOTEMPTY` for the reverse, a no-op for a self-rename), so the
/// guard is not what enforces the rule — it is what stops a *half*-rewritten
/// table if one ever slips through. Measured before the guard existed:
/// `/x → /x` and `/a/b → /a` each stripped `rev` down to the root, after which
/// the next `lookup` mints a second inode for a path `fwd` already knows.
#[test]
fn rekey_refuses_an_overlapping_move() {
    for (from, to, why) in [
        ("/x", "/x", "a path onto itself"),
        ("/a", "/a/b", "a directory into its own descendant"),
        ("/a/b", "/a", "a directory onto its own ancestor"),
        ("/", "/new", "the root, which every path starts with"),
    ] {
        let mut t = table(&[
            ("/x", 0),
            ("/x/y", 0),
            ("/a", 0),
            ("/a/b", 0),
            ("/a/b/c", 0),
        ]);
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

/// A sibling move is not overlapping, so the guard must let it through — a
/// guard that refused everything would pass the test above and be useless.
#[test]
fn rekey_allows_a_sibling_move() {
    let mut t = table(&[("/a/b", 0)]);
    let ino = t.intern(PathBuf::from("/a/b"));

    t.rekey_subtree(Path::new("/a/b"), Path::new("/a/c"));

    assert_eq!(t.path_of(ino).as_deref(), Some(Path::new("/a/c")));
    assert_eq!(t.rev.get(Path::new("/a/c")), Some(&ino));
    assert!(!t.rev.contains_key(Path::new("/a/b")));
}

/// The number survives the move. That is the whole reason this is a rekey and
/// not an eviction: the kernel goes on quoting the inode it was given, so
/// dropping the entry would turn its next `getattr` into `ESTALE`.
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

    // A later lookup of a moved path has to land on the same number, or the
    // kernel would see two inodes for one file.
    assert_eq!(t.rev.get(Path::new("/archive/a.md")), Some(&file));
    assert!(!t.rev.contains_key(Path::new("/notes/a.md")));

    // Nothing outside the subtree is disturbed.
    assert_eq!(t.path_of(ROOT_INODE).as_deref(), Some(Path::new("/")));
}

/// Renaming onto an existing entry: the destination's *name* mapping goes,
/// because whatever was there has been replaced, and the moved inode takes the
/// name over.
#[test]
fn rekey_hands_an_overwritten_destination_to_the_mover() {
    let mut t = InodeTable::new();
    let from = t.intern(PathBuf::from("/from.md"));
    let doomed = t.intern(PathBuf::from("/to.md"));
    assert_ne!(from, doomed);

    t.rekey_subtree(Path::new("/from.md"), Path::new("/to.md"));

    assert_eq!(
        t.rev.get(Path::new("/to.md")),
        Some(&from),
        "the name now resolves to the inode that moved onto it"
    );
    assert!(!t.rev.contains_key(Path::new("/from.md")));
    assert_eq!(t.path_of(from).as_deref(), Some(Path::new("/to.md")));
    // The replaced inode keeps its forward entry so an already-open handle can
    // still be `fstat`ed — the same reason `evict_path` keeps one.
    assert!(t.path_of(doomed).is_some());
}

/// `to.join("")` would append a separator. Path equality ignores a trailing
/// one, so it is cosmetic rather than wrong — pinned so the display stays clean.
#[test]
fn rekey_does_not_leave_a_trailing_separator() {
    let mut t = InodeTable::new();
    let ino = t.intern(PathBuf::from("/a"));

    t.rekey_subtree(Path::new("/a"), Path::new("/b"));

    assert_eq!(
        t.path_of(ino).unwrap().display().to_string(),
        "/b",
        "not \"/b/\""
    );
}

#[test]
fn missing_entries_report_not_found() {
    let fs = adapter();
    assert!(matches!(
        fs.lookup_child(ROOT_INODE, OsStr::new("nope")),
        Err(CortexError::NotFound)
    ));
    // An inode we never issued resolves to nothing.
    assert!(matches!(fs.stat_inode(9999), Err(CortexError::NotFound)));
}

#[test]
fn forget_evicts_but_spares_the_root() {
    let fs = adapter();
    let (inode, _) = fs
        .lookup_child(ROOT_INODE, OsStr::new("greeting.txt"))
        .unwrap();
    assert!(fs.stat_inode(inode).is_ok());

    // One lookup, one reference: forgetting it evicts the entry.
    fs.forget_inode(inode, 1);
    assert!(matches!(fs.stat_inode(inode), Err(CortexError::NotFound)));

    // The root survives any amount of forgetting; losing it would strand
    // every path that resolves through it.
    fs.forget_inode(ROOT_INODE, 1_000);
    assert!(fs.stat_inode(ROOT_INODE).is_ok());
}

#[test]
fn repeated_lookups_share_one_inode() {
    let fs = adapter();
    let (first, _) = fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap();
    let (second, _) = fs.lookup_child(ROOT_INODE, OsStr::new("sub")).unwrap();
    assert_eq!(first, second);

    // Two references now, so one forget leaves the inode live.
    fs.forget_inode(first, 1);
    assert!(fs.stat_inode(first).is_ok());
    fs.forget_inode(first, 1);
    assert!(matches!(fs.stat_inode(first), Err(CortexError::NotFound)));
}
