use super::*;
use std::sync::Arc;

use crate::{CortexError, InMemVolume, Workspace};

/// A `Dirent` cannot claim one kind and carry metadata saying another.
///
/// `kind` exists twice — once on the entry, once inside its [`Stat`] — so the only
/// thing keeping them honest is that `with_stat` derives one from the other. That is
/// why `stat` is not a public field: with one, `Dirent { kind: Dir, stat: <a file's>
/// }` would compile, and nothing would be wrong until a kernel believed it.
#[test]
fn an_entry_takes_its_kind_from_the_metadata_it_carries() {
    let carried = Dirent::with_stat("d", Stat::new(DirentKind::Dir, 0));
    assert_eq!(carried.kind, DirentKind::Dir);
    assert_eq!(
        carried.stat().expect("metadata was given").kind,
        carried.kind,
        "the entry's kind and its metadata's kind are the same fact"
    );

    // And an entry the listing knew nothing about says so, rather than inventing a
    // `Stat` whose fields would all be guesses.
    let bare = Dirent::new("f", DirentKind::File);
    assert_eq!(bare.kind, DirentKind::File);
    assert!(bare.stat().is_none());
}

/// A flag lands on an access mode without disturbing the others, so a caller can
/// say one thing at a time.
///
/// The asymmetry worth pinning is `create_new`. It is a constructor, because a
/// setter of that name would collide with it, so the combinations wanting `O_EXCL`
/// without both access modes are reached by narrowing rather than by setting.
#[test]
fn a_flag_lands_on_an_access_mode_without_disturbing_the_others() {
    let appending = OpenOptions::write_only().append(true);
    assert!(!appending.read && appending.write, "the mode survived");
    assert!(appending.append);
    assert!(
        !appending.truncate && !appending.create && !appending.create_new,
        "a setter answers for its own flag only"
    );

    let exclusive_write = OpenOptions::create_new().read(false);
    assert!(exclusive_write.create_new && exclusive_write.write);
    assert!(
        !exclusive_write.read,
        "narrowed off the half it did not want"
    );
}

/// What a read-only backend refuses on.
///
/// Five of the six flags mean modification, and the predicate lives here rather
/// than in each backend, where the fifth term is the easy one to leave out.
/// `create` counts even though it writes no bytes: it modifies the parent.
#[test]
fn every_flag_but_read_means_modification() {
    let ro = OpenOptions::read_only();
    assert!(!ro.intends_write());

    for (flag, options) in [
        ("write", ro.write(true)),
        ("append", ro.append(true)),
        ("truncate", ro.truncate(true)),
        ("create", ro.create(true)),
        ("create_new", OpenOptions::create_new()),
    ] {
        assert!(options.intends_write(), "{flag} means modification");
    }
}

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
