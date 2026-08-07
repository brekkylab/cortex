use super::*;
use crate::test_support::scratch;
use crate::{FileExt, InMemVolume, PassthroughVolume};
use std::fs;

/// Only the mount table matters here, so the cheapest backend will do.
fn table(paths: &[&str]) -> Workspace {
    paths.iter().fold(Workspace::new(), |ws, p| {
        ws.try_with_mount(p, InMemVolume::new()).unwrap()
    })
}

fn children_of(ws: &Workspace, at: &str) -> Vec<(String, bool)> {
    ws.mount_children(Path::new(at)).into_iter().collect()
}

/// Both mount-table queries share one range, so one invariant: `Excluded`, hence
/// strict descendants only. An `Included` bound would paper over a backend that
/// answers `NotFound` at its own root with a fabricated directory.
#[test]
fn the_mount_table_sees_only_strict_descendants() {
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
    assert!(ws.spans_mounts(Path::new("")));
    assert!(ws.spans_mounts(Path::new("a")));
    assert!(ws.spans_mounts(Path::new("a/b")));

    for exact in ["z", "a/x", "a/b/c"] {
        assert!(
            children_of(&ws, exact).is_empty(),
            "{exact} is not its own child"
        );
        assert!(
            !ws.spans_mounts(Path::new(exact)),
            "{exact} does not span itself"
        );
    }
    // A path the table knows nothing about answers for neither.
    assert!(children_of(&ws, "q").is_empty());
    assert!(!ws.spans_mounts(Path::new("q")));
}

#[tokio::test]
async fn a_rootless_workspace_has_a_root_directory() {
    let ws = table(&["s3-prod", "notion", "gdrive"]);

    assert_eq!(
        Mountable::stat(&ws, Path::new("")).await.unwrap().kind,
        DirentKind::Dir,
        "without it nothing can be mounted at all"
    );
    // A mount point itself is still its backend's answer.
    assert_eq!(
        Mountable::stat(&ws, Path::new("notion")).await.unwrap().kind,
        DirentKind::Dir
    );
    // Neither a mount nor on the way to one.
    assert!(matches!(
        Mountable::stat(&ws, Path::new("nope")).await,
        Err(CortexError::NotFound)
    ));
}

/// Both ladders at once, because the two have to agree at every rung: a step that
/// stats as a directory but lists nothing is a path `cd` enters and `ls` empties.
#[tokio::test]
async fn every_step_toward_a_deep_mount_is_a_directory_listing_the_next_one() {
    let ws = table(&["a/b/c"]);

    for (on_the_way, next) in [("", "a"), ("a", "b"), ("a/b", "c")] {
        assert_eq!(
            Mountable::stat(&ws, Path::new(on_the_way)).await.unwrap().kind,
            DirentKind::Dir,
            "{on_the_way:?} leads to a mount"
        );
        assert_eq!(
            listing(&ws, on_the_way).await,
            [(next.into(), DirentKind::Dir, false)],
            "{on_the_way:?} lists exactly the next component"
        );
    }
    // A sibling of a step leads nowhere.
    assert!(matches!(
        Mountable::stat(&ws, Path::new("a/x")).await,
        Err(CortexError::NotFound)
    ));
}

#[tokio::test]
async fn an_empty_workspace_is_an_empty_directory_not_a_missing_one() {
    let ws = Workspace::new();

    assert_eq!(
        Mountable::stat(&ws, Path::new("")).await.unwrap().kind,
        DirentKind::Dir,
        "a freshly mounted tmpfs is empty, not absent"
    );
    assert_eq!(
        listing(&ws, "").await,
        [],
        "and it lists nothing rather than failing"
    );
    assert!(matches!(
        Mountable::stat(&ws, Path::new("anything")).await,
        Err(CortexError::NotFound)
    ));
}

#[tokio::test]
async fn a_backend_that_does_not_know_a_path_on_the_way_to_a_mount_is_overridden() {
    // What "synthesize only when routing fails" misses: routing *succeeds* — the
    // root backend claims `data` — and the backend is what answers `NotFound`.
    let ws = Workspace::new()
        .try_with_mount("", InMemVolume::new())
        .unwrap()
        .try_with_mount("data/raw", InMemVolume::new())
        .unwrap();

    assert!(
        matches!(
            Mountable::stat(&InMemVolume::new(), Path::new("data")).await,
            Err(CortexError::NotFound)
        ),
        "precondition: a fresh in-memory volume has no `data`"
    );
    assert_eq!(
        Mountable::stat(&ws, Path::new("data")).await.unwrap().kind,
        DirentKind::Dir
    );
}

#[tokio::test]
async fn a_broken_root_backend_is_not_hidden_behind_a_synthesized_root() {
    // Nothing lies below the root, so there is no mount table to synthesize from
    // and the misconfiguration has to surface.
    let missing = scratch("workspace", "gone");
    fs::remove_dir_all(&missing).unwrap();
    let ws = Workspace::new()
        .try_with_mount("", PassthroughVolume::new(&missing))
        .unwrap();

    assert!(matches!(
        Mountable::stat(&ws, Path::new("")).await,
        Err(CortexError::NotFound)
    ));
}

async fn listing(ws: &Workspace, at: &str) -> Vec<(String, DirentKind, bool)> {
    Mountable::list(ws, Path::new(at)).await
        .unwrap()
        .into_iter()
        // `stat()` borrows, so it has to be read before `name` is moved out.
        .map(|d| {
            let carries_metadata = d.stat().is_some();
            (d.name, d.kind, carries_metadata)
        })
        .collect()
}

#[tokio::test]
async fn a_rootless_workspace_lists_its_mount_points() {
    let ws = table(&["s3-prod", "notion", "gdrive"]);

    assert_eq!(
        listing(&ws, "").await,
        [
            ("gdrive".into(), DirentKind::Dir, false),
            ("notion".into(), DirentKind::Dir, false),
            ("s3-prod".into(), DirentKind::Dir, false),
        ]
    );
}

#[tokio::test]
async fn a_name_merely_leading_to_a_mount_keeps_the_backends_entry() {
    // `shared` is in the root backend *and* leads to a mount at `shared/extra`.
    // No mount sits on it, so the backend keeps the name — and its entry carries
    // metadata a synthesized one cannot, which is the observable difference.
    let root = InMemVolume::new();
    Mountable::mkdir(&root, Path::new("shared")).await.unwrap();
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data/raw", InMemVolume::new())
        .unwrap()
        .try_with_mount("shared/extra", InMemVolume::new())
        .unwrap();

    assert_eq!(
        listing(&ws, "").await,
        [
            // Mount-derived names come first, sorted.
            ("data".into(), DirentKind::Dir, false),
            ("shared".into(), DirentKind::Dir, true),
        ],
        "`shared` appears once, and it is the backend's entry — it has a stat"
    );
}

#[tokio::test]
async fn a_mount_shadows_a_backend_entry_of_the_same_name() {
    // The topology `apply_krun` recommends — a root backend with mounts on top —
    // where the root holds a *file* named `data` and a directory lands over it.
    let root = InMemVolume::new();
    let (file, _) = Mountable::open(&root, Path::new("data"), OpenOptions::create_new()).await.unwrap();
    drop(file);
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data", InMemVolume::new())
        .unwrap();

    assert_eq!(
        listing(&ws, "").await,
        [("data".into(), DirentKind::Dir, false)],
        "readdir must not report a file where getattr reports a directory"
    );
    assert_eq!(
        Mountable::stat(&ws, Path::new("data")).await.unwrap().kind,
        DirentKind::Dir,
        "and this is the answer readdir has to agree with"
    );
}

#[tokio::test]
async fn backend_entries_that_are_not_mount_paths_come_after_the_mount_derived_ones() {
    let root = InMemVolume::new();
    for name in ["aaa", "zzz"] {
        Mountable::mkdir(&root, Path::new(name)).await.unwrap();
    }
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("mmm", InMemVolume::new())
        .unwrap();

    let names: Vec<_> = listing(&ws, "").await.into_iter().map(|(n, ..)| n).collect();
    assert_eq!(
        names[0], "mmm",
        "the mount holds position 1 however the backend sorts, so its churn \
         cannot shift the mount out of a readdir window"
    );
    assert_eq!(names.len(), 3);
}

#[tokio::test]
async fn a_list_error_other_than_not_found_is_not_papered_over() {
    // A *file* at `data` with `data/raw` mounted below it: `list` has to surface
    // ENOTDIR rather than pretend `data` is a directory it can enumerate.
    let root = InMemVolume::new();
    let (file, _) = Mountable::open(&root, Path::new("data"), OpenOptions::create_new()).await.unwrap();
    drop(file);
    let ws = Workspace::new()
        .try_with_mount("", root)
        .unwrap()
        .try_with_mount("data/raw", InMemVolume::new())
        .unwrap();

    assert!(matches!(
        Mountable::list(&ws, Path::new("data")).await,
        Err(CortexError::NotADirectory)
    ));
    // The asymmetry is deliberate: `stat` says `File`, `list` says ENOTDIR, and
    // the mount below is unreachable. Such a workspace is misconfigured and
    // says so rather than papering over it.
    assert_eq!(
        Mountable::stat(&ws, Path::new("data")).await.unwrap().kind,
        DirentKind::File
    );
    assert_eq!(
        Mountable::stat(&ws, Path::new("data/raw")).await.unwrap().kind,
        DirentKind::Dir
    );
}

/// What the root backend holds at `holder`, with a mount at `holder/inner`.
/// Every variant is one the backend would answer `Ok` to — which is what makes
/// checking the mount table first load-bearing rather than decorative.
enum Holder {
    /// An empty directory: `rmdir` would succeed on it.
    EmptyDir,
    /// A real file: `unlink` would delete it.
    File,
    /// Nothing: `mkdir` would create one, and `open(create)` a file.
    Absent,
}

async fn shadowing_backend(holder: Holder) -> Workspace {
    let root = InMemVolume::new();
    match holder {
        Holder::EmptyDir => Mountable::mkdir(&root, Path::new("holder")).await.unwrap(),
        Holder::File => {
            Mountable::open(&root, Path::new("holder"), OpenOptions::create_new()).await.unwrap();
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
/// below it. Every row's backend would answer `Ok`, so without the pre-flight
/// check each one detaches the mount.
#[tokio::test]
async fn a_mutation_aimed_at_the_mount_table_is_refused_before_the_backend_is_asked() {
    type Op = for<'a> fn(
        &'a Workspace,
        &'a Path,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>>;

    let rmdir: Op = |ws, path| Box::pin(async move { Mountable::rmdir(ws, path).await });
    let unlink: Op = |ws, path| Box::pin(async move { Mountable::unlink(ws, path).await });
    let mkdir: Op = |ws, path| Box::pin(async move { Mountable::mkdir(ws, path).await });
    let create: Op = |ws, path| {
        Box::pin(async move {
            Mountable::open(ws, path, OpenOptions::read_write().create(true))
                .await
                .map(|_| ())
        })
    };

    #[rustfmt::skip]
    let rows = [
        (Holder::EmptyDir, rmdir,  CortexError::NotEmpty,
         "removes a directory the mount table still populates"),
        (Holder::File,     unlink, CortexError::IsADirectory,
         "deletes the mount's parent"),
        (Holder::Absent,   mkdir,  CortexError::AlreadyExists,
         "succeeds where POSIX requires EEXIST"),
        (Holder::Absent,   create, CortexError::IsADirectory,
         "leaves a file, after which everything under it is ENOTDIR"),
    ];
    for (holder, op, expected, damage) in rows {
        let ws = shadowing_backend(holder).await;
        let got = op(&ws, Path::new("holder")).await;
        assert!(
            std::mem::discriminant(got.as_ref().unwrap_err()) == std::mem::discriminant(&expected),
            "expected {expected:?}, got {got:?} — unguarded this {damage}"
        );
        assert_eq!(
            Mountable::stat(&ws, Path::new("holder/inner")).await
                .unwrap()
                .kind,
            DirentKind::Dir,
            "the mount was disturbed: {damage}"
        );
    }
}

#[tokio::test]
async fn creating_in_the_synthesized_namespace_is_read_only_not_missing() {
    // No backend at the root to create in — but the root stats as a directory and
    // lists three entries, so `NotFound` would be a lie that reads to the user as
    // a filesystem bug.
    let ws = table(&["s3-prod", "notion", "gdrive"]);

    assert!(matches!(
        Mountable::mkdir(&ws, Path::new("newthing")).await,
        Err(CortexError::ReadOnly)
    ));
    assert!(matches!(
        Mountable::open(
            &ws,
            Path::new("newthing"),
            OpenOptions::read_write().create(true)
        ).await,
        Err(CortexError::ReadOnly)
    ));

    // Narrow on purpose: an *unrouted parent* really is a missing component.
    assert!(matches!(
        Mountable::mkdir(&ws, Path::new("nowhere/deeper")).await,
        Err(CortexError::NotFound)
    ));
    assert!(
        Mountable::mkdir(&ws, Path::new("notion/fresh")).await.is_ok(),
        "a real mount still writes"
    );
}

/// Every path a mount cannot be keyed by, through both entry points since they
/// must not disagree.
#[test]
fn a_mount_path_that_cannot_be_keyed_is_refused() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    // A listing name is a `String`, so a non-UTF-8 component could only be
    // reported lossily — and a lossy name does not round-trip, giving an entry
    // that appears in `list` but whose `lookup` fails. This is where mount names
    // first become visible, so this is where it is caught.
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

    // A path that climbs out of the workspace root, likewise.
    assert!(matches!(
        ws.mount("../escape", InMemVolume::new()),
        Err(CortexError::InvalidName)
    ));

    // A point already taken. `try_with_mount` is the builder and overwrites.
    ws.mount("m", InMemVolume::new()).unwrap();
    assert!(matches!(
        ws.mount("m", InMemVolume::new()),
        Err(CortexError::AlreadyExists)
    ));
}

#[tokio::test]
async fn a_workspace_mounted_inside_a_workspace_serves_through_both() {
    // Synthesized roots are what make this work: the inner workspace has to answer
    // for its own root before the outer one can route into it.
    let inner = Workspace::new()
        .try_with_mount("leaf", InMemVolume::new())
        .unwrap();
    let outer = Workspace::new().try_with_mount("nested", inner).unwrap();

    assert_eq!(
        Mountable::stat(&outer, Path::new("nested")).await.unwrap().kind,
        DirentKind::Dir
    );
    assert_eq!(
        listing(&outer, "nested").await,
        [("leaf".into(), DirentKind::Dir, false)]
    );
    assert_eq!(
        Mountable::stat(&outer, Path::new("nested/leaf")).await
            .unwrap()
            .kind,
        DirentKind::Dir
    );

    // And a write reaches the innermost backend through both hops.
    Mountable::mkdir(&outer, Path::new("nested/leaf/deep")).await.unwrap();
    assert_eq!(
        Mountable::stat(&outer, Path::new("nested/leaf/deep")).await
            .unwrap()
            .kind,
        DirentKind::Dir
    );
}

#[tokio::test]
async fn a_rename_inside_one_mount_reaches_its_backend() {
    let ws = table(&["work"]);
    Mountable::open(&ws, Path::new("work/a"), OpenOptions::create_new()).await.unwrap();

    Mountable::rename(&ws, Path::new("work/a"), Path::new("work/b")).await.unwrap();

    assert!(matches!(
        Mountable::stat(&ws, Path::new("work/a")).await,
        Err(CortexError::NotFound)
    ));
    assert_eq!(
        Mountable::stat(&ws, Path::new("work/b")).await.unwrap().kind,
        DirentKind::File
    );
}

#[tokio::test]
async fn a_rename_across_two_mounts_is_a_cross_device_move() {
    // One kernel mount, two backends. The kernel cannot see the workspace's mount
    // table, so it asks — and `EXDEV` is the answer `mv` recovers from by copying.
    let ws = table(&["notion", "s3"]);
    Mountable::open(&ws, Path::new("notion/draft.md"), OpenOptions::create_new()).await.unwrap();

    assert!(matches!(
        Mountable::rename(&ws, Path::new("notion/draft.md"), Path::new("s3/draft.md")).await,
        Err(CortexError::CrossDevice)
    ));
    assert!(
        Mountable::stat(&ws, Path::new("notion/draft.md")).await.is_ok(),
        "nothing moved"
    );
}

#[tokio::test]
async fn the_mount_table_is_not_the_filesystems_to_rearrange() {
    // `s3-prod`/`notion` are mount points; `a` exists only because `a/b/c` is
    // below it. Moving either would rewrite the mount table through a file op.
    let ws = table(&["s3-prod", "notion", "a/b/c"]);

    #[rustfmt::skip]
    let rows = [
        ("s3-prod",   "archive",   "a mount point as the source"),
        ("notion",    "s3-prod",   "a mount point as the destination"),
        ("a",         "b",         "a synthesized directory as the source"),
        ("s3-prod/f", "a",         "a synthesized directory as the destination"),
        ("a/b",       "elsewhere", "a deeper synthesized directory"),
    ];
    for (from, to, what) in rows {
        assert!(
            matches!(
                Mountable::rename(&ws, Path::new(from), Path::new(to)).await,
                Err(CortexError::ReadOnly)
            ),
            "{what} ({from} -> {to})"
        );
    }
}

#[tokio::test]
async fn a_rename_with_nowhere_to_come_from_or_go_to() {
    let ws = table(&["work"]);
    Mountable::open(&ws, Path::new("work/a"), OpenOptions::create_new()).await.unwrap();

    // Nothing claims the source at all.
    assert!(matches!(
        Mountable::rename(&ws, Path::new("nowhere"), Path::new("work/b")).await,
        Err(CortexError::NotFound)
    ));
    // The destination is a *create* under the synthesized root, which exists —
    // so `ReadOnly`, not a claim that the path is missing.
    assert!(matches!(
        Mountable::rename(&ws, Path::new("work/a"), Path::new("newthing")).await,
        Err(CortexError::ReadOnly)
    ));
    // ...but an absent parent really is absent.
    assert!(matches!(
        Mountable::rename(&ws, Path::new("work/a"), Path::new("nowhere/deeper")).await,
        Err(CortexError::NotFound)
    ));
}

#[tokio::test]
async fn a_synthesized_directory_reports_a_stable_timestamp() {
    let ws = table(&["a/b"]);

    let first = Mountable::stat(&ws, Path::new("a")).await.unwrap();
    let second = Mountable::stat(&ws, Path::new("a")).await.unwrap();
    assert!(
        first.mtime.is_some(),
        "the epoch fallback is what this avoids"
    );
    assert_eq!(
        first.mtime, second.mtime,
        "an mtime that moves per call invalidates a guest's cache forever"
    );
}

#[test]
fn a_longer_sibling_name_does_not_leak_into_the_range() {
    // The algorithm rests on same-prefix keys forming one contiguous run, and
    // `ab` sorts adjacent to `a` without being under it: `Path::starts_with`
    // works on component boundaries, not bytes. Same below `/` (0x2F).
    let ws = table(&["a", "ab", "a-b", "a b", "a/y"]);

    assert_eq!(children_of(&ws, "a"), [("y".into(), true)]);
    assert!(!ws.spans_mounts(Path::new("ab")));
    assert!(!ws.spans_mounts(Path::new("a-b")));
    assert!(ws.spans_mounts(Path::new("a")));
}

#[tokio::test]
async fn longest_prefix_routing() {
    let root_dir = scratch("workspace", "root");
    fs::write(root_dir.join("top.txt"), b"root").unwrap();

    let data_dir = scratch("workspace", "data");
    fs::write(data_dir.join("inner.txt"), b"inner").unwrap();

    let ws = Workspace::new()
        .try_with_mount("", PassthroughVolume::new(&root_dir))
        .unwrap()
        .try_with_mount("data", PassthroughVolume::new(&data_dir))
        .unwrap();

    // Both traits are in scope via the blanket impl, so shared method names have
    // to be qualified. `top.txt` is the root backend's.
    assert_eq!(
        Mountable::stat(&ws, Path::new("top.txt")).await.unwrap().kind,
        DirentKind::File
    );
    let (h, _) = Mountable::open(&ws, Path::new("top.txt"), OpenOptions::read_only()).await.unwrap();
    let mut buf = [0u8; 4];
    h.read_exact_at(&mut buf, 0).await.unwrap();
    assert_eq!(&buf, b"root");

    // Routed to the deeper mount, re-based to `inner.txt`.
    let (h, _) =
        Mountable::open(&ws, Path::new("data/inner.txt"), OpenOptions::read_only()).await.unwrap();
    let mut buf = [0u8; 5];
    h.read_exact_at(&mut buf, 0).await.unwrap();
    assert_eq!(&buf, b"inner");

    assert_eq!(
        Mountable::list(&ws, Path::new("data")).await.unwrap().len(),
        1,
        "the deeper mount only sees its own single file"
    );

    fs::remove_dir_all(&root_dir).unwrap();
    fs::remove_dir_all(&data_dir).unwrap();
}

/// Records every [`FsEvent`] the workspace fires, as `"<verb> <path>"`, so a test
/// can assert the exact sequence — order included.
#[derive(Default)]
struct RecordingHook(std::sync::Mutex<Vec<String>>);

impl crate::FsHook for RecordingHook {
    fn on_change(&self, event: crate::FsEvent<'_>) {
        let line = match event {
            crate::FsEvent::Created(p) => format!("created {p}"),
            crate::FsEvent::Modified(p) => format!("modified {p}"),
            crate::FsEvent::Removed(p) => format!("removed {p}"),
        };
        self.0.lock().unwrap().push(line);
    }
}

/// The hook fires on every *successful* mutation, once per write, and names the
/// right verb. Covers the fire points that had gaps: `mkdir` (was silent while
/// `rmdir` fired), and a bare `truncate`+`commit` (a resize with no flush) — plus
/// the create-vs-modify split and the "nothing on failure" gate.
#[tokio::test]
async fn hook_fires_once_per_successful_mutation() {
    use crate::{FileExt, FileHandle};
    use std::sync::Arc;

    let hook = Arc::new(RecordingHook::default());
    let ws = Workspace::new()
        .try_with_mount("", InMemVolume::new())
        .unwrap()
        .with_hook(Some(hook.clone() as Arc<dyn crate::FsHook>));

    // mkdir fires `Created` — symmetric with rmdir's `Removed`.
    Mountable::mkdir(&ws, Path::new("sub")).await.unwrap();

    // A fresh file: create + write + flush fires `Created` exactly once.
    let (h, _) = Mountable::open(&ws, Path::new("a.txt"), OpenOptions::create_new()).await.unwrap();
    h.write_all_at(b"hi", 0).await.unwrap();
    h.flush().await.unwrap();

    // Overwriting an existing file fires `Modified`.
    let (h, _) =
        Mountable::open(&ws, Path::new("a.txt"), OpenOptions::read_write()).await.unwrap();
    h.write_all_at(b"yo", 0).await.unwrap();
    h.flush().await.unwrap();

    // A bare truncate finalized with `commit` (no flush) still fires — the path a
    // `setattr(size)` with no open fd takes.
    let (h, _) =
        Mountable::open(&ws, Path::new("a.txt"), OpenOptions::read_write()).await.unwrap();
    h.truncate(1).await.unwrap();
    h.commit().await.unwrap();

    // A failed mutation (unlink of a path that isn't there) fires nothing.
    assert!(Mountable::unlink(&ws, Path::new("nope.txt")).await.is_err());

    assert_eq!(
        *hook.0.lock().unwrap(),
        ["created sub", "created a.txt", "modified a.txt", "modified a.txt"],
    );
}
