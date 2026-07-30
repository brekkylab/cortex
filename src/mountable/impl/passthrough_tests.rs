use super::*;
use crate::{FileExt, FileHandle};

/// Create a unique scratch directory under the system temp dir without
/// pulling in extra crates.
fn scratch(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "cortex-passthrough-test-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(vol: &dyn Mountable<Handle = fs::File>, path: &str) -> Vec<String> {
    let mut names: Vec<_> = vol
        .list(Path::new(path))
        .unwrap()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    names.sort();
    names
}

#[test]
fn stat_open_read_write() {
    let base = scratch("rwlu");
    let vol = PassthroughVolume::new(&base);

    vol.mkdir(Path::new("sub")).unwrap();
    fs::write(base.join("hello.txt"), b"world").unwrap();

    let st = vol.stat(Path::new("hello.txt")).unwrap();
    assert_eq!(st.kind, DirentKind::File);
    assert_eq!(st.size, 5);
    assert_eq!(vol.stat(Path::new("sub")).unwrap().kind, DirentKind::Dir);

    // Positioned read through the open handle.
    let (handle, _) = vol
        .open(Path::new("hello.txt"), OpenOptions::read_write())
        .unwrap();
    let mut buf = [0u8; 5];
    handle.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"world");

    // Positioned write, then truncate, both observable on disk.
    handle.write_all_at(b"HELLO", 0).unwrap();
    handle.truncate(3).unwrap();
    assert_eq!(fs::read(base.join("hello.txt")).unwrap(), b"HEL");

    assert_eq!(names(&vol, ""), vec!["hello.txt", "sub"]);

    vol.unlink(Path::new("hello.txt")).unwrap();
    assert!(matches!(
        vol.stat(Path::new("hello.txt")),
        Err(CortexError::NotFound)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn a_listing_reports_kinds_but_not_metadata() {
    let base = scratch("listing");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("sub")).unwrap();
    fs::write(base.join("f"), b"12345").unwrap();

    let entries = vol.list(Path::new("")).unwrap();
    let dir = entries.iter().find(|e| e.name == "sub").unwrap();
    let file = entries.iter().find(|e| e.name == "f").unwrap();

    // The kind rides along in the directory entry itself (`d_type`), so it
    // is free and always reported.
    assert_eq!(dir.kind, DirentKind::Dir);
    assert_eq!(file.kind, DirentKind::File);

    // The size is not: it would be an `lstat` per entry, which a plain `ls`
    // never asked for. A local listing therefore reports no metadata, and a
    // caller that wants it asks per entry — the opposite of an object store,
    // where the listing already contains it.
    assert!(dir.stat.is_none());
    assert!(file.stat.is_none());
    assert_eq!(vol.stat(Path::new("f")).unwrap().size, 5);

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn rename_defers_the_overwrite_contract_to_the_platform() {
    let base = scratch("rename");
    let vol = PassthroughVolume::new(&base);
    fs::write(base.join("a"), b"payload").unwrap();
    fs::create_dir(base.join("d")).unwrap();
    fs::create_dir(base.join("busy")).unwrap();
    fs::write(base.join("busy/occupied"), b"x").unwrap();

    // Moves, and the bytes come along.
    vol.rename(Path::new("a"), Path::new("b")).unwrap();
    assert_eq!(fs::read_to_string(base.join("b")).unwrap(), "payload");
    assert!(!base.join("a").exists());

    // The mismatched pairs, which arrive already classified.
    assert!(matches!(
        vol.rename(Path::new("b"), Path::new("d")),
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("b")),
        Err(CortexError::NotADirectory)
    ));
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("busy")),
        Err(CortexError::NotEmpty)
    ));
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("d/inner")),
        Err(CortexError::InvalidArgument)
    ));
    assert!(matches!(
        vol.rename(Path::new("nope"), Path::new("x")),
        Err(CortexError::NotFound)
    ));
    // A path that leaves the volume is refused before the OS is asked.
    assert!(matches!(
        vol.rename(Path::new("b"), Path::new("../escape")),
        Err(CortexError::InvalidName)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn unlink_takes_files_and_rmdir_takes_empty_directories() {
    let base = scratch("removal");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("dir")).unwrap();
    fs::write(base.join("dir/child"), b"x").unwrap();
    fs::write(base.join("file"), b"y").unwrap();

    assert!(matches!(
        vol.unlink(Path::new("dir")),
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.rmdir(Path::new("file")),
        Err(CortexError::NotADirectory)
    ));
    // The old implementation reached for `remove_dir_all` here, which would
    // have taken `child` with it. Nothing on disk may disappear.
    assert!(matches!(
        vol.rmdir(Path::new("dir")),
        Err(CortexError::NotEmpty)
    ));
    assert!(base.join("dir/child").exists());

    vol.unlink(Path::new("dir/child")).unwrap();
    vol.rmdir(Path::new("dir")).unwrap();
    assert!(!base.join("dir").exists());

    assert!(matches!(
        vol.rmdir(Path::new("dir")),
        Err(CortexError::NotFound)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn kind_errors() {
    let base = scratch("kind");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("dir")).unwrap();
    fs::write(base.join("file"), b"x").unwrap();

    assert!(matches!(
        vol.open(Path::new("dir"), OpenOptions::read_write()),
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.list(Path::new("file")),
        Err(CortexError::NotADirectory)
    ));
    assert!(matches!(
        vol.mkdir(Path::new("file")),
        Err(CortexError::AlreadyExists)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn escapes_and_missing_root_rejected() {
    let base = scratch("escape");
    let vol = PassthroughVolume::new(&base);
    assert!(matches!(
        vol.open(Path::new("../secret"), OpenOptions::read_write()),
        Err(CortexError::InvalidName)
    ));
    fs::remove_dir_all(&base).unwrap();

    // `new` doesn't touch disk, so a missing root only surfaces on use.
    let missing = scratch("missing");
    fs::remove_dir_all(&missing).unwrap();
    let vol = PassthroughVolume::new(&missing);
    assert!(matches!(
        vol.list(Path::new("")),
        Err(CortexError::NotFound)
    ));
}
