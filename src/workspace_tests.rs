use super::*;
use crate::{FileExt, InMemVolume, PassthroughVolume};
use std::fs;

/// A workspace whose mount table is all this test cares about. The backends
/// are never consulted, so the cheapest one will do.
fn table(paths: &[&str]) -> Workspace {
    paths.iter().fold(Workspace::new(), |ws, p| {
        ws.try_with_mount(p, InMemVolume::new()).unwrap()
    })
}

fn children_of(ws: &Workspace, at: &str) -> Vec<(String, bool)> {
    ws.mount_children(Path::new(at)).into_iter().collect()
}

#[test]
fn mount_children_yields_the_next_component_and_flags_exact_mounts() {
    let ws = table(&["a/b/c", "a/x", "z"]);

    assert_eq!(
        children_of(&ws, ""),
        [("a".into(), false), ("z".into(), true)],
        "only the first component, and `a` is merely on the way to a mount"
    );
    assert_eq!(
        children_of(&ws, "a"),
        [("b".into(), false), ("x".into(), true)]
    );
    assert_eq!(children_of(&ws, "a/b"), [("c".into(), true)]);
    assert!(
        children_of(&ws, "z").is_empty(),
        "a mount is not its own child"
    );
    assert!(children_of(&ws, "q").is_empty());
}

#[test]
fn spans_mounts_counts_only_strict_descendants() {
    let ws = table(&["a/b/c", "a/x", "z"]);

    assert!(ws.spans_mounts(Path::new("")));
    assert!(ws.spans_mounts(Path::new("a")));
    assert!(ws.spans_mounts(Path::new("a/b")));

    // The bound is `Excluded`, so a mount does not span itself. Were it
    // `Included`, a backend that answered `NotFound` at its own root would be
    // papered over with a fabricated directory.
    assert!(!ws.spans_mounts(Path::new("z")));
    assert!(!ws.spans_mounts(Path::new("a/x")));
    assert!(!ws.spans_mounts(Path::new("a/b/c")));
    assert!(!ws.spans_mounts(Path::new("q")));
}

#[test]
fn a_rootless_workspace_has_a_root_directory() {
    let ws = table(&["s3-prod", "notion", "gdrive"]);

    assert_eq!(
        Mountable::stat(&ws, Path::new("")).unwrap().kind,
        DirentKind::Dir,
        "the directory holding the mounts has to exist, or nothing can be mounted"
    );
    // A mount point itself is still answered by its backend.
    assert_eq!(
        Mountable::stat(&ws, Path::new("notion")).unwrap().kind,
        DirentKind::Dir
    );
    // Neither a mount nor on the way to one.
    assert!(matches!(
        Mountable::stat(&ws, Path::new("nope")),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn every_ancestor_of_a_deep_mount_is_a_directory() {
    let ws = table(&["a/b/c"]);

    for on_the_way in ["", "a", "a/b"] {
        assert_eq!(
            Mountable::stat(&ws, Path::new(on_the_way)).unwrap().kind,
            DirentKind::Dir,
            "{on_the_way:?} leads to a mount"
        );
    }
    assert!(matches!(
        Mountable::stat(&ws, Path::new("a/x")),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn an_empty_workspace_is_an_empty_directory_not_a_missing_one() {
    let ws = Workspace::new();

    assert_eq!(
        Mountable::stat(&ws, Path::new("")).unwrap().kind,
        DirentKind::Dir,
        "a freshly mounted tmpfs is empty, not absent"
    );
    assert_eq!(
        listing(&ws, ""),
        [],
        "and it lists nothing rather than failing"
    );
    assert!(matches!(
        Mountable::stat(&ws, Path::new("anything")),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn a_backend_that_does_not_know_a_path_on_the_way_to_a_mount_is_overridden() {
    // The case a "synthesize only when routing fails" implementation misses:
    // routing *succeeds* here — the root backend claims `data` — and it is the
    // backend that answers `NotFound`.
    let ws = Workspace::new()
        .try_with_mount("", InMemVolume::new())
        .unwrap()
        .try_with_mount("data/raw", InMemVolume::new())
        .unwrap();

    assert!(
        matches!(
            Mountable::stat(&InMemVolume::new(), Path::new("data")),
            Err(CortexError::NotFound)
        ),
        "precondition: a fresh in-memory volume has no `data`"
    );
    assert_eq!(
        Mountable::stat(&ws, Path::new("data")).unwrap().kind,
        DirentKind::Dir
    );
}

#[test]
fn a_broken_root_backend_is_not_hidden_behind_a_synthesized_root() {
    // A passthrough pointing at a directory that does not exist. Nothing lies
    // below the root, so there is no mount table to synthesize from and the
    // misconfiguration must surface.
    let missing = scratch("gone");
    fs::remove_dir_all(&missing).unwrap();
    let ws = Workspace::new()
        .try_with_mount("", PassthroughVolume::new(&missing))
        .unwrap();

    assert!(matches!(
        Mountable::stat(&ws, Path::new("")),
        Err(CortexError::NotFound)
    ));
}

fn listing(ws: &Workspace, at: &str) -> Vec<(String, DirentKind, bool)> {
    Mountable::list(ws, Path::new(at))
        .unwrap()
        .into_iter()
        .map(|d| (d.name, d.kind, d.stat.is_some()))
        .collect()
}

#[test]
fn a_rootless_workspace_lists_its_mount_points() {
    let ws = table(&["s3-prod", "notion", "gdrive"]);

    assert_eq!(
        listing(&ws, ""),
        [
            ("gdrive".into(), DirentKind::Dir, false),
            ("notion".into(), DirentKind::Dir, false),
            ("s3-prod".into(), DirentKind::Dir, false),
        ]
    );
}

#[test]
fn each_step_toward_a_deep_mount_lists_the_next_component() {
    let ws = table(&["a/b/c"]);

    assert_eq!(listing(&ws, ""), [("a".into(), DirentKind::Dir, false)]);
    assert_eq!(listing(&ws, "a"), [("b".into(), DirentKind::Dir, false)]);
    assert_eq!(listing(&ws, "a/b"), [("c".into(), DirentKind::Dir, false)]);
}

#[test]
fn a_name_merely_leading_to_a_mount_keeps_the_backends_entry() {
    // `shared` exists in the root backend *and* leads to a mount at
    // `shared/extra`. No mount sits at `shared` itself, so the backend keeps
    // the name — and its entry carries real metadata the synthesized one
    // cannot, which is the observable difference.
    let root = InMemVolume::new();
    Mountable::mkdir(&root, Path::new("shared")).unwrap();
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data/raw", InMemVolume::new())
        .unwrap()
        .try_with_mount("shared/extra", InMemVolume::new())
        .unwrap();

    assert_eq!(
        listing(&ws, ""),
        [
            // Mount-derived names come first, sorted, so their positions do not
            // move when the backend's contents churn.
            ("data".into(), DirentKind::Dir, false),
            ("shared".into(), DirentKind::Dir, true),
        ],
        "`shared` appears once, and it is the backend's entry — it has a stat"
    );
}

#[test]
fn a_mount_shadows_a_backend_entry_of_the_same_name() {
    // The topology `apply_krun` recommends: a root backend for inode 1 with
    // other mounts on top. Here the root backend holds a *file* named `data`
    // and a directory is mounted over it.
    let root = InMemVolume::new();
    let (file, _) = Mountable::open(
        &root,
        Path::new("data"),
        OpenOptions {
            create_new: true,
            ..OpenOptions::read_write()
        },
    )
    .unwrap();
    drop(file);
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data", InMemVolume::new())
        .unwrap();

    assert_eq!(
        listing(&ws, ""),
        [("data".into(), DirentKind::Dir, false)],
        "readdir must not report a file where getattr reports a directory"
    );
    assert_eq!(
        Mountable::stat(&ws, Path::new("data")).unwrap().kind,
        DirentKind::Dir,
        "and this is the answer readdir has to agree with"
    );
}

#[test]
fn backend_entries_that_are_not_mount_paths_come_after_the_mount_derived_ones() {
    let root = InMemVolume::new();
    for name in ["aaa", "zzz"] {
        Mountable::mkdir(&root, Path::new(name)).unwrap();
    }
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("mmm", InMemVolume::new())
        .unwrap();

    let names: Vec<_> = listing(&ws, "").into_iter().map(|(n, ..)| n).collect();
    assert_eq!(
        names[0], "mmm",
        "the mount holds position 1 regardless of how the backend sorts, so \
         backend churn cannot shift it out of a readdir window"
    );
    assert_eq!(names.len(), 3);
}

#[test]
fn a_list_error_other_than_not_found_is_not_papered_over() {
    // The root backend holds a *file* at `data` while `data/raw` is mounted
    // below it. `list("data")` reaches the file and must surface ENOTDIR rather
    // than pretending `data` is a directory it can enumerate.
    let root = InMemVolume::new();
    let (file, _) = Mountable::open(
        &root,
        Path::new("data"),
        OpenOptions {
            create_new: true,
            ..OpenOptions::read_write()
        },
    )
    .unwrap();
    drop(file);
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data/raw", InMemVolume::new())
        .unwrap();

    assert!(matches!(
        Mountable::list(&ws, Path::new("data")),
        Err(CortexError::NotADirectory)
    ));
    // The asymmetry this leaves is deliberate and recorded: `stat` answers
    // `File` while `list` answers ENOTDIR, so the mount below is unreachable.
    // A workspace configured this way is misconfigured, and it says so.
    assert_eq!(
        Mountable::stat(&ws, Path::new("data")).unwrap().kind,
        DirentKind::File
    );
    assert_eq!(
        Mountable::stat(&ws, Path::new("data/raw")).unwrap().kind,
        DirentKind::Dir
    );
}

/// What the root backend holds at `holder`, in a workspace with a mount at
/// `holder/inner`.
///
/// Each variant is chosen so the *backend* would answer the mutation with `Ok`
/// if it were asked — which is what makes checking the mount table first
/// load-bearing rather than decorative.
enum Holder {
    /// An empty directory: `rmdir` would succeed on it.
    EmptyDir,
    /// A real file: `unlink` would delete it.
    File,
    /// Nothing: `mkdir` would create one, and `open(create)` a file.
    Absent,
}

fn shadowing_backend(holder: Holder) -> Workspace {
    let root = InMemVolume::new();
    match holder {
        Holder::EmptyDir => Mountable::mkdir(&root, Path::new("holder")).unwrap(),
        Holder::File => {
            Mountable::open(
                &root,
                Path::new("holder"),
                OpenOptions {
                    create_new: true,
                    ..OpenOptions::read_write()
                },
            )
            .unwrap();
        }
        Holder::Absent => {}
    }
    Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("holder/inner", InMemVolume::new())
        .unwrap()
}

/// Four mutations aimed at a directory that exists only because a mount lies
/// below it. Each is refused *before* the backend is consulted, and each row's
/// backend would otherwise have answered `Ok` — so without the pre-flight check
/// every one of these detaches the mount.
#[test]
fn a_mutation_aimed_at_the_mount_table_is_refused_before_the_backend_is_asked() {
    type Op = fn(&Workspace, &Path) -> Result<()>;

    let rmdir: Op = |ws, path| Mountable::rmdir(ws, path);
    let unlink: Op = |ws, path| Mountable::unlink(ws, path);
    let mkdir: Op = |ws, path| Mountable::mkdir(ws, path);
    let create: Op = |ws, path| {
        Mountable::open(
            ws,
            path,
            OpenOptions {
                create: true,
                ..OpenOptions::read_write()
            },
        )
        .map(|_| ())
    };

    for (holder, op, expected, damage) in [
        (
            Holder::EmptyDir,
            rmdir,
            CortexError::NotEmpty,
            "removes a directory the mount table still populates, and evicts \
             inode mappings the kernel may still hold",
        ),
        (
            Holder::File,
            unlink,
            CortexError::IsADirectory,
            "deletes the mount's parent",
        ),
        (
            Holder::Absent,
            mkdir,
            CortexError::AlreadyExists,
            "succeeds where POSIX requires EEXIST, the path already being a \
             directory",
        ),
        (
            Holder::Absent,
            create,
            CortexError::IsADirectory,
            "creates a file, after which the kernel answers ENOTDIR for \
             everything under it and the mount is unreachable",
        ),
    ] {
        let ws = shadowing_backend(holder);
        let got = op(&ws, Path::new("holder"));
        assert!(
            std::mem::discriminant(got.as_ref().unwrap_err()) == std::mem::discriminant(&expected),
            "expected {expected:?}, got {got:?} — unguarded this {damage}"
        );
        // And in every case the mount below is still reachable.
        assert_eq!(
            Mountable::stat(&ws, Path::new("holder/inner"))
                .unwrap()
                .kind,
            DirentKind::Dir,
            "the mount was disturbed: {damage}"
        );
    }
}

#[test]
fn creating_in_the_synthesized_namespace_is_read_only_not_missing() {
    // Nothing is mounted at the root, so there is no backend to create in. The
    // root itself just answered `stat` as a directory and lists three entries,
    // so `NotFound` would be a plain lie — and the message a user sees
    // ("No such file or directory") reads as a filesystem bug.
    let ws = table(&["s3-prod", "notion", "gdrive"]);

    assert!(matches!(
        Mountable::mkdir(&ws, Path::new("newthing")),
        Err(CortexError::ReadOnly)
    ));
    assert!(matches!(
        Mountable::open(
            &ws,
            Path::new("newthing"),
            OpenOptions {
                create: true,
                ..OpenOptions::read_write()
            }
        ),
        Err(CortexError::ReadOnly)
    ));

    // Narrow on purpose: a path whose parent is *also* unrouted is a genuine
    // missing-component case, which POSIX answers with ENOENT.
    assert!(matches!(
        Mountable::mkdir(&ws, Path::new("nowhere/deeper")),
        Err(CortexError::NotFound)
    ));
    // And a real mount still takes writes.
    assert!(Mountable::mkdir(&ws, Path::new("notion/fresh")).is_ok());
}

#[test]
fn a_mount_path_that_is_not_utf8_is_refused() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // A listing name is a `String`, so a non-UTF-8 component could only be
    // reported lossily — and a lossy name does not round-trip, giving an entry
    // that appears in `list` but whose `lookup` fails. Refused where it enters
    // instead, since this is where mount names first become visible.
    let bad = PathBuf::from(OsString::from_vec(vec![0x66, 0xFF, 0x6F]));
    let mut ws = Workspace::new();
    assert!(matches!(
        ws.mount(&bad, InMemVolume::new()),
        Err(CortexError::InvalidName)
    ));
    assert!(matches!(
        Workspace::new().try_with_mount(&bad, InMemVolume::new()),
        Err(CortexError::InvalidName)
    ));
}

#[test]
fn a_workspace_mounted_inside_a_workspace_serves_through_both() {
    // The module doc has claimed this since the beginning; synthesized roots are
    // what make it true, because the inner workspace has to answer for its own
    // root before the outer one can route into it.
    let inner = Workspace::new()
        .try_with_mount("leaf", InMemVolume::new())
        .unwrap();
    let outer = Workspace::new().try_with_mount("nested", inner).unwrap();

    assert_eq!(
        Mountable::stat(&outer, Path::new("nested")).unwrap().kind,
        DirentKind::Dir
    );
    assert_eq!(
        listing(&outer, "nested"),
        [("leaf".into(), DirentKind::Dir, false)]
    );
    assert_eq!(
        Mountable::stat(&outer, Path::new("nested/leaf"))
            .unwrap()
            .kind,
        DirentKind::Dir
    );

    // A write reaches the innermost backend through both hops.
    Mountable::mkdir(&outer, Path::new("nested/leaf/deep")).unwrap();
    assert_eq!(
        Mountable::stat(&outer, Path::new("nested/leaf/deep"))
            .unwrap()
            .kind,
        DirentKind::Dir
    );
}

#[test]
fn a_rename_inside_one_mount_reaches_its_backend() {
    let ws = table(&["work"]);
    Mountable::open(
        &ws,
        Path::new("work/a"),
        OpenOptions {
            create_new: true,
            ..OpenOptions::read_write()
        },
    )
    .unwrap();

    Mountable::rename(&ws, Path::new("work/a"), Path::new("work/b")).unwrap();

    assert!(matches!(
        Mountable::stat(&ws, Path::new("work/a")),
        Err(CortexError::NotFound)
    ));
    assert_eq!(
        Mountable::stat(&ws, Path::new("work/b")).unwrap().kind,
        DirentKind::File
    );
}

#[test]
fn a_rename_across_two_mounts_is_a_cross_device_move() {
    // One kernel mount, two backends. The kernel cannot see the workspace's own
    // mount table, so it sends the rename here and this layer has to say that
    // an in-place move is impossible — as `EXDEV`, the answer `mv` knows how to
    // recover from by copying and deleting instead.
    let ws = table(&["notion", "s3"]);
    Mountable::open(
        &ws,
        Path::new("notion/draft.md"),
        OpenOptions {
            create_new: true,
            ..OpenOptions::read_write()
        },
    )
    .unwrap();

    assert!(matches!(
        Mountable::rename(&ws, Path::new("notion/draft.md"), Path::new("s3/draft.md")),
        Err(CortexError::CrossDevice)
    ));
    // Nothing moved.
    assert!(Mountable::stat(&ws, Path::new("notion/draft.md")).is_ok());
}

#[test]
fn the_mount_table_is_not_the_filesystems_to_rearrange() {
    // `s3-prod` and `notion` are mount points; `a` exists only because `a/b/c`
    // lies below it. Moving either would mean rewriting the mount table through
    // a file operation.
    let ws = table(&["s3-prod", "notion", "a/b/c"]);

    for (from, to, what) in [
        ("s3-prod", "archive", "a mount point as the source"),
        ("notion", "s3-prod", "a mount point as the destination"),
        ("a", "b", "a synthesized directory as the source"),
        (
            "s3-prod/f",
            "a",
            "a synthesized directory as the destination",
        ),
        ("a/b", "elsewhere", "a deeper synthesized directory"),
    ] {
        assert!(
            matches!(
                Mountable::rename(&ws, Path::new(from), Path::new(to)),
                Err(CortexError::ReadOnly)
            ),
            "{what} ({from} -> {to})"
        );
    }
}

#[test]
fn a_rename_with_nowhere_to_come_from_or_go_to() {
    let ws = table(&["work"]);
    Mountable::open(
        &ws,
        Path::new("work/a"),
        OpenOptions {
            create_new: true,
            ..OpenOptions::read_write()
        },
    )
    .unwrap();

    // The source has to exist, and nothing claims this path at all.
    assert!(matches!(
        Mountable::rename(&ws, Path::new("nowhere"), Path::new("work/b")),
        Err(CortexError::NotFound)
    ));
    // The destination is a *create*, and its parent is the synthesized root —
    // which exists and was just listed, so `ReadOnly` rather than a claim that
    // the path is missing.
    assert!(matches!(
        Mountable::rename(&ws, Path::new("work/a"), Path::new("newthing")),
        Err(CortexError::ReadOnly)
    ));
    // ...but a destination whose parent is not there either is genuinely absent.
    assert!(matches!(
        Mountable::rename(&ws, Path::new("work/a"), Path::new("nowhere/deeper")),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn a_synthesized_directory_reports_a_stable_timestamp() {
    let ws = table(&["a/b"]);

    let first = Mountable::stat(&ws, Path::new("a")).unwrap();
    let second = Mountable::stat(&ws, Path::new("a")).unwrap();
    assert!(
        first.mtime.is_some(),
        "the UNIX-epoch fallback is what this avoids"
    );
    assert_eq!(
        first.mtime, second.mtime,
        "an mtime that moves per call invalidates a guest's cache forever"
    );
}

#[test]
fn a_longer_sibling_name_does_not_leak_into_the_range() {
    // The whole algorithm rests on same-prefix keys forming one contiguous run.
    // `ab` sorts adjacent to `a` but is not under it: `Path::starts_with` works
    // on component boundaries, not raw bytes. Same for names whose next byte
    // sorts below `/` (0x2F).
    let ws = table(&["a", "ab", "a-b", "a b", "a/y"]);

    assert_eq!(children_of(&ws, "a"), [("y".into(), true)]);
    assert!(!ws.spans_mounts(Path::new("ab")));
    assert!(!ws.spans_mounts(Path::new("a-b")));
    assert!(ws.spans_mounts(Path::new("a")));
}

fn scratch(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "cortex-workspace-test-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn longest_prefix_routing() {
    // Two on-disk backends: a root, and a deeper one mounted at `data`.
    let root_dir = scratch("root");
    fs::write(root_dir.join("top.txt"), b"root").unwrap();

    let data_dir = scratch("data");
    fs::write(data_dir.join("inner.txt"), b"inner").unwrap();

    let ws = Workspace::new()
        .try_with_mount("", PassthroughVolume::new(&root_dir))
        .unwrap()
        .try_with_mount("data", PassthroughVolume::new(&data_dir))
        .unwrap();

    // Both `Mountable` and `DynMountable` (blanket impl) are in scope, so
    // calls to shared method names must name the trait explicitly.

    // `top.txt` is served by the root backend.
    assert_eq!(
        Mountable::stat(&ws, Path::new("top.txt")).unwrap().kind,
        DirentKind::File
    );
    let (h, _) = Mountable::open(&ws, Path::new("top.txt"), OpenOptions::read_only()).unwrap();
    let mut buf = [0u8; 4];
    h.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"root");

    // `data/inner.txt` is routed to the deeper mount, re-based to `inner.txt`.
    let (h, _) =
        Mountable::open(&ws, Path::new("data/inner.txt"), OpenOptions::read_only()).unwrap();
    let mut buf = [0u8; 5];
    h.read_exact_at(&mut buf, 0).unwrap();
    assert_eq!(&buf, b"inner");

    assert_eq!(
        Mountable::list(&ws, Path::new("data")).unwrap().len(),
        1,
        "the deeper mount only sees its own single file"
    );

    fs::remove_dir_all(&root_dir).unwrap();
    fs::remove_dir_all(&data_dir).unwrap();
}

#[test]
fn unmounted_paths_are_not_found() {
    let ws = Workspace::new();
    assert!(matches!(
        Mountable::stat(&ws, Path::new("anything")),
        Err(CortexError::NotFound)
    ));
}

#[test]
fn mount_rejects_duplicates_and_escapes() {
    let dir = scratch("dup");
    let mut ws = Workspace::new();
    ws.mount("m", PassthroughVolume::new(&dir)).unwrap();
    assert!(matches!(
        ws.mount("m", PassthroughVolume::new(&dir)),
        Err(CortexError::AlreadyExists)
    ));
    assert!(matches!(
        ws.mount("../escape", PassthroughVolume::new(&dir)),
        Err(CortexError::InvalidName)
    ));
    fs::remove_dir_all(&dir).unwrap();
}
