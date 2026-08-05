use super::*;
use std::time::UNIX_EPOCH;

fn mtime_of(vol: &InMemVolume, path: &str) -> SystemTime {
    vol.stat(Path::new(path))
        .unwrap()
        .mtime
        .expect("every entry reports an mtime")
}

#[test]
fn a_new_entry_reports_a_real_timestamp() {
    let started = SystemTime::now();
    let vol = InMemVolume::new();
    let (_, stat) = vol.open(Path::new("f"), OpenOptions::create_new()).unwrap();
    vol.mkdir(Path::new("d")).unwrap();

    for (what, mtime) in [
        ("file (from open)", stat.mtime),
        ("file (from stat)", vol.stat(Path::new("f")).unwrap().mtime),
        ("directory", vol.stat(Path::new("d")).unwrap().mtime),
        ("root", vol.stat(Path::new("")).unwrap().mtime),
    ] {
        let mtime = mtime.unwrap_or_else(|| panic!("{what} has no mtime"));
        assert_ne!(mtime, UNIX_EPOCH, "{what} fell back to the epoch");
        assert!(mtime >= started, "{what} predates the volume");
    }

    // A listing carries each entry's own timestamp — free, since it already locks
    // every child for its kind, and an N+1 otherwise.
    for entry in vol.list(Path::new("")).unwrap() {
        let stat = entry
            .stat()
            .expect("the in-memory listing carries metadata");
        assert!(
            stat.mtime.is_some_and(|m| m >= started),
            "{} has no usable mtime",
            entry.name
        );
    }
}

/// Whichever route it arrives by. The file-side counterpart of
/// [`changing_a_directorys_children_advances_its_mtime`].
#[test]
fn changing_a_files_contents_advances_its_mtime() {
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("f"), OpenOptions::create_new()).unwrap();

    // Not `after > before`: two `now()` calls can land on the same tick. "At or
    // after the moment it began" is the real contract, and needs no sleep.
    let began = SystemTime::now();
    handle.write_all_at(b"hello", 0).unwrap();
    let stat = vol.stat(Path::new("f")).unwrap();
    assert_eq!(stat.size, 5);
    assert!(
        stat.mtime.unwrap() >= began,
        "a write must stamp at or after it started"
    );

    let began = SystemTime::now();
    handle.truncate(2).unwrap();
    let stat = vol.stat(Path::new("f")).unwrap();
    assert_eq!(stat.size, 2);
    assert!(stat.mtime.unwrap() >= began, "an explicit truncate");

    // `O_TRUNC` rides the open, so it is a second path to the same resize — and
    // the metadata handed back already has to reflect it.
    let began = SystemTime::now();
    let (_, stat) = vol
        .open(Path::new("f"), OpenOptions::read_write().truncate(true))
        .unwrap();
    assert_eq!(stat.size, 0);
    assert!(stat.mtime.unwrap() >= began, "truncation through the open");
}

#[test]
fn changing_a_directorys_children_advances_its_mtime() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();

    let began = SystemTime::now();
    vol.mkdir(Path::new("d/sub")).unwrap();
    assert!(mtime_of(&vol, "d") >= began, "mkdir of a child");

    let began = SystemTime::now();
    vol.open(Path::new("d/f"), OpenOptions::create_new())
        .unwrap();
    assert!(mtime_of(&vol, "d") >= began, "creating a file in it");

    let began = SystemTime::now();
    vol.unlink(Path::new("d/f")).unwrap();
    assert!(mtime_of(&vol, "d") >= began, "unlink of a child");

    let began = SystemTime::now();
    vol.rmdir(Path::new("d/sub")).unwrap();
    assert!(mtime_of(&vol, "d") >= began, "rmdir of a child");
}

#[test]
fn a_childs_write_leaves_the_parent_directory_alone() {
    // POSIX: only adding or removing a *name* touches the directory.
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    let (handle, _) = vol
        .open(Path::new("d/f"), OpenOptions::create_new())
        .unwrap();

    let before = mtime_of(&vol, "d");
    handle.write_all_at(b"payload", 0).unwrap();
    assert_eq!(mtime_of(&vol, "d"), before);
}

#[test]
fn reading_and_listing_leave_timestamps_alone() {
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("f"), OpenOptions::create_new()).unwrap();
    handle.write_all_at(b"hello", 0).unwrap();
    vol.mkdir(Path::new("d")).unwrap();

    let (file, dir) = (mtime_of(&vol, "f"), mtime_of(&vol, "d"));

    let mut buf = [0u8; 5];
    handle.read_exact_at(&mut buf, 0).unwrap();
    vol.list(Path::new("")).unwrap();
    vol.stat(Path::new("f")).unwrap();

    assert_eq!(mtime_of(&vol, "f"), file, "a read is not a modification");
    assert_eq!(mtime_of(&vol, "d"), dir, "nor is a listing");
}

#[test]
fn created_is_set_once_and_does_not_move() {
    let vol = InMemVolume::new();
    let (handle, stat) = vol.open(Path::new("f"), OpenOptions::create_new()).unwrap();
    let born = stat.created.expect("a new file records its birth time");

    handle.write_all_at(b"hello", 0).unwrap();
    handle.truncate(1).unwrap();

    let after = vol.stat(Path::new("f")).unwrap();
    assert_eq!(after.created, Some(born), "birth time is not modified");
    assert!(after.mtime.unwrap() >= born, "but mtime has moved on");
}

/// Every row measured against real `fs::rename` first, so this backend answers
/// what a local one does. A caller that has to branch per backend has no contract.
#[test]
fn rename_follows_the_overwrite_table() {
    let file = |vol: &InMemVolume, p: &str| {
        vol.open(Path::new(p), OpenOptions::create_new()).unwrap();
    };

    // file -> absent: moves
    let vol = InMemVolume::new();
    file(&vol, "a");
    assert!(vol.rename(Path::new("a"), Path::new("b")).is_ok());
    assert!(vol.stat(Path::new("a")).is_err());
    assert_eq!(vol.stat(Path::new("b")).unwrap().kind, DirentKind::File);

    // file -> file: replaces, silently
    let vol = InMemVolume::new();
    let (src, _) = vol.open(Path::new("a"), OpenOptions::create_new()).unwrap();
    src.write_all_at(b"new", 0).unwrap();
    file(&vol, "b");
    assert!(vol.rename(Path::new("a"), Path::new("b")).is_ok());
    assert_eq!(vol.stat(Path::new("b")).unwrap().size, 3);

    // file -> directory: EISDIR
    let vol = InMemVolume::new();
    file(&vol, "a");
    vol.mkdir(Path::new("d")).unwrap();
    assert!(matches!(
        vol.rename(Path::new("a"), Path::new("d")),
        Err(CortexError::IsADirectory)
    ));

    // directory -> absent: moves, with its contents
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    file(&vol, "d/inside");
    assert!(vol.rename(Path::new("d"), Path::new("moved")).is_ok());
    assert_eq!(
        vol.stat(Path::new("moved/inside")).unwrap().kind,
        DirentKind::File
    );

    // directory -> empty directory: replaces
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    file(&vol, "d/inside");
    vol.mkdir(Path::new("empty")).unwrap();
    assert!(vol.rename(Path::new("d"), Path::new("empty")).is_ok());
    assert!(vol.stat(Path::new("empty/inside")).is_ok());

    // directory -> non-empty directory: ENOTEMPTY
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    vol.mkdir(Path::new("busy")).unwrap();
    file(&vol, "busy/occupied");
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("busy")),
        Err(CortexError::NotEmpty)
    ));

    // directory -> file: ENOTDIR
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    file(&vol, "f");
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("f")),
        Err(CortexError::NotADirectory)
    ));

    // onto itself: succeeds without destroying anything
    let vol = InMemVolume::new();
    file(&vol, "a");
    vol.mkdir(Path::new("d")).unwrap();
    assert!(vol.rename(Path::new("a"), Path::new("a")).is_ok());
    assert!(vol.stat(Path::new("a")).is_ok(), "still there");
    assert!(vol.rename(Path::new("d"), Path::new("d")).is_ok());
    assert!(vol.stat(Path::new("d")).is_ok());

    // directory -> its own descendant: EINVAL, or the subtree detaches entirely.
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("d/inner")),
        Err(CortexError::InvalidArgument)
    ));
    vol.mkdir(Path::new("d/inner")).unwrap();
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("d/inner")),
        Err(CortexError::InvalidArgument)
    ));

    // source absent, and destination's parent absent: both ENOENT
    let vol = InMemVolume::new();
    file(&vol, "a");
    assert!(matches!(
        vol.rename(Path::new("nope"), Path::new("b")),
        Err(CortexError::NotFound)
    ));
    assert!(matches!(
        vol.rename(Path::new("a"), Path::new("nodir/b")),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn rename_touches_both_directories_but_not_the_file() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("from")).unwrap();
    vol.mkdir(Path::new("to")).unwrap();
    let (handle, _) = vol
        .open(Path::new("from/f"), OpenOptions::create_new())
        .unwrap();
    handle.write_all_at(b"payload", 0).unwrap();
    let file_mtime = mtime_of(&vol, "from/f");

    let began = SystemTime::now();
    vol.rename(Path::new("from/f"), Path::new("to/f")).unwrap();

    assert!(
        mtime_of(&vol, "from") >= began,
        "a name left this directory"
    );
    assert!(mtime_of(&vol, "to") >= began, "and arrived in this one");
    assert_eq!(
        mtime_of(&vol, "to/f"),
        file_mtime,
        "POSIX: moving a file does not modify its contents"
    );
}

#[test]
fn an_open_handle_survives_a_rename() {
    // The handle shares the body, not the place in the tree — as an open fd does.
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("a"), OpenOptions::create_new()).unwrap();
    handle.write_all_at(b"kept", 0).unwrap();

    vol.rename(Path::new("a"), Path::new("b")).unwrap();

    let mut buf = [0u8; 4];
    handle.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"kept");
    // ...and a write through it still lands in the file under its new name.
    handle.write_all_at(b"MORE", 4).unwrap();
    assert_eq!(vol.stat(Path::new("b")).unwrap().size, 8);
}

fn names(vol: &InMemVolume, path: &str) -> Vec<String> {
    let mut names: Vec<_> = vol
        .list(Path::new(path))
        .unwrap()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    names.sort();
    names
}

/// Create a file and fill it with `data` in one step.
fn write_file(vol: &InMemVolume, path: &str, data: &[u8]) {
    let (handle, _) = vol
        .open(Path::new(path), OpenOptions::create_new())
        .unwrap();
    handle.write_all_at(data, 0).unwrap();
}

fn read_file(vol: &InMemVolume, path: &str) -> Vec<u8> {
    let (handle, stat) = vol.open(Path::new(path), OpenOptions::read_only()).unwrap();
    let mut buf = vec![0u8; stat.size as usize];
    handle.read_exact_at(&mut buf, 0).unwrap();
    buf
}

#[test]
fn positioned_writes_and_truncate_are_shared() {
    let vol = InMemVolume::new();
    write_file(&vol, "/f", b"world");

    // Both handles share the tree's buffer, so `stat` sees the new size.
    let (a, _) = vol
        .open(Path::new("/f"), OpenOptions::read_write())
        .unwrap();
    let (b, _) = vol
        .open(Path::new("/f"), OpenOptions::read_write())
        .unwrap();
    a.write_all_at(b"HELLO", 0).unwrap();
    let mut buf = [0u8; 5];
    b.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"HELLO");

    // Growth zero-fills.
    a.write_all_at(b"!", 6).unwrap();
    assert_eq!(read_file(&vol, "/f"), b"HELLO\0!");

    a.truncate(3).unwrap();
    assert_eq!(vol.stat(Path::new("/f")).unwrap().size, 3);
    assert_eq!(read_file(&vol, "/f"), b"HEL");
}

#[test]
fn open_options_pin_the_creation_contract() {
    let vol = InMemVolume::new();
    let rw = OpenOptions::read_write();
    let create = rw.create(true);
    let create_new = OpenOptions::create_new().create(true);

    // Metadata comes back with the handle: a FUSE `create` must answer with both
    // in one message, and a follow-up `stat` is a round trip plus a window for the
    // entry to be replaced underneath.
    let (handle, stat) = vol.open(Path::new("/f"), create).unwrap();
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, 0);
    handle.write_all_at(b"hello", 0).unwrap();

    // `create` on something that already exists simply opens it.
    let (_, stat) = vol.open(Path::new("/f"), create).unwrap();
    assert_eq!(stat.size, 5);

    // The exclusive one, and the backend's job: `stat`-then-create is a race, and
    // for a remote store the atomic form (a conditional PUT) is the only one there
    // is. Hence options travelling with the call.
    assert!(matches!(
        vol.open(Path::new("/f"), create_new),
        Err(CortexError::AlreadyExists)
    ));

    // At open time, before the handle exists.
    let (_, stat) = vol.open(Path::new("/f"), rw.truncate(true)).unwrap();
    assert_eq!(stat.size, 0);

    // A missing parent is never created implicitly.
    assert!(matches!(
        vol.open(Path::new("/missing/f"), create_new),
        Err(CortexError::NotFound)
    ));

    // Without `create`, `open` opens only what is already there.
    assert!(matches!(
        vol.open(Path::new("/nope"), rw),
        Err(CortexError::NotFound)
    ));

    // Both spellings, not one for symmetry: `create` inspects the child it would
    // have made, the plain open navigates to the entry. Two branches, two
    // `IsADirectory`s — a coverage run is what proved they are not one line.
    vol.mkdir(Path::new("/d")).unwrap();
    assert!(matches!(
        vol.open(Path::new("/d"), create),
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.open(Path::new("/d"), rw),
        Err(CortexError::IsADirectory)
    ));

    // `O_RDONLY | O_CREAT` is legal POSIX and stays legal here.
    let (_, stat) = vol
        .open(Path::new("/ro"), OpenOptions::read_only().create(true))
        .unwrap();
    assert_eq!(stat.size, 0);

    // Asking for neither read nor write leaves nothing the handle can do.
    assert!(matches!(
        vol.open(Path::new("/f"), OpenOptions::default()),
        Err(CortexError::InvalidArgument)
    ));
}

#[test]
fn absurd_offsets_are_refused_rather_than_allocated() {
    let vol = InMemVolume::new();
    let (handle, _) = vol
        .open(Path::new("/f"), OpenOptions::create_new())
        .unwrap();
    handle.write_all_at(b"keep", 0).unwrap();

    // virtio-fs caps the byte *count* but passes the guest's `offset` through, so
    // the offset is attacker-controlled. Growing to meet it aborts the host, and
    // there is no `catch_unwind` between here and the virtio-fs worker.
    let err = handle.write_at(b"x", 1 << 45).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);

    // Near the top of the range, `offset + len` itself wraps.
    let err = handle.write_at(b"x", u64::MAX).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);

    // `truncate` reaches the same `resize`.
    assert!(matches!(
        handle.truncate(1 << 45),
        Err(CortexError::FileTooLarge)
    ));

    // A rejected call leaves the file as it was, and one inside the limit works.
    assert_eq!(vol.stat(Path::new("/f")).unwrap().size, 4);
    assert_eq!(read_file(&vol, "/f"), b"keep");
    handle.write_all_at(b"!", 4).unwrap();
    assert_eq!(read_file(&vol, "/f"), b"keep!");
}

#[test]
fn unlink_takes_files_and_rmdir_takes_empty_directories() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("/dir")).unwrap();
    write_file(&vol, "/dir/child", b"x");
    write_file(&vol, "/file", b"y");

    // Each refuses the other's kind, as the two syscalls do.
    assert!(matches!(
        vol.unlink(Path::new("/dir")),
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.rmdir(Path::new("/file")),
        Err(CortexError::NotADirectory)
    ));
    // A directory with children is never discarded implicitly.
    assert!(matches!(
        vol.rmdir(Path::new("/dir")),
        Err(CortexError::NotEmpty)
    ));
    assert_eq!(names(&vol, "/dir"), vec!["child"]);

    // The `rm -rf` sequence a kernel actually sends.
    vol.unlink(Path::new("/dir/child")).unwrap();
    vol.rmdir(Path::new("/dir")).unwrap();
    assert!(matches!(
        vol.stat(Path::new("/dir")),
        Err(CortexError::NotFound)
    ));

    assert!(matches!(
        vol.rmdir(Path::new("/dir")),
        Err(CortexError::NotFound)
    ));
}

/// The kind mismatches an `open` refuses live in
/// [`open_options_pin_the_creation_contract`], where the option combinations are.
#[test]
fn the_namespace_operations_classify_what_they_refuse() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("/dir")).unwrap();
    write_file(&vol, "/file", b"x");

    assert!(matches!(
        vol.list(Path::new("/file")),
        Err(CortexError::NotADirectory)
    ));
    assert!(matches!(
        vol.mkdir(Path::new("/file")),
        Err(CortexError::AlreadyExists)
    ));
    assert!(matches!(
        vol.stat(Path::new("/nope")),
        Err(CortexError::NotFound)
    ));
    // `..` is a *name* error, not a missing path: refused before any lookup.
    assert!(matches!(
        vol.mkdir(Path::new("/a/../b")),
        Err(CortexError::InvalidName)
    ));
}

/// An `append` handle writes at the end whatever offset it is given, which is
/// what `OpenOptions::append` promises and what the kernel does for
/// `PassthroughVolume`.
#[test]
fn append_writes_land_at_the_end() {
    let vol = InMemVolume::new();
    let append = OpenOptions::read_write().append(true);
    let (h, _) = vol.open(Path::new("log"), append.create(true)).unwrap();

    h.write_all_at(b"AAA", 0).unwrap();
    h.write_all_at(b"BBB", 0).unwrap();

    let mut buf = [0u8; 6];
    h.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"AAABBB");
}

/// The access mode outlives the open, as it does for a file descriptor.
#[test]
fn a_handle_refuses_what_its_open_did_not_allow() {
    let vol = InMemVolume::new();
    vol.open(Path::new("f"), OpenOptions::create_new()).unwrap();

    let (ro, _) = vol.open(Path::new("f"), OpenOptions::read_only()).unwrap();
    assert_eq!(ro.write_at(b"x", 0).unwrap_err().raw_os_error(), Some(9));
    assert!(matches!(ro.truncate(0), Err(CortexError::InvalidArgument)));

    let (wo, _) = vol.open(Path::new("f"), OpenOptions::write_only()).unwrap();
    assert_eq!(
        wo.read_at(&mut [0u8; 1], 0).unwrap_err().raw_os_error(),
        Some(9)
    );
}
