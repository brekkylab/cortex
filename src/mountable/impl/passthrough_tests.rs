use super::*;
use crate::test_support::scratch;
use crate::{FileExt, FileHandle};

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
    let base = scratch("passthrough", "rwlu");
    let vol = PassthroughVolume::new(&base);

    vol.mkdir(Path::new("sub")).unwrap();
    fs::write(base.join("hello.txt"), b"world").unwrap();

    let st = vol.stat(Path::new("hello.txt")).unwrap();
    assert_eq!(st.kind, DirentKind::File);
    assert_eq!(st.size, 5);
    assert_eq!(vol.stat(Path::new("sub")).unwrap().kind, DirentKind::Dir);

    let (handle, _) = vol
        .open(Path::new("hello.txt"), OpenOptions::read_write())
        .unwrap();
    let mut buf = [0u8; 5];
    handle.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"world");

    // Both observable on disk.
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
    let base = scratch("passthrough", "listing");
    let vol = PassthroughVolume::new(&base);
    vol.mkdir(Path::new("sub")).unwrap();
    fs::write(base.join("f"), b"12345").unwrap();

    let entries = vol.list(Path::new("")).unwrap();
    let dir = entries.iter().find(|e| e.name == "sub").unwrap();
    let file = entries.iter().find(|e| e.name == "f").unwrap();

    // The kind rides along in `d_type`, so it is free and always reported.
    assert_eq!(dir.kind, DirentKind::Dir);
    assert_eq!(file.kind, DirentKind::File);

    // The size is not: that is an `lstat` per entry, which `ls` never asked for.
    // The opposite of an object store, whose listing already carries it.
    assert!(dir.stat().is_none());
    assert!(file.stat().is_none());
    assert_eq!(vol.stat(Path::new("f")).unwrap().size, 5);

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn rename_defers_the_overwrite_contract_to_the_platform() {
    let base = scratch("passthrough", "rename");
    let vol = PassthroughVolume::new(&base);
    fs::write(base.join("a"), b"payload").unwrap();
    fs::create_dir(base.join("d")).unwrap();
    fs::create_dir(base.join("busy")).unwrap();
    fs::write(base.join("busy/occupied"), b"x").unwrap();

    vol.rename(Path::new("a"), Path::new("b")).unwrap();
    assert_eq!(fs::read_to_string(base.join("b")).unwrap(), "payload");
    assert!(!base.join("a").exists());

    // The mismatched pairs arrive already classified.
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
    // Refused before the OS is asked.
    assert!(matches!(
        vol.rename(Path::new("b"), Path::new("../escape")),
        Err(CortexError::InvalidName)
    ));

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn unlink_takes_files_and_rmdir_takes_empty_directories() {
    let base = scratch("passthrough", "removal");
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
    // An earlier implementation reached for `remove_dir_all` here, taking `child`
    // with it.
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

/// The three flavours it has to tell apart: a kind mismatch, a path that leaves
/// the root, and a root that is not there.
#[test]
fn the_volume_refuses_what_it_cannot_serve() {
    let base = scratch("passthrough", "refuse");
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
    // A *name* error, not whatever lies outside the root.
    assert!(matches!(
        vol.open(Path::new("../secret"), OpenOptions::read_write()),
        Err(CortexError::InvalidName)
    ));
    fs::remove_dir_all(&base).unwrap();

    // `new` doesn't touch disk, so a missing root only surfaces on use.
    let missing = scratch("passthrough", "missing");
    fs::remove_dir_all(&missing).unwrap();
    let vol = PassthroughVolume::new(&missing);
    assert!(matches!(
        vol.list(Path::new("")),
        Err(CortexError::NotFound)
    ));
}

#[cfg(unix)]
fn link(target: impl AsRef<Path>, at: impl AsRef<Path>) {
    std::os::unix::fs::symlink(target.as_ref(), at.as_ref()).unwrap();
}

/// The containment property, by every route a link offers out of the root.
#[test]
#[cfg(unix)]
fn a_link_cannot_take_a_request_out_of_the_root() {
    let base = scratch("passthrough", "escape");
    let (root, outside) = (base.join("root"), base.join("outside"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"OUT").unwrap();
    fs::write(root.join("real.txt"), b"IN").unwrap();

    link(outside.join("secret.txt"), root.join("onleaf"));
    link(&outside, root.join("inmiddle"));
    // Dangling: `exists()` follows the link and so answers false, which is how
    // a parent-only check lets a creating `open` write on the far side.
    link(outside.join("new.txt"), root.join("dangling"));

    let vol = PassthroughVolume::new(&root);
    for escaping in ["onleaf", "inmiddle/secret.txt", "dangling"] {
        assert!(
            matches!(vol.stat(Path::new(escaping)), Err(CortexError::NotFound)),
            "{escaping} escaped"
        );
        assert!(matches!(
            vol.open(Path::new(escaping), OpenOptions::create_new()),
            Err(CortexError::NotFound)
        ));
    }
    assert!(!outside.join("new.txt").exists(), "a create wrote outside");

    // None of it reached the far side.
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"OUT");

    fs::remove_dir_all(&base).unwrap();
}

/// Refusing to *traverse* a link out of the root is not refusing to admit it is
/// there. The name is an entry in a directory this volume owns, and `unlink`
/// takes it out of that directory without touching what it points at — so the
/// listing has to show it, or a caller meets a `NotEmpty` with nothing in the
/// listing to account for it and no way to clear it.
#[test]
#[cfg(unix)]
fn a_link_out_of_the_root_is_listed_and_can_be_removed() {
    let base = scratch("passthrough", "escapelist");
    let (root, outside) = (base.join("root"), base.join("outside"));
    fs::create_dir_all(root.join(".venv/bin")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("python3.12"), b"BINARY").unwrap();
    // What `python -m venv` leaves behind, and pnpm after it.
    link(outside.join("python3.12"), root.join(".venv/bin/python"));

    let vol = PassthroughVolume::new(&root);

    // Reported as `File`: what it points at is outside, so it is not asked.
    let listed = vol.list(Path::new(".venv/bin")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "python");
    assert_eq!(listed[0].kind, DirentKind::File);

    // Still not traversable — the target is no more reachable than before.
    assert!(matches!(
        vol.stat(Path::new(".venv/bin/python")),
        Err(CortexError::NotFound)
    ));

    // And the directory can be emptied, which is the whole point.
    vol.unlink(Path::new(".venv/bin/python")).unwrap();
    vol.rmdir(Path::new(".venv/bin")).unwrap();
    vol.rmdir(Path::new(".venv")).unwrap();
    assert!(!root.join(".venv").exists());

    // The link went; what it pointed at did not.
    assert_eq!(fs::read(outside.join("python3.12")).unwrap(), b"BINARY");

    fs::remove_dir_all(&base).unwrap();
}

/// A link *inside* the root, to make the same point from the other side: the
/// entry goes and the file it named stays. `stat` follows and `unlink` does not,
/// so this is the assertion that stops a later tidy-up making them agree.
#[test]
#[cfg(unix)]
fn unlink_removes_the_link_and_not_what_it_points_at() {
    let base = scratch("passthrough", "unlinklink");
    fs::create_dir_all(base.join("sub")).unwrap();
    fs::write(base.join("sub/target.txt"), b"KEEP").unwrap();
    link("sub/target.txt", base.join("lnk"));
    let vol = PassthroughVolume::new(&base);

    assert_eq!(vol.stat(Path::new("lnk")).unwrap().size, 4); // follows
    vol.unlink(Path::new("lnk")).unwrap(); // does not
    assert!(fs::symlink_metadata(base.join("lnk")).is_err());
    assert_eq!(fs::read(base.join("sub/target.txt")).unwrap(), b"KEEP");

    // A directory link, where following would have meant `IsADirectory`.
    link("sub", base.join("dirlnk"));
    vol.unlink(Path::new("dirlnk")).unwrap();
    assert!(base.join("sub").is_dir());

    fs::remove_dir_all(&base).unwrap();
}

/// The parent is what an entry operation has to have contained, and a link
/// standing in for a *directory* mid-path is still a way out of the root.
#[test]
#[cfg(unix)]
fn an_entry_operation_cannot_reach_through_a_link_out_of_the_root() {
    let base = scratch("passthrough", "entryescape");
    let (root, outside) = (base.join("root"), base.join("outside"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"OUT").unwrap();
    fs::write(root.join("bait.txt"), b"IN").unwrap();
    link(&outside, root.join("inmiddle"));

    let vol = PassthroughVolume::new(&root);
    let victim = Path::new("inmiddle/secret.txt");
    assert!(matches!(vol.unlink(victim), Err(CortexError::NotFound)));
    assert!(matches!(vol.mkdir(victim), Err(CortexError::NotFound)));
    assert!(matches!(vol.rmdir(victim), Err(CortexError::NotFound)));
    assert!(matches!(
        vol.rename(Path::new("bait.txt"), victim),
        Err(CortexError::NotFound)
    ));
    assert!(matches!(
        vol.rename(victim, Path::new("bait.txt")),
        Err(CortexError::NotFound)
    ));
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"OUT");

    fs::remove_dir_all(&base).unwrap();
}

/// A link that stays inside is followed, so what `stat` and `list` report is
/// what `open` will hand back.
#[test]
#[cfg(unix)]
fn a_link_inside_the_root_is_followed() {
    let base = scratch("passthrough", "inlink");
    fs::create_dir_all(base.join("sub")).unwrap();
    fs::write(base.join("sub/inner.txt"), b"INNER").unwrap();
    link("sub/inner.txt", base.join("tofile"));
    link("sub", base.join("todir"));
    let vol = PassthroughVolume::new(&base);

    // An `lstat` answers 13 — the link string's own length — and a kernel that
    // believes it truncates every read of the file to it, reporting success.
    assert_eq!(vol.stat(Path::new("tofile")).unwrap().size, 5);
    let (handle, opened) = vol
        .open(Path::new("tofile"), OpenOptions::read_only())
        .unwrap();
    assert_eq!(opened.size, 5);
    let mut buf = vec![0u8; 5];
    handle.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"INNER");

    // `d_type` answers DT_LNK and never DT_DIR, so a listing's kind has to come
    // from the target or a caller is handed a file it cannot descend into.
    let entries = vol.list(Path::new("")).unwrap();
    let listed = entries.iter().find(|e| e.name == "todir").unwrap();
    assert_eq!(listed.kind, DirentKind::Dir);
    assert_eq!(names(&vol, "todir"), vec!["inner.txt"]);

    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn list_comes_back_in_a_stable_order() {
    let base = scratch("passthrough", "order");
    let vol = PassthroughVolume::new(&base);
    for name in [
        "zebra", "yak", "xray", "walrus", "viper", "umbrella", "tiger", "snake",
    ] {
        fs::write(base.join(name), b"x").unwrap();
    }

    // Not through `names`, which sorts before comparing — that is why every
    // other test here is blind to order.
    let entries = vol.list(Path::new("")).unwrap();
    let listed: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        listed,
        [
            "snake", "tiger", "umbrella", "viper", "walrus", "xray", "yak", "zebra"
        ]
    );

    fs::remove_dir_all(&base).unwrap();
}

/// The root is the one path a containment walk must not need: asking where it
/// lands would resolve it, and resolving needs it to be there. So a volume can
/// still make the directory it was anchored at.
#[test]
fn a_volume_can_make_the_root_it_was_anchored_at() {
    let base = scratch("passthrough", "mkroot");
    fs::remove_dir_all(&base).unwrap();
    let vol = PassthroughVolume::new(&base);

    vol.mkdir(Path::new("")).unwrap();
    assert!(base.is_dir());

    fs::remove_dir_all(&base).unwrap();
}

/// A link spelling its target absolutely, in the root's own *uncanonical*
/// spelling — which on macOS every scratch path has, `/var` being a link to
/// `/private/var`. Resolving component by component walks through that one too,
/// where comparing the target's spelling against the root's would not.
#[test]
#[cfg(unix)]
fn a_link_to_an_absolute_path_inside_the_root_is_followed() {
    let base = scratch("passthrough", "abslink");
    fs::create_dir_all(base.join("sub")).unwrap();
    fs::write(base.join("sub/inner.txt"), b"INNER").unwrap();
    link(base.join("sub/inner.txt"), base.join("abs"));
    let vol = PassthroughVolume::new(&base);

    assert_eq!(vol.stat(Path::new("abs")).unwrap().size, 5);
    assert!(names(&vol, "").contains(&"abs".to_string()));

    fs::remove_dir_all(&base).unwrap();
}

/// `..` that stays inside is an ordinary request, and folding it is what
/// [`Workspace`](crate::Workspace) already does to the same spelling. One that
/// would climb past the root stays refused — `the_volume_refuses_what_it_cannot_serve`
/// holds that line.
#[test]
fn a_dotdot_that_stays_inside_the_root_is_folded_rather_than_refused() {
    let base = scratch("passthrough", "fold");
    fs::create_dir_all(base.join("sub")).unwrap();
    fs::write(base.join("sub/inner.txt"), b"INNER").unwrap();
    let vol = PassthroughVolume::new(&base);

    let folding = Path::new("sub/../sub/inner.txt");
    assert_eq!(vol.stat(folding).unwrap().size, 5);
    let (handle, _) = vol.open(folding, OpenOptions::read_only()).unwrap();
    let mut buf = [0u8; 5];
    handle.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"INNER");

    fs::remove_dir_all(&base).unwrap();
}

/// The fold is lexical, so a `..` behind a directory link returns to the link's
/// own parent rather than to the target's, where the kernel would go.
///
/// Pinned because the difference reads as a bug, and closing it would break what
/// it is holding up: [`Workspace`](crate::Workspace) routes on a normalized key
/// and hands this backend the folded remainder, so folding the kernel's way here
/// would answer one way called directly and another way through the mount table.
/// Moving to the kernel's `..` is `Workspace::normalize`'s question first.
#[test]
#[cfg(unix)]
fn a_dotdot_behind_a_directory_link_folds_lexically_rather_than_as_the_kernel_would() {
    let base = scratch("passthrough", "linkdotdot");
    fs::create_dir_all(base.join("sub/deep")).unwrap();
    fs::write(base.join("sub/beside.txt"), b"BESIDE").unwrap();
    fs::write(base.join("atroot.txt"), b"ROOT").unwrap();
    link("sub/deep", base.join("deeplink"));
    let vol = PassthroughVolume::new(&base);

    // The kernel would read `sub/beside.txt` here, `deeplink/..` being `sub`.
    assert!(matches!(
        vol.stat(Path::new("deeplink/../beside.txt")),
        Err(CortexError::NotFound)
    ));
    // It lands at the root instead, where the fold takes it.
    assert_eq!(
        vol.stat(Path::new("deeplink/../atroot.txt")).unwrap().size,
        4
    );

    fs::remove_dir_all(&base).unwrap();
}

/// A `..` the request never spelled: it arrives inside a link's target, and there
/// it applies to what is already *resolved* — the target's parent, where the
/// kernel applies it — not to the folded request that never left the root.
#[test]
#[cfg(unix)]
fn a_dotdot_in_a_link_target_cannot_climb_out_of_the_root() {
    let base = scratch("passthrough", "targetdotdot");
    let (root, outside) = (base.join("root"), base.join("outside"));
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"OUT").unwrap();
    link("sub/../../outside/secret.txt", root.join("climb"));

    let vol = PassthroughVolume::new(&root);
    assert!(matches!(
        vol.stat(Path::new("climb")),
        Err(CortexError::NotFound)
    ));
    // Listed, as any other entry is; being unable to traverse it is a different
    // answer from its not being there.
    assert_eq!(names(&vol, ""), vec!["climb", "sub"]);
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"OUT");

    fs::remove_dir_all(&base).unwrap();
}

/// A cycle has no answer to give, so the walk has to stop having one rather than
/// keep asking. Reading links instead of asking the kernel to resolve them means
/// owning the ceiling the kernel was applying.
#[test]
#[cfg(unix)]
fn a_link_that_loops_is_refused_rather_than_walked_forever() {
    let base = scratch("passthrough", "loop");
    link("b", base.join("a"));
    link("a", base.join("b"));
    let vol = PassthroughVolume::new(&base);

    assert!(vol.stat(Path::new("a")).is_err());
    assert!(vol.list(Path::new("")).is_ok());

    fs::remove_dir_all(&base).unwrap();
}

/// A link that stays inside the root and names something not there yet. Where it
/// points is what containment is about, and that is inside; whether the target
/// exists is the OS's business, and POSIX has a creating `open` make it.
#[test]
#[cfg(unix)]
fn a_create_through_a_contained_link_reaches_its_target() {
    let base = scratch("passthrough", "pending");
    fs::create_dir_all(base.join("sub")).unwrap();
    link("sub/new.txt", base.join("pending"));
    let vol = PassthroughVolume::new(&base);

    let creating = OpenOptions {
        create: true,
        ..OpenOptions::read_write()
    };
    let (handle, _) = vol.open(Path::new("pending"), creating).unwrap();
    handle.write_all_at(b"MADE", 0).unwrap();
    assert_eq!(fs::read(base.join("sub/new.txt")).unwrap(), b"MADE");

    fs::remove_dir_all(&base).unwrap();
}

/// And in a listing. Omitting it would claim the name is not there over a
/// question containment never asked; `DirentKind` has no `Symlink`, so `File` is
/// what is left to say.
#[test]
#[cfg(unix)]
fn a_link_that_dangles_inside_the_root_is_listed_as_a_file() {
    let base = scratch("passthrough", "danglelist");
    link("absent.txt", base.join("dangle"));
    let vol = PassthroughVolume::new(&base);

    let entries = vol.list(Path::new("")).unwrap();
    let listed = entries.iter().find(|entry| entry.name == "dangle").unwrap();
    assert_eq!(listed.kind, DirentKind::File);

    fs::remove_dir_all(&base).unwrap();
}
