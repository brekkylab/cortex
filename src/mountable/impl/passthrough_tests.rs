use super::*;
use crate::test_support::scratch;
use crate::{FileExt, FileHandle};

async fn names(vol: &dyn Mountable<Handle = fs::File>, path: &str) -> Vec<String> {
    let mut names: Vec<_> = vol
        .list(Path::new(path)).await
        .unwrap()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn stat_open_read_write() {
    let base = scratch("passthrough", "rwlu");
    let vol = PassthroughVolume::new(&base);

    vol.mkdir(Path::new("sub")).await.unwrap();
    fs::write(base.join("hello.txt"), b"world").unwrap();

    let st = vol.stat(Path::new("hello.txt")).await.unwrap();
    assert_eq!(st.kind, DirentKind::File);
    assert_eq!(st.size, 5);
    assert_eq!(vol.stat(Path::new("sub")).await.unwrap().kind, DirentKind::Dir);

    let (handle, _) = vol
        .open(Path::new("hello.txt"), OpenOptions::read_write()).await
        .unwrap();
    let mut buf = [0u8; 5];
    handle.read_exact_at(&mut buf, 0).await.unwrap();
    assert_eq!(&buf, b"world");

    // Both observable on disk.
    handle.write_all_at(b"HELLO", 0).await.unwrap();
    handle.truncate(3).await.unwrap();
    assert_eq!(fs::read(base.join("hello.txt")).unwrap(), b"HEL");

    assert_eq!(names(&vol, "").await, vec!["hello.txt", "sub"]);

    vol.unlink(Path::new("hello.txt")).await.unwrap();
    assert!(matches!(
        vol.stat(Path::new("hello.txt")).await,
        Err(CortexError::NotFound)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn a_listing_reports_kinds_but_not_metadata() {
    let base = scratch("passthrough", "listing");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("sub")).await.unwrap();
    fs::write(base.join("f"), b"12345").unwrap();

    let entries = vol.list(Path::new("")).await.unwrap();
    let dir = entries.iter().find(|e| e.name == "sub").unwrap();
    let file = entries.iter().find(|e| e.name == "f").unwrap();

    // The kind rides along in `d_type`, so it is free and always reported.
    assert_eq!(dir.kind, DirentKind::Dir);
    assert_eq!(file.kind, DirentKind::File);

    // The size is not: that is an `lstat` per entry, which `ls` never asked for.
    // The opposite of an object store, whose listing already carries it.
    assert!(dir.stat().is_none());
    assert!(file.stat().is_none());
    assert_eq!(vol.stat(Path::new("f")).await.unwrap().size, 5);

    fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn rename_defers_the_overwrite_contract_to_the_platform() {
    let base = scratch("passthrough", "rename");
    let vol = PassthroughVolume::new(&base);
    fs::write(base.join("a"), b"payload").unwrap();
    fs::create_dir(base.join("d")).unwrap();
    fs::create_dir(base.join("busy")).unwrap();
    fs::write(base.join("busy/occupied"), b"x").unwrap();

    vol.rename(Path::new("a"), Path::new("b")).await.unwrap();
    assert_eq!(fs::read_to_string(base.join("b")).unwrap(), "payload");
    assert!(!base.join("a").exists());

    // The mismatched pairs arrive already classified.
    assert!(matches!(
        vol.rename(Path::new("b"), Path::new("d")).await,
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("b")).await,
        Err(CortexError::NotADirectory)
    ));
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("busy")).await,
        Err(CortexError::NotEmpty)
    ));
    assert!(matches!(
        vol.rename(Path::new("d"), Path::new("d/inner")).await,
        Err(CortexError::InvalidArgument)
    ));
    assert!(matches!(
        vol.rename(Path::new("nope"), Path::new("x")).await,
        Err(CortexError::NotFound)
    ));
    // Refused before the OS is asked.
    assert!(matches!(
        vol.rename(Path::new("b"), Path::new("../escape")).await,
        Err(CortexError::InvalidName)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn unlink_takes_files_and_rmdir_takes_empty_directories() {
    let base = scratch("passthrough", "removal");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("dir")).await.unwrap();
    fs::write(base.join("dir/child"), b"x").unwrap();
    fs::write(base.join("file"), b"y").unwrap();

    assert!(matches!(
        vol.unlink(Path::new("dir")).await,
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.rmdir(Path::new("file")).await,
        Err(CortexError::NotADirectory)
    ));
    // An earlier implementation reached for `remove_dir_all` here, taking `child`
    // with it.
    assert!(matches!(
        vol.rmdir(Path::new("dir")).await,
        Err(CortexError::NotEmpty)
    ));
    assert!(base.join("dir/child").exists());

    vol.unlink(Path::new("dir/child")).await.unwrap();
    vol.rmdir(Path::new("dir")).await.unwrap();
    assert!(!base.join("dir").exists());

    assert!(matches!(
        vol.rmdir(Path::new("dir")).await,
        Err(CortexError::NotFound)
    ));

    fs::remove_dir_all(&base).unwrap();
}

/// The three flavours it has to tell apart: a kind mismatch, a path that leaves
/// the root, and a root that is not there.
#[tokio::test]
async fn the_volume_refuses_what_it_cannot_serve() {
    let base = scratch("passthrough", "refuse");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("dir")).await.unwrap();
    fs::write(base.join("file"), b"x").unwrap();

    assert!(matches!(
        vol.open(Path::new("dir"), OpenOptions::read_write()).await,
        Err(CortexError::IsADirectory)
    ));
    assert!(matches!(
        vol.list(Path::new("file")).await,
        Err(CortexError::NotADirectory)
    ));
    assert!(matches!(
        vol.mkdir(Path::new("file")).await,
        Err(CortexError::AlreadyExists)
    ));
    // A *name* error, not whatever lies outside the root.
    assert!(matches!(
        vol.open(Path::new("../secret"), OpenOptions::read_write()).await,
        Err(CortexError::InvalidName)
    ));
    fs::remove_dir_all(&base).unwrap();

    // `new` doesn't touch disk, so a missing root only surfaces on use.
    let missing = scratch("passthrough", "missing");
    fs::remove_dir_all(&missing).unwrap();
    let vol = PassthroughVolume::new(&missing);
    assert!(matches!(
        vol.list(Path::new("")).await,
        Err(CortexError::NotFound)
    ));
}
