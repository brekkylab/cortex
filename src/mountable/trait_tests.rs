use super::*;
use std::sync::Arc;

use crate::{CortexError, InMemVolume, Workspace};

/// Stands in for a read-only source whose author never writes a `rename`. Its
/// whole job is to pin what those get for free, so it must never grow an override.
struct ReadOnlyStub;

impl Mountable for ReadOnlyStub {
    type Handle = Box<dyn FileHandle>;

    fn stat(&self, _: &Path) -> Result<Stat> {
        Ok(Stat::new(DirentKind::Dir, 0))
    }
    fn list(&self, _: &Path) -> Result<Vec<Dirent>> {
        Ok(Vec::new())
    }
    fn mkdir(&self, _: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }
    fn unlink(&self, _: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }
    fn rmdir(&self, _: &Path) -> Result<()> {
        Err(CortexError::ReadOnly)
    }
    fn open(&self, _: &Path, _: OpenOptions) -> Result<(Self::Handle, Stat)> {
        Err(CortexError::ReadOnly)
    }
}

/// `EROFS`, per-request — unlike the `ENOSYS` a kernel applies to the whole mount.
#[test]
fn a_backend_that_does_not_write_refuses_rename_without_implementing_it() {
    let backend = ReadOnlyStub;

    assert!(matches!(
        Mountable::rename(&backend, Path::new("a"), Path::new("b")),
        Err(CortexError::ReadOnly)
    ));
    // Through the erased face, which a mount table stores it as.
    assert!(matches!(
        DynMountable::rename(&backend, Path::new("a"), Path::new("b")),
        Err(CortexError::ReadOnly)
    ));
    // And through a shared handle, which is how one store feeds two consumers.
    assert!(matches!(
        Mountable::rename(&Arc::new(ReadOnlyStub), Path::new("a"), Path::new("b")),
        Err(CortexError::ReadOnly)
    ));
}

/// One store, several owners, through both faces. Necessary because every
/// consumer takes its backend *by value* and offers no way back, so without this
/// impl a store feeds exactly one consumer.
#[test]
fn an_arc_backend_is_a_backend_and_shares_one_store() {
    let vol = Arc::new(InMemVolume::new());

    let (handle, _) = Mountable::open(&vol, Path::new("shared.txt"), OpenOptions::create_new())
        .expect("fresh volume");
    handle.write_all_at(b"once", 0).unwrap();

    // Read back through a *different* clone: one store, not a copy per owner.
    let other = Arc::clone(&vol);
    let (handle, stat) = Mountable::open(&other, Path::new("shared.txt"), OpenOptions::read_only())
        .expect("every clone sees the same store");
    assert_eq!(stat.size, 4);
    let mut buf = [0u8; 4];
    handle.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"once");

    // The blanket `DynMountable` impl reaches `Arc<T>` too, and the workspace
    // *shares* the store: a write bypassing it is still visible through it.
    Mountable::mkdir(&vol, Path::new("dir")).unwrap();
    let ws = Workspace::new()
        .try_with_mount("", Arc::clone(&vol))
        .expect("Arc<InMemVolume> erases to DynMountable via the blanket impl");
    assert_eq!(Mountable::list(&ws, Path::new("dir")).unwrap().len(), 0);

    Mountable::mkdir(&vol, Path::new("dir/deeper")).unwrap();
    assert_eq!(Mountable::list(&ws, Path::new("dir")).unwrap().len(), 1);
}
