use super::*;
use std::time::UNIX_EPOCH;

/// The options every timestamp test opens with.
fn fresh() -> OpenOptions {
    OpenOptions {
        create_new: true,
        ..OpenOptions::read_write()
    }
}

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
    let (_, stat) = vol.open(Path::new("f"), fresh()).unwrap();
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
}

#[test]
fn writing_advances_the_files_mtime() {
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("f"), fresh()).unwrap();

    // Not `after > before`: two `SystemTime::now()` calls can land on the same
    // tick. Stamping at or after the moment the write began is the real
    // contract, and it holds without sleeping.
    let began = SystemTime::now();
    handle.write_all_at(b"hello", 0).unwrap();
    let stat = vol.stat(Path::new("f")).unwrap();

    assert_eq!(stat.size, 5);
    assert!(
        stat.mtime.unwrap() >= began,
        "a write must stamp at or after it started"
    );
}

#[test]
fn truncating_advances_the_files_mtime() {
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("f"), fresh()).unwrap();
    handle.write_all_at(b"hello", 0).unwrap();

    let began = SystemTime::now();
    handle.truncate(2).unwrap();

    let stat = vol.stat(Path::new("f")).unwrap();
    assert_eq!(stat.size, 2);
    assert!(stat.mtime.unwrap() >= began);
}

#[test]
fn truncating_through_the_open_advances_it_too() {
    // `O_TRUNC` rides the open rather than arriving as a separate call, so it
    // is a second path to the same resize.
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("f"), fresh()).unwrap();
    handle.write_all_at(b"hello", 0).unwrap();

    let began = SystemTime::now();
    let (_, stat) = vol
        .open(
            Path::new("f"),
            OpenOptions {
                truncate: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();

    assert_eq!(stat.size, 0);
    assert!(stat.mtime.unwrap() >= began);
}

#[test]
fn changing_a_directorys_children_advances_its_mtime() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();

    // Each mutation is checked against a timestamp taken just before it.
    let began = SystemTime::now();
    vol.mkdir(Path::new("d/sub")).unwrap();
    assert!(mtime_of(&vol, "d") >= began, "mkdir of a child");

    let began = SystemTime::now();
    vol.open(Path::new("d/f"), fresh()).unwrap();
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
    // POSIX: writing a file's *contents* does not touch its directory. Only
    // adding or removing a name does.
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("d")).unwrap();
    let (handle, _) = vol.open(Path::new("d/f"), fresh()).unwrap();

    let before = mtime_of(&vol, "d");
    handle.write_all_at(b"payload", 0).unwrap();
    assert_eq!(mtime_of(&vol, "d"), before);
}

#[test]
fn reading_and_listing_leave_timestamps_alone() {
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("f"), fresh()).unwrap();
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
    let (handle, stat) = vol.open(Path::new("f"), fresh()).unwrap();
    let born = stat.created.expect("a new file records its birth time");

    handle.write_all_at(b"hello", 0).unwrap();
    handle.truncate(1).unwrap();

    let after = vol.stat(Path::new("f")).unwrap();
    assert_eq!(after.created, Some(born), "birth time is not modified");
    assert!(after.mtime.unwrap() >= born, "but mtime has moved on");
}

#[test]
fn a_listing_carries_each_entrys_timestamp() {
    // The listing already locks each child to learn its kind, so the timestamp
    // is free there — and a `readdirplus` that had to re-`stat` every name
    // would be an N+1 round trip.
    let vol = InMemVolume::new();
    vol.open(Path::new("f"), fresh()).unwrap();
    vol.mkdir(Path::new("d")).unwrap();

    for entry in vol.list(Path::new("")).unwrap() {
        let stat = entry.stat.expect("the in-memory listing carries metadata");
        assert!(
            stat.mtime.is_some_and(|m| m != UNIX_EPOCH),
            "{} has no usable mtime",
            entry.name
        );
    }
}

/// Every row of the overwrite table, measured against real `fs::rename` on
/// macOS first so this backend answers what a local one already does. A caller
/// that has to branch on which backend it is talking to has no contract.
#[test]
fn rename_follows_the_overwrite_table() {
    let file = |vol: &InMemVolume, p: &str| {
        vol.open(Path::new(p), fresh()).unwrap();
    };

    // file -> absent: moves
    let vol = InMemVolume::new();
    file(&vol, "a");
    assert!(vol.rename(Path::new("a"), Path::new("b")).is_ok());
    assert!(vol.stat(Path::new("a")).is_err());
    assert_eq!(vol.stat(Path::new("b")).unwrap().kind, DirentKind::File);

    // file -> file: replaces, silently
    let vol = InMemVolume::new();
    let (src, _) = vol.open(Path::new("a"), fresh()).unwrap();
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

    // directory -> its own descendant: EINVAL. Allowing it would detach the
    // subtree from the tree entirely.
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
    let (handle, _) = vol.open(Path::new("from/f"), fresh()).unwrap();
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
    // The handle shares the file's body, not its place in the tree — the same
    // deal an open fd gets from a `mv`.
    let vol = InMemVolume::new();
    let (handle, _) = vol.open(Path::new("a"), fresh()).unwrap();
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
        .open(
            Path::new(path),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
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
fn create_read_list_unlink() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("/sub")).unwrap();
    write_file(&vol, "/hello.txt", b"world");
    write_file(&vol, "/sub/inner", b"hi");

    assert_eq!(read_file(&vol, "/hello.txt"), b"world");
    assert_eq!(read_file(&vol, "/sub/inner"), b"hi");
    assert_eq!(names(&vol, "/"), vec!["hello.txt", "sub"]);
    assert_eq!(names(&vol, "/sub"), vec!["inner"]);

    vol.unlink(Path::new("/hello.txt")).unwrap();
    assert!(matches!(
        vol.open(Path::new("/hello.txt"), OpenOptions::read_write()),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn positioned_writes_and_truncate_are_shared() {
    let vol = InMemVolume::new();
    write_file(&vol, "/f", b"world");

    // A second handle sees writes made through the first, and both share the
    // tree's buffer, so `stat` reflects the new size.
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

    // Growth zero-fills; a positioned write past EOF extends the file.
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
    let create = OpenOptions { create: true, ..rw };
    let create_new = OpenOptions {
        create_new: true,
        ..create
    };

    // `open` reports the entry's metadata in the same call that hands out
    // the handle: a FUSE `create` must answer with attributes *and* an `fh`
    // in one message, and a follow-up `stat` would be both a second backend
    // round trip and a window for the entry to be replaced underneath.
    let (handle, stat) = vol.open(Path::new("/f"), create).unwrap();
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, 0);
    handle.write_all_at(b"hello", 0).unwrap();

    // `create` on something that already exists simply opens it.
    let (_, stat) = vol.open(Path::new("/f"), create).unwrap();
    assert_eq!(stat.size, 5);

    // `create_new` is the exclusive one. This check has to belong to the
    // backend: decomposing it into `stat`-then-create would be a race, and
    // for a remote backend the atomic form is the only one that exists
    // (a conditional PUT, `O_EXCL`) — which is why the options travel with
    // the call instead of `create` being a separate operation.
    assert!(matches!(
        vol.open(Path::new("/f"), create_new),
        Err(CortexError::AlreadyExists)
    ));

    // Truncation happens at open time, before the handle exists, so the
    // metadata reported back already reflects it.
    let (_, stat) = vol
        .open(
            Path::new("/f"),
            OpenOptions {
                truncate: true,
                ..rw
            },
        )
        .unwrap();
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

    // A directory never becomes a file handle, whatever the options ask.
    vol.mkdir(Path::new("/d")).unwrap();
    assert!(matches!(
        vol.open(Path::new("/d"), create),
        Err(CortexError::IsADirectory)
    ));

    // `O_RDONLY | O_CREAT` is legal POSIX and stays legal here.
    let (_, stat) = vol
        .open(
            Path::new("/ro"),
            OpenOptions {
                create: true,
                ..OpenOptions::read_only()
            },
        )
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
        .open(
            Path::new("/f"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            },
        )
        .unwrap();
    handle.write_all_at(b"keep", 0).unwrap();

    // virtio-fs caps the byte *count* at 1 MiB but passes the guest's
    // `offset` through untouched, so the offset is attacker-controlled.
    // Growing the buffer to meet it would abort the host process, and there
    // is no `catch_unwind` between here and the virtio-fs worker thread.
    let err = handle.write_at(b"x", 1 << 45).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);

    // Near the top of the range the `offset + len` addition itself wraps.
    let err = handle.write_at(b"x", u64::MAX).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);

    // `truncate` reaches the same `resize`, so it needs the same guard.
    assert!(matches!(
        handle.truncate(1 << 45),
        Err(CortexError::FileTooLarge)
    ));

    // A rejected call leaves the file exactly as it was.
    assert_eq!(vol.stat(Path::new("/f")).unwrap().size, 4);
    assert_eq!(read_file(&vol, "/f"), b"keep");

    // A write that lands inside the limit still works.
    handle.write_all_at(b"!", 4).unwrap();
    assert_eq!(read_file(&vol, "/f"), b"keep!");
}

#[test]
fn unlink_takes_files_and_rmdir_takes_empty_directories() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("/dir")).unwrap();
    write_file(&vol, "/dir/child", b"x");
    write_file(&vol, "/file", b"y");

    // Each call refuses the other's kind, exactly as the two syscalls do.
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

    // The `rm -rf` sequence a kernel actually sends: empty it, then drop it.
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

#[test]
fn kind_errors() {
    let vol = InMemVolume::new();
    vol.mkdir(Path::new("/dir")).unwrap();
    write_file(&vol, "/file", b"x");

    assert!(matches!(
        vol.open(Path::new("/dir"), OpenOptions::read_write()),
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.list(Path::new("/file")),
        Err(CortexError::NotADirectory)
    ));
    assert!(matches!(
        vol.mkdir(Path::new("/file")),
        Err(CortexError::AlreadyExists)
    ));
    assert!(matches!(
        vol.open(
            Path::new("/file"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            }
        ),
        Err(CortexError::AlreadyExists)
    ));
}

#[test]
fn missing_and_invalid_paths_rejected() {
    let vol = InMemVolume::new();
    assert!(matches!(
        vol.stat(Path::new("/nope")),
        Err(CortexError::NotFound)
    ));
    assert!(matches!(
        vol.open(
            Path::new("/missing/deep"),
            OpenOptions {
                create_new: true,
                ..OpenOptions::read_write()
            }
        ),
        Err(CortexError::NotFound)
    ));
    assert!(matches!(
        vol.mkdir(Path::new("/a/../b")),
        Err(CortexError::InvalidName)
    ));
}
