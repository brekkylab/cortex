//! Tests for the S3 backend, over `object_store`'s in-memory store.
//!
//! No credentials and no network: `object_store::memory` is not feature-gated, so
//! `InMemory` stands in for the real thing everywhere the two agree. Where they do
//! not, the test says so — a key with a trailing `/` (an S3 console's folder
//! marker) cannot be represented at all, because `Path::parse` strips the slash.

use std::sync::Arc;

use object_store::ObjectStoreExt;
use object_store::memory::InMemory;
use object_store::path::Path as OsPath;

use super::*;

/// A volume over an empty in-memory store.
fn empty() -> S3Volume {
    S3Volume::with_store(Arc::new(InMemory::new()), String::new())
}

/// A volume whose keys all sit under `prefix`.
fn under(prefix: &str) -> S3Volume {
    S3Volume::with_store(Arc::new(InMemory::new()), prefix.to_string())
}

/// A volume holding `keys`, each with the given body. Keys are absolute in the
/// store, so a test that also wants a prefix states it in both places.
fn holding(prefix: &str, keys: &[(&str, &str)]) -> S3Volume {
    let store = Arc::new(InMemory::new());
    runtime().expect("runtime").block_on(async {
        for (key, body) in keys {
            store
                .put(&OsPath::from(*key), body.to_string().into())
                .await
                .expect("put");
        }
    });
    S3Volume::with_store(store, prefix.to_string())
}

// ---------------------------------------------------------------- error mapping

/// Every variant of `object_store::Error` that exists today, and what it becomes.
///
/// The wildcard `to_cortex_error` needs for `#[non_exhaustive]` means a dropped arm
/// would compile and silently answer `Io`, so this is what guards the table — for the
/// arms that answer something *other* than `Io`. For the rest the wildcard gives the
/// same label, so the guard is
/// [elsewhere](the_io_arms_forward_their_source_rather_than_the_whole_error).
#[test]
fn every_object_store_error_maps_to_its_own_cortex_error() {
    use object_store::Error;

    let cases: Vec<(Error, &str)> = vec![
        (
            Error::NotFound {
                path: "k".into(),
                source: Box::new(io::Error::other("x")),
            },
            "NotFound",
        ),
        (
            Error::AlreadyExists {
                path: "k".into(),
                source: Box::new(io::Error::other("x")),
            },
            "AlreadyExists",
        ),
        (
            Error::PermissionDenied {
                path: "k".into(),
                source: Box::new(io::Error::other("x")),
            },
            "PermissionDenied",
        ),
        (
            Error::Unauthenticated {
                path: "k".into(),
                source: Box::new(io::Error::other("x")),
            },
            "PermissionDenied",
        ),
        (
            Error::UnknownConfigurationKey {
                store: "s3",
                key: "k".into(),
            },
            "InvalidArgument",
        ),
        (
            Error::NotSupported {
                source: Box::new(io::Error::other("x")),
            },
            "Unsupported",
        ),
        (
            Error::NotImplemented {
                operation: "op".into(),
                implementer: "imp".into(),
            },
            "Unsupported",
        ),
        (
            Error::Precondition {
                path: "k".into(),
                source: Box::new(io::Error::other("x")),
            },
            "Io",
        ),
        (
            Error::NotModified {
                path: "k".into(),
                source: Box::new(io::Error::other("x")),
            },
            "Io",
        ),
        (
            Error::Generic {
                store: "s3",
                source: Box::new(io::Error::other("x")),
            },
            "Io",
        ),
    ];

    for (err, want) in cases {
        let got = to_cortex_error(err);
        assert_eq!(label(&got), want, "mapped to {got:?}");
    }
}

/// `InvalidPath` needs the store's own error type to construct, so it gets its own
/// case: an empty segment is the simplest way to make one.
#[test]
fn an_unparsable_key_is_an_invalid_name() {
    let err = OsPath::parse("a//b").expect_err("empty segment is rejected");
    assert_eq!(label(&to_cortex_error(err.into())), "InvalidName");
}

/// Where the table test's guard runs out.
///
/// Four arms answer `Io`, which is also what the wildcard answers, so dropping one of
/// them would leave the table green. What they do that the wildcard does not is forward
/// the inner `source` instead of wrapping the whole error — so that is what gets
/// asserted, and it is the difference a lost arm would actually make. `Precondition`
/// stands for the group: were it wrapped, its `Display` would prepend
/// `"Request precondition failure for path k: "`.
#[test]
fn the_io_arms_forward_their_source_rather_than_the_whole_error() {
    let err = to_cortex_error(object_store::Error::Precondition {
        path: "k".into(),
        source: Box::new(io::Error::other("the etag moved")),
    });
    let CortexError::Io(inner) = err else {
        panic!("Precondition should map to Io");
    };
    assert_eq!(
        inner.to_string(),
        "the etag moved",
        "the source should arrive alone; anything longer means the error was wrapped"
    );
}

/// Anything sent to `Io` must carry no OS errno, or `host_errno` would forward it
/// to userspace instead of collapsing to EIO.
#[test]
fn errors_mapped_to_io_carry_no_raw_errno() {
    // 28 is ENOSPC on this platform, but the number is immaterial — the test needs an
    // `io::Error` that carries *some* errno.
    let err = to_cortex_error(object_store::Error::Generic {
        store: "s3",
        source: Box::new(io::Error::from_raw_os_error(28)),
    });
    match err {
        CortexError::Io(e) => assert!(
            e.raw_os_error().is_none(),
            "a preserved errno would reach userspace as itself"
        ),
        other => panic!("expected Io, got {other:?}"),
    }
}

fn label(err: &CortexError) -> &'static str {
    match err {
        CortexError::NotFound => "NotFound",
        CortexError::NotADirectory => "NotADirectory",
        CortexError::IsADirectory => "IsADirectory",
        CortexError::AlreadyExists => "AlreadyExists",
        CortexError::NotEmpty => "NotEmpty",
        CortexError::InvalidName => "InvalidName",
        CortexError::InvalidArgument => "InvalidArgument",
        CortexError::FileTooLarge => "FileTooLarge",
        CortexError::BadHandle => "BadHandle",
        CortexError::PermissionDenied => "PermissionDenied",
        CortexError::NoSpace => "NoSpace",
        CortexError::ReadOnly => "ReadOnly",
        CortexError::CrossDevice => "CrossDevice",
        CortexError::Unsupported => "Unsupported",
        CortexError::Io(_) => "Io",
    }
}

/// Every kind `to_io_error` picks has to be one `From<io::Error>` turns back into the
/// same variant. The data plane speaks `io::Error`, so a kind the conversion does not
/// recognise arrives at the errno tables as a bare `Io` and renders EIO — losing the
/// classification those tables exist to make.
#[test]
fn a_data_plane_error_survives_the_round_trip() {
    // Built by closure because `CortexError` is not `Clone` — the original is needed
    // twice, once to convert and once to name.
    let cases: [fn() -> CortexError; 10] = [
        || CortexError::NotFound,
        || CortexError::PermissionDenied,
        || CortexError::ReadOnly,
        || CortexError::Unsupported,
        || CortexError::IsADirectory,
        || CortexError::NotADirectory,
        || CortexError::AlreadyExists,
        || CortexError::NotEmpty,
        || CortexError::FileTooLarge,
        || CortexError::NoSpace,
    ];
    for make in cases {
        let want = label(&make());
        let back: CortexError = to_io_error(make()).into();
        assert_eq!(label(&back), want, "{want} did not survive");
    }

    // `InvalidName` and `InvalidArgument` share `InvalidInput`, so the round trip
    // collapses them. Harmless: both errno tables render EINVAL either way.
    let back: CortexError = to_io_error(CortexError::InvalidName).into();
    assert_eq!(label(&back), "InvalidArgument");

    // The two that cannot survive, asserted so the exception stays deliberate: std
    // has no `ErrorKind` for either.
    for make in [|| CortexError::BadHandle, || CortexError::CrossDevice] {
        let back: CortexError = to_io_error(make()).into();
        assert_eq!(
            label(&back),
            "Io",
            "{} should degrade to Io",
            label(&make())
        );
    }
}

// ------------------------------------------------------------------ key mapping

#[test]
fn a_path_becomes_a_key_under_the_prefix() {
    let vol = under("root/sub");
    assert_eq!(vol.key(Path::new("a/b.txt")).unwrap(), "root/sub/a/b.txt");
    assert_eq!(vol.key(Path::new("/a")).unwrap(), "root/sub/a");
    assert_eq!(vol.key(Path::new("")).unwrap(), "root/sub");
}

#[test]
fn no_prefix_leaves_the_path_alone() {
    assert_eq!(empty().key(Path::new("a/b.txt")).unwrap(), "a/b.txt");
    assert_eq!(empty().key(Path::new("")).unwrap(), "");
}

/// The prefix is stored without surrounding slashes so joining never doubles one.
#[test]
fn a_prefix_is_normalized_however_it_is_written() {
    for written in ["root", "/root", "root/", "/root/"] {
        assert_eq!(
            under(written).key(Path::new("a")).unwrap(),
            "root/a",
            "prefix written as {written:?}"
        );
    }
}

/// The rule `PassthroughVolume` applies to its root: `..` is rejected, not
/// resolved, so no request can address a key outside the prefix.
#[test]
fn a_path_cannot_escape_the_prefix() {
    let vol = under("root");
    for escape in ["..", "../x", "a/../../x", "a/.."] {
        assert_eq!(
            label(&vol.key(Path::new(escape)).unwrap_err()),
            "InvalidName",
            "path {escape:?}"
        );
    }
}

/// `.` and a leading `/` are ignored rather than rejected — ordinary ways to spell
/// the same path.
#[test]
fn curdir_and_root_components_are_ignored() {
    let vol = under("root");
    assert_eq!(vol.key(Path::new("./a")).unwrap(), "root/a");
    assert_eq!(vol.key(Path::new("a/./b")).unwrap(), "root/a/b");
}

// ---------------------------------------------------------------- write refusal

/// Every write answers `ReadOnly`, never `Unsupported`: the store could write, this
/// backend will not, and `EROFS` is the state userspace has a path for.
#[test]
fn every_namespace_write_is_read_only() {
    let vol = empty();
    assert_eq!(label(&vol.mkdir(Path::new("d")).unwrap_err()), "ReadOnly");
    assert_eq!(label(&vol.unlink(Path::new("f")).unwrap_err()), "ReadOnly");
    assert_eq!(label(&vol.rmdir(Path::new("d")).unwrap_err()), "ReadOnly");
    assert_eq!(
        label(&Mountable::rename(&vol, Path::new("a"), Path::new("b")).unwrap_err()),
        "ReadOnly"
    );
}

/// `write_at` returns `io::Error`, so read-only has to travel as
/// `ErrorKind::ReadOnlyFilesystem` — the one kind `From<io::Error>` turns back into
/// `ReadOnly`. `ErrorKind::Unsupported` would arrive as ENOSYS instead.
#[test]
fn a_handle_refuses_writes_as_read_only_not_unsupported() {
    let handle = S3Handle {
        store: Arc::new(InMemory::new()),
        key: OsPath::from("f"),
        size: 0,
        cache: Mutex::default(),
    };
    let err = FileExt::write_at(&handle, b"x", 0).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ReadOnlyFilesystem);
    assert_eq!(label(&CortexError::from(err)), "ReadOnly");
    assert_eq!(label(&handle.truncate(1).unwrap_err()), "ReadOnly");
}

/// A listing that fails must not read as "no children" — [`children_from`] gives the
/// reason.
///
/// Asked of the decision rather than of a store, because that is where the rule lives:
/// standing up an `ObjectStore` whose listings fail would need two proc-macro crates
/// named in `Cargo.toml` to assert one line of logic.
#[test]
fn a_failed_listing_is_not_an_absent_path() {
    let failed = Err(object_store::Error::Generic {
        store: "test",
        source: Box::new(io::Error::other("the store is unreachable")),
    });
    assert_eq!(
        label(&children_from(failed).unwrap_err()),
        "Io",
        "a failed listing must not become an answer about children"
    );

    // And the answers it *can* give, so the shape of both is pinned together.
    let empty = object_store::ListResult {
        common_prefixes: Vec::new(),
        objects: Vec::new(),
        extensions: Default::default(),
    };
    assert!(!children_from(Ok(empty)).expect("an empty listing is an answer"));
}

// ----------------------------------------------------------------------- stat

/// A file carries everything the listing knew, not just its size — the fields a
/// consumer needs for cache validation.
#[test]
fn a_file_reports_its_size_and_metadata() {
    let vol = holding("", &[("a.txt", "hello")]);
    let stat = vol.stat(Path::new("a.txt")).expect("stat");
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, 5);
    assert!(
        stat.mtime.is_some(),
        "LastModified should reach Stat::mtime"
    );
    assert!(stat.etag.is_some(), "ETag should reach Stat::etag");
}

/// The root is a directory by construction. Answering it without a round trip
/// matters because every mount begins with a `getattr` on it.
#[test]
fn the_root_is_a_directory() {
    let stat = holding("", &[("a.txt", "x")])
        .stat(Path::new(""))
        .expect("stat root");
    assert_eq!(stat.kind, DirentKind::Dir);

    // Same when the volume is rooted under a key prefix that has no object of its
    // own — the prefix root is still a directory.
    let stat = holding("root", &[("root/a.txt", "x")])
        .stat(Path::new(""))
        .expect("stat prefix root");
    assert_eq!(stat.kind, DirentKind::Dir);
}

/// A prefix is a directory even though no object has that key. `head` 404s and the
/// listing fallback is what finds the children.
#[test]
fn a_prefix_with_children_is_a_directory() {
    let vol = holding("", &[("dir/a.txt", "x")]);
    assert_eq!(
        vol.stat(Path::new("dir")).expect("stat dir").kind,
        DirentKind::Dir
    );
}

/// An object whose key equals a prefix and whose body is empty is a folder marker. `head` *succeeds* on it, so reporting what `head` said would call
/// the directory a 0-byte file and the guest could not descend into it.
#[test]
fn a_zero_byte_marker_with_children_is_a_directory() {
    let vol = holding("", &[("dir", ""), ("dir/a.txt", "x")]);
    assert_eq!(
        vol.stat(Path::new("dir")).expect("stat marker").kind,
        DirentKind::Dir,
        "a marker with children must be traversable"
    );
}

/// The other side of that: a genuinely empty file is a file. `_SUCCESS` and
/// `.gitkeep` are not directories just because they are empty.
#[test]
fn a_zero_byte_file_without_children_stays_a_file() {
    let vol = holding("", &[("_SUCCESS", "")]);
    let stat = vol.stat(Path::new("_SUCCESS")).expect("stat empty file");
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, 0);
}

/// A path with neither an object nor children is absent. The listing fallback
/// cannot answer this by itself: a missing prefix returns an empty result rather
/// than an error, so without the emptiness check every path would look like a
/// directory.
#[test]
fn a_missing_path_is_not_found() {
    let vol = holding("", &[("dir/a.txt", "x")]);
    for missing in ["nope", "dir/nope", "nope/deeper"] {
        assert_eq!(
            label(&vol.stat(Path::new(missing)).unwrap_err()),
            "NotFound",
            "path {missing:?}"
        );
    }
}

/// The prefix composes with the request path, so a volume rooted at `root` sees
/// `root/a.txt` when asked for `a.txt` — and cannot see anything outside it.
#[test]
fn stat_resolves_through_the_key_prefix() {
    let vol = holding("root", &[("root/a.txt", "hello"), ("outside.txt", "no")]);
    assert_eq!(vol.stat(Path::new("a.txt")).expect("stat").size, 5);
    assert_eq!(
        label(&vol.stat(Path::new("outside.txt")).unwrap_err()),
        "NotFound",
        "a key beside the prefix is not visible through it"
    );
}

// ----------------------------------------------------------------------- list

/// Names in a listing, as `kind:name`, sorted — enough to assert both what is there
/// and what kind each entry claims to be.
fn listed(vol: &S3Volume, path: &str) -> Vec<String> {
    let mut names: Vec<_> = vol
        .list(Path::new(path))
        .expect("list")
        .into_iter()
        .map(|entry| {
            let kind = match entry.kind {
                DirentKind::Dir => "dir",
                DirentKind::File => "file",
            };
            format!("{kind}:{}", entry.name)
        })
        .collect();
    names.sort();
    names
}

/// One level, not the whole subtree: a listing names the entries directly under the
/// path, with deeper keys rolled into the directory they sit in.
#[test]
fn a_listing_names_one_level() {
    let vol = holding(
        "",
        &[
            ("a.txt", "x"),
            ("dir/b.txt", "y"),
            ("dir/sub/c.txt", "z"),
            ("dir2/d.txt", "w"),
        ],
    );
    assert_eq!(listed(&vol, ""), ["dir:dir", "dir:dir2", "file:a.txt"]);
    assert_eq!(listed(&vol, "dir"), ["dir:sub", "file:b.txt"]);
}

/// The listing already carries each object's metadata, so the entry does too.
/// This is what lets a `readdirplus` answer without a `stat` per name.
#[test]
fn a_listing_carries_metadata_it_already_had() {
    let vol = holding("", &[("a.txt", "hello")]);
    let entries = vol.list(Path::new("")).expect("list");
    let file = entries
        .iter()
        .find(|e| e.name == "a.txt")
        .expect("a.txt listed");
    let stat = file.stat().expect("stat filled from the listing");
    assert_eq!(stat.size, 5);
    assert!(stat.mtime.is_some());
    assert!(stat.etag.is_some());
}

/// A directory has no metadata to carry: a prefix is not an object, so there is
/// nothing the listing could have known about it.
#[test]
fn a_listed_directory_carries_no_metadata() {
    let vol = holding("", &[("dir/a.txt", "x")]);
    let dir = vol
        .list(Path::new(""))
        .expect("list")
        .into_iter()
        .find(|e| e.name == "dir")
        .expect("dir listed");
    assert!(dir.stat().is_none());
}

/// A folder marker surfaces as an object *at its own level* — the key equals the
/// prefix being listed. It is this directory, not an entry inside it.
#[test]
fn a_marker_is_not_an_entry_in_its_own_directory() {
    let vol = holding("", &[("dir", ""), ("dir/a.txt", "x")]);
    assert_eq!(listed(&vol, "dir"), ["file:a.txt"]);
}

/// The zero-byte branch of a name collision: it arrives from both `common_prefixes` and
/// `objects`, and a `readdir` may not repeat a name. An empty body means the object
/// stands for the directory, so the object goes and the directory stays.
#[test]
fn a_zero_byte_object_yields_to_the_directory_of_the_same_name() {
    let vol = holding("", &[("dir/sub", ""), ("dir/sub/c.txt", "z")]);
    assert_eq!(
        listed(&vol, "dir"),
        ["dir:sub"],
        "the name must appear once, as the directory"
    );
}

/// The other branch: an object with a body is real content and cannot be
/// hidden, so the directory yields instead. Its subtree becomes unreachable — the
/// cost this rule accepts, and the reason the branch turns on size.
#[test]
fn an_object_with_content_wins_over_the_directory_of_the_same_name() {
    let vol = holding("", &[("dir/sub", "eleven byte"), ("dir/sub/c.txt", "z")]);
    assert_eq!(
        listed(&vol, "dir"),
        ["file:sub"],
        "the name must appear once, as the file"
    );
}

/// `list` and `stat` have to agree, or a kernel gets a directory from `readdir` and a
/// file from the `lookup` that follows. They agree by reading the same fact — the
/// object's size — rather than by consulting each other.
#[test]
fn list_and_stat_agree_on_a_name_that_is_both() {
    for (body, want) in [("", DirentKind::Dir), ("eleven byte", DirentKind::File)] {
        let vol = holding("", &[("dir/sub", body), ("dir/sub/c.txt", "z")]);
        let listed_kind = vol
            .list(Path::new("dir"))
            .expect("list")
            .into_iter()
            .find(|e| e.name == "sub")
            .expect("sub listed")
            .kind;
        let stat_kind = vol.stat(Path::new("dir/sub")).expect("stat").kind;
        assert_eq!(listed_kind, want, "list disagreed for body {body:?}");
        assert_eq!(stat_kind, want, "stat disagreed for body {body:?}");
    }
}

/// An empty directory lists as empty rather than failing — and a path that is not a
/// directory at all is `NotFound`, which is `stat`'s job to have established.
#[test]
fn listing_a_missing_prefix_is_empty() {
    let vol = holding("", &[("dir/a.txt", "x")]);
    assert!(listed(&vol, "nope").is_empty());
}

#[test]
fn list_resolves_through_the_key_prefix() {
    let vol = holding("root", &[("root/a.txt", "x"), ("outside.txt", "no")]);
    assert_eq!(listed(&vol, ""), ["file:a.txt"]);
}

// ----------------------------------------------------------------------- open

#[test]
fn opening_a_file_yields_its_metadata_with_the_handle() {
    let vol = holding("", &[("a.txt", "hello")]);
    let (handle, stat) = vol
        .open(Path::new("a.txt"), OpenOptions::read_only())
        .expect("open");
    assert_eq!(stat.kind, DirentKind::File);
    assert_eq!(stat.size, 5);
    assert!(
        stat.etag.is_some(),
        "the listing's metadata rides along with the open"
    );
    // The handle carries the same size, which is what lets a read past the end
    // answer without asking the store.
    assert_eq!(handle.size, 5);
}

/// Every write intent is refused, and before the network is touched — the answer does
/// not depend on the object.
#[test]
fn opening_for_writing_is_read_only() {
    let vol = holding("", &[("a.txt", "hello")]);
    let write_intents = [
        OpenOptions::read_write(),
        OpenOptions::create_new(),
        OpenOptions::read_only().append(true),
        OpenOptions::read_only().truncate(true),
        OpenOptions::read_only().create(true),
    ];
    for options in write_intents {
        assert_eq!(
            label(&vol.open(Path::new("a.txt"), options).unwrap_err()),
            "ReadOnly",
            "options {options:?}"
        );
    }
}

/// The one self-contradictory combination is the trait's to reject, and this backend
/// routes through it rather than re-deriving the rule.
#[test]
fn opening_for_neither_reading_nor_writing_is_invalid() {
    let vol = holding("", &[("a.txt", "hello")]);
    assert_eq!(
        label(
            &vol.open(Path::new("a.txt"), OpenOptions::default())
                .unwrap_err()
        ),
        "InvalidArgument"
    );
}

/// A directory cannot be opened — including the root, a bare prefix, and a 0-byte
/// marker standing for one. The last is the case that would otherwise read as an
/// empty file.
#[test]
fn opening_a_directory_says_so() {
    let vol = holding("", &[("dir", ""), ("dir/a.txt", "x"), ("plain/b.txt", "y")]);
    for dir in ["", "dir", "plain"] {
        assert_eq!(
            label(
                &vol.open(Path::new(dir), OpenOptions::read_only())
                    .unwrap_err()
            ),
            "IsADirectory",
            "path {dir:?}"
        );
    }
}

#[test]
fn opening_a_missing_key_is_not_found() {
    let vol = holding("", &[("a.txt", "x")]);
    assert_eq!(
        label(
            &vol.open(Path::new("nope"), OpenOptions::read_only())
                .unwrap_err()
        ),
        "NotFound"
    );
}

// -------------------------------------------------------------------- read_at

/// A body long enough to cross a read-ahead window, built so every byte says where
/// it came from — a misplaced copy shows up as wrong content, not just a wrong count.
fn ruler(len: usize) -> String {
    (0..len).map(|i| (b'a' + (i % 26) as u8) as char).collect()
}

fn open_reader(body: &str) -> S3Handle {
    let vol = holding("", &[("big.bin", body)]);
    vol.open(Path::new("big.bin"), OpenOptions::read_only())
        .expect("open")
        .0
}

#[test]
fn a_read_returns_the_bytes_at_that_offset() {
    let body = ruler(1000);
    let handle = open_reader(&body);
    let mut buf = vec![0u8; 100];
    assert_eq!(handle.read_at(&mut buf, 400).expect("read"), 100);
    assert_eq!(buf, body.as_bytes()[400..500]);
}

/// Past the end is EOF, and answered without asking the store — the size the open
/// recorded is enough.
#[test]
fn a_read_past_the_end_is_empty() {
    let handle = open_reader(&ruler(100));
    let mut buf = vec![0u8; 10];
    assert_eq!(handle.read_at(&mut buf, 100).expect("read"), 0);
    assert_eq!(handle.read_at(&mut buf, 1000).expect("read"), 0);
}

/// A request straddling the end is clamped, not rejected. An unclamped range would
/// come back as the store's generic error, indistinguishable from a real failure.
#[test]
fn a_read_crossing_the_end_returns_what_exists() {
    let body = ruler(100);
    let handle = open_reader(&body);
    let mut buf = vec![0u8; 50];
    assert_eq!(handle.read_at(&mut buf, 80).expect("read"), 20);
    assert_eq!(&buf[..20], &body.as_bytes()[80..]);
}

/// An empty request is settled without a fetch — `Bounded(0..0)` is an error at the
/// store — and it must not disturb the sequence, or a zero-length read in the middle
/// of a stream would cost the next read its read-ahead.
#[test]
fn an_empty_read_changes_nothing() {
    let handle = open_reader(&ruler(1000));
    let mut probe: [u8; 0] = [];
    assert_eq!(handle.read_at(&mut probe, 500).expect("read"), 0);
    assert_eq!(
        handle.cache.lock().unwrap().last_end,
        None,
        "an empty read must not enter the sequence"
    );
}

/// The first read cannot be known to be sequential, so it fetches exactly what was
/// asked. Observed through the window, which is what a fetch leaves behind.
#[test]
fn the_first_read_does_not_read_ahead() {
    let handle = open_reader(&ruler(1_000_000));
    let mut buf = vec![0u8; 4096];
    handle.read_at(&mut buf, 0).expect("read");
    let cache = handle.cache.lock().unwrap();
    let (start, data) = cache.window.as_ref().expect("window filled");
    assert_eq!(*start, 0);
    assert_eq!(data.len(), 4096, "no read-ahead without a known sequence");
    assert_eq!(cache.last_end, Some(4096));
}

/// A read that continues where the last ended earns a window: continuity is the
/// signal, not the request size.
#[test]
fn a_contiguous_read_fetches_a_window() {
    let handle = open_reader(&ruler(1_000_000));
    let mut buf = vec![0u8; 4096];
    handle.read_at(&mut buf, 0).expect("first");
    handle.read_at(&mut buf, 4096).expect("second");
    let cache = handle.cache.lock().unwrap();
    let (start, data) = cache.window.as_ref().expect("window filled");
    assert_eq!(*start, 4096);
    assert!(
        data.len() > 4096,
        "a contiguous read should have fetched ahead, got {}",
        data.len()
    );
}

/// A read landing inside the window is served from it. Proven by the window: a fetch
/// would have replaced it.
#[test]
fn a_read_inside_the_window_does_not_fetch() {
    let body = ruler(1_000_000);
    let handle = open_reader(&body);
    let mut buf = vec![0u8; 4096];
    handle.read_at(&mut buf, 0).expect("first");
    handle
        .read_at(&mut buf, 4096)
        .expect("second, fetches ahead");
    let before = handle.cache.lock().unwrap().window.clone();

    handle.read_at(&mut buf, 8192).expect("third, from cache");
    assert_eq!(buf, body.as_bytes()[8192..12288]);
    assert_eq!(
        handle.cache.lock().unwrap().window,
        before,
        "the window should be untouched, so nothing was fetched"
    );
}

/// A read that starts elsewhere breaks the sequence and fetches only what it needs —
/// otherwise random access would pull a window per request.
///
/// The file has to be larger than a window for this to be observable: a jump within
/// one the read-ahead already covers is a cache hit, and rightly fetches nothing.
#[test]
fn a_scattered_read_does_not_fetch_a_window() {
    let jump = READAHEAD_CHUNK + 500_000;
    let handle = open_reader(&ruler(jump as usize + 100_000));
    let mut buf = vec![0u8; 4096];
    handle.read_at(&mut buf, 0).expect("first");
    handle
        .read_at(&mut buf, 4096)
        .expect("second, fetches ahead");
    handle
        .read_at(&mut buf, jump)
        .expect("jump beyond the window");
    let cache = handle.cache.lock().unwrap();
    let (start, data) = cache.window.as_ref().expect("window");
    assert_eq!(*start, jump);
    assert_eq!(data.len(), 4096, "a jump must not fetch ahead");
}

/// A request that begins inside the window and ends past it must still come back whole: a short return means EOF to `PosixFs`, so stopping at
/// the window edge would make the file appear to end there.
///
/// The offsets are deliberately unaligned to the window, which is how the real case
/// arises — a guest's read-ahead ramps through 16K/32K/64K/128K and lands off any
/// fixed boundary.
#[test]
fn a_read_crossing_the_window_edge_is_filled() {
    let size = READAHEAD_CHUNK as usize + 100_000;
    let body = ruler(size);
    let handle = open_reader(&body);

    // Establish a window: two contiguous reads, the second fetching ahead.
    let mut buf = vec![0u8; 7000];
    handle.read_at(&mut buf, 0).expect("first");
    handle
        .read_at(&mut buf, 7000)
        .expect("second, fetches ahead");
    let window_end = {
        let cache = handle.cache.lock().unwrap();
        let (start, data) = cache.window.as_ref().expect("window");
        *start + data.len() as u64
    };

    // A request straddling that edge.
    let at = window_end - 1000;
    let mut buf = vec![0u8; 5000];
    let read = handle.read_at(&mut buf, at).expect("straddling read");
    assert_eq!(
        read, 5000,
        "a partial hit must be filled, not returned short"
    );
    assert_eq!(buf, &body.as_bytes()[at as usize..at as usize + 5000]);
}

/// Sequential streaming over a file larger than one window keeps its read-ahead
/// across every boundary. This is what fails if `last_end` is only updated on a
/// fetch: the hits in between would leave it stale and every boundary would reset.
#[test]
fn streaming_past_a_window_boundary_keeps_reading_ahead() {
    let size = READAHEAD_CHUNK as usize + 500_000;
    let body = ruler(size);
    let handle = open_reader(&body);

    let step = 128 * 1024;
    let mut buf = vec![0u8; step];
    let mut at = 0u64;
    while at < size as u64 {
        let want = step.min(size - at as usize);
        let read = handle.read_at(&mut buf[..want], at).expect("read");
        assert_eq!(read, want, "short read mid-file at offset {at}");
        assert_eq!(
            &buf[..want],
            &body.as_bytes()[at as usize..at as usize + want]
        );
        at += read as u64;
    }

    // Past the boundary the window must be a fetched-ahead one, not a per-request
    // slice — which is only true if continuity survived the run of cache hits.
    let cache = handle.cache.lock().unwrap();
    let (_, data) = cache.window.as_ref().expect("window");
    assert!(
        data.len() > step,
        "read-ahead was lost crossing the boundary, window is {}",
        data.len()
    );
}

// ---------------------------------------------------------------- credentials

/// The secret must not survive a `{:?}`. Asserted rather than left to the doc,
/// because swapping the hand-written impl for a derive would restore the leak and
/// nothing else would notice.
#[test]
fn debugging_a_config_does_not_print_the_secret() {
    let cfg = S3Config {
        bucket: "b".into(),
        region: "r".into(),
        access_key_id: "AKIAEXAMPLE".into(),
        secret_access_key: "n0b0dy-should-see-this".into(),
        endpoint: None,
        key_prefix: None,
    };
    let shown = format!("{cfg:?}");
    assert!(
        !shown.contains("n0b0dy-should-see-this"),
        "secret leaked into Debug output: {shown}"
    );
    assert!(shown.contains("[redacted]"), "no redaction marker: {shown}");
    // The rest stays legible — a redacted Debug is only useful if it still debugs.
    assert!(shown.contains("AKIAEXAMPLE"), "key id should show: {shown}");
    assert!(shown.contains('b'), "bucket should show: {shown}");
}

// ------------------------------------------------------------------ workspace

/// Two path layers meet here, written independently: `Workspace` strips the mount
/// path, then `key()` prepends the key prefix. Neither knows about the other, so this
/// asserts the composition rather than either half.
#[test]
fn a_mount_path_composes_with_the_key_prefix() {
    let vol = holding("bucket-root", &[("bucket-root/a.txt", "hello")]);
    let ws = crate::volume::Workspace::new()
        .try_with_mount("data", vol)
        .expect("mount");

    let stat = Mountable::stat(&ws, Path::new("data/a.txt")).expect("stat through workspace");
    assert_eq!(stat.size, 5, "mount path stripped, key prefix prepended");

    // And the composition does not open a way past the prefix.
    assert_eq!(
        label(&Mountable::stat(&ws, Path::new("data/../a.txt")).unwrap_err()),
        "NotFound",
        "`..` is resolved by the workspace before the backend sees it"
    );
}

/// The point of `Mountable for Arc<T>`: one volume served two ways at once. A
/// workspace takes its backend by value, so without the `Arc` impl a bucket could
/// feed exactly one consumer.
#[test]
fn a_shared_volume_serves_a_workspace_and_its_owner() {
    let vol = Arc::new(holding("", &[("a.txt", "hello")]));
    let scratch = crate::volume::InMemVolume::new();
    let ws = crate::volume::Workspace::new()
        .try_with_mount("s3", Arc::clone(&vol))
        .expect("mount s3")
        .try_with_mount("scratch", scratch)
        .expect("mount scratch");

    // Through the workspace, beside a different backend.
    assert_eq!(
        Mountable::stat(&ws, Path::new("s3/a.txt"))
            .expect("stat")
            .size,
        5
    );
    // And directly, through the handle the caller kept.
    assert_eq!(vol.stat(Path::new("a.txt")).expect("stat").size, 5);

    // The synthesized root names both mounts.
    let mut names: Vec<_> = Mountable::list(&ws, Path::new(""))
        .expect("list root")
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, ["s3", "scratch"]);
}

/// A read through the workspace goes through its erased handle
/// (`Handle = Box<dyn FileHandle>`), which is a different path from the concrete one
/// every other test here takes.
#[test]
fn a_read_through_a_workspace_reaches_the_object() {
    let body = ruler(9000);
    let vol = holding("", &[("big.bin", &body)]);
    let ws = crate::volume::Workspace::new()
        .try_with_mount("s3", vol)
        .expect("mount");

    let (handle, stat) = Mountable::open(&ws, Path::new("s3/big.bin"), OpenOptions::read_only())
        .expect("open through workspace");
    assert_eq!(stat.size, 9000);
    let mut buf = vec![0u8; 5000];
    assert_eq!(handle.read_at(&mut buf, 2000).expect("read"), 5000);
    assert_eq!(buf, &body.as_bytes()[2000..7000]);
}

/// The window's contents must not survive a `{:?}` — a derive would print megabytes
/// of file data. Asserted for the same reason as the config's secret: swapping the
/// hand-written impl for a derive would restore the leak and nothing else would
/// notice.
#[test]
fn debugging_a_handle_does_not_print_the_window() {
    let body = ruler(200_000);
    let handle = open_reader(&body);
    let mut buf = vec![0u8; 4096];
    handle.read_at(&mut buf, 0).expect("first");
    handle
        .read_at(&mut buf, 4096)
        .expect("second, fetches ahead");

    let shown = format!("{handle:?}");
    assert!(
        !shown.contains(&body[..64]),
        "the window's bytes reached Debug: {shown}"
    );
    assert!(shown.contains("window"), "the extent is worth reporting");
    assert!(shown.len() < 500, "Debug should be an extent, not a body");
}

// ------------------------------------------------------------- a real mount

/// The one place a kernel asks. Everything above calls `read_at` with sizes and
/// offsets *this file* chose; here the operating system chooses them, which matters
/// because the read-ahead policy is a response to what the kernel actually sends.
///
/// It also exercises two things no unit test reaches:
///
/// * the `block_on` precondition — the mount's session thread is not a Tokio worker,
///   which the module doc relies on and nothing else here proves;
/// * the errno userspace really sees, rather than the `CortexError` on the way to it.
///
/// The store is `InMemory` because no S3 is available, but the layer under test is
/// unchanged: `S3Volume` reaches it through the same `dyn ObjectStore`.
///
/// Lives here rather than in `tests/host_mount.rs` because `with_store` is private
/// (see its doc) and an integration test is a separate crate. `#[ignore]` for the
/// same reasons that file gives, and **`--test-threads=1` is required** — FUSE-T
/// serves through a helper process and several coming up at once wedges:
///
/// ```sh
/// PKG_CONFIG_PATH="$PWD/contrib/pkgconfig:/usr/local/lib/pkgconfig" \
///     cargo test --features s3,fuse-t --lib -- --ignored --test-threads=1 mounted
/// ```
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
mod mounted {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use crate::volume::HostMount;

    fn mountpoint(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cortex-s3-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir is writable");
        dir
    }

    /// A bucket shaped like the cases this backend had to reason about: a file, a
    /// prefix, a zero-byte folder marker with children, and a body long enough to
    /// cross a read-ahead window.
    fn bucket(big: &str) -> S3Volume {
        holding(
            "",
            &[
                ("greeting.txt", "Hello from an object store!\n"),
                ("dir/nested.txt", "nested\n"),
                ("marker", ""),
                ("marker/inside.txt", "inside\n"),
                ("big.bin", big),
            ],
        )
    }

    #[test]
    #[ignore = "needs a libfuse provider and mounts a real filesystem"]
    fn the_operating_system_can_read_an_object_store_mount() {
        let big = ruler(READAHEAD_CHUNK as usize + 300_000);
        let mnt = mountpoint("read");
        let mount = HostMount::spawn(bucket(&big), &mnt).expect("mount");

        // A real `readdir`, with the collision rules applied by the kernel's rules.
        let mut names: Vec<_> = fs::read_dir(&mnt)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["big.bin", "dir", "greeting.txt", "marker"]);

        assert_eq!(
            fs::read_to_string(mnt.join("greeting.txt")).expect("read file"),
            "Hello from an object store!\n"
        );

        // A prefix is a directory the OS can descend into.
        assert_eq!(
            fs::read_to_string(mnt.join("dir/nested.txt")).expect("read nested"),
            "nested\n"
        );

        // End to end: a zero-byte object standing for a folder has to be traversable, or this path does not resolve at all.
        assert!(
            fs::metadata(mnt.join("marker"))
                .expect("stat marker")
                .is_dir(),
            "a marker with children must look like a directory to the OS"
        );
        assert_eq!(
            fs::read_to_string(mnt.join("marker/inside.txt")).expect("read inside"),
            "inside\n"
        );

        // The kernel's own request sizes, across more than one window, with content
        // checked — the path a window-boundary short read would break.
        let read = fs::read(mnt.join("big.bin")).expect("read big");
        assert_eq!(
            read.len(),
            big.len(),
            "the file must not appear to end early"
        );
        assert_eq!(read, big.as_bytes());

        mount.unmount().expect("unmount");
        fs::remove_dir_all(&mnt).ok();
    }

    /// Every unit test above asserts a `CortexError`; this asserts what userspace is
    /// actually told, which is the only thing tools act on.
    #[test]
    #[ignore = "needs a libfuse provider and mounts a real filesystem"]
    fn the_operating_system_is_told_the_mount_is_read_only() {
        let mnt = mountpoint("ro");
        let mount = HostMount::spawn(bucket("x"), &mnt).expect("mount");

        let denied = [
            fs::write(mnt.join("new.txt"), b"nope").err(),
            fs::write(mnt.join("greeting.txt"), b"nope").err(),
            fs::create_dir(mnt.join("newdir")).err(),
            fs::remove_file(mnt.join("greeting.txt")).err(),
            fs::rename(mnt.join("greeting.txt"), mnt.join("moved.txt")).err(),
        ];
        for err in denied {
            let err = err.expect("a write on a read-only mount must fail");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::ReadOnlyFilesystem,
                "userspace should hear EROFS, not {err:?}"
            );
        }

        mount.unmount().expect("unmount");
        fs::remove_dir_all(&mnt).ok();
    }
}

// ------------------------------------------------------- what this cannot test

/// The shape these tests *cannot* reach, recorded so the gap is visible rather than
/// assumed away.
///
/// An object store console writes its folder marker with a trailing slash — `dir/`,
/// not `dir`. `Path` does not carry one: its own contract is "no leading or trailing
/// delimiters", and `parse` strips it. So the two spellings collapse to one key here,
/// and the console's convention cannot be represented in the in-memory store at all.
///
/// What that costs: at its parent's level a real `dir/` key rolls up into a
/// `CommonPrefix` and does *not* appear among the objects, so the collision these
/// tests exercise never arises for it. Only a slash-less `dir` — which the console
/// cannot produce but the API can — reaches [`S3Volume::resolve_collision`]. That
/// branch is what is covered above; the other rests on `object_store`'s source and
/// AWS's documented `Delimiter` behaviour.
#[test]
fn a_trailing_slash_cannot_be_stored_so_the_console_shape_is_untested() {
    assert_eq!(
        OsPath::parse("dir/sub/").expect("parses").as_ref(),
        "dir/sub",
        "the trailing slash is stripped, so `dir/sub/` and `dir/sub` are one key"
    );
}

// -------------------------------------------------------------------- plumbing

/// One runtime for the whole crate, however many volumes exist.
#[test]
fn every_volume_shares_one_runtime() {
    let first = runtime().expect("runtime") as *const Runtime;
    let second = runtime().expect("runtime") as *const Runtime;
    assert_eq!(first, second);
}
