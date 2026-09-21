//! A store that is one SQLite file: what its tables are, how one is made, how one is opened,
//! and how a file that is not one is refused.
//!
//! # One schema, two kinds
//!
//! `mem` and `index` keep the same tables. What differs is which columns they fill:
//!
//! | | `mem` | `index` |
//! |---|---|---|
//! | `item.id` | a UUID | null |
//! | `item.path` | null | the path it was given under |
//! | `item.title` / `mtime` / `len` | left at their defaults | the file's name and stamp |
//! | `item_fts.title` | empty | the file's name |
//!
//! **A column a kind does not fill costs it nothing.** An FTS5 column with no tokens in it
//! contributes nothing to `bm25` — measured over a corpus where scores actually separate, the
//! ranking and the scores are identical to eight decimals with the column present or absent —
//! and it matches nothing, so it cannot produce a hit. `unique` on a nullable column is the
//! same story: SQLite allows any number of nulls, so `mem` can hold the same sentence
//! twice while `path` still makes a document's identity, which is what makes `ingest`
//! idempotent.
//!
//! # `kind`, and why the version is not enough
//!
//! The tables are the same either way, so structure cannot say which command wrote a file.
//! `meta.kind` does: [`create`] writes it and [`open`] insists on it, so an `index` handed a
//! `mem` file is told what it is rather than answering with rows whose `path` is null.
//!
//! `schema_version` alone could not carry it. It says which shape a file has, and both kinds
//! have the same one, so a match across two kinds is a mismatch nobody catches until a column
//! reads null three statements later.
//!
//! # `meta`, and what will go in it
//!
//! `schema_version` and `kind` are read before any other table is touched. It is also where the
//! keys that outlive one schema go: a store that grows vectors needs to record which model
//! produced them and at what dimension, and both kinds want that spelled the same way —
//! otherwise whatever fills `chunk.embedding` has to know which kind of store it is filling.
//!
//! # What a store's content must be
//!
//! **The text a store holds is the text it was given.** Not a normalized form of it, not a list
//! of terms a tokenizer made of it — the original, with its case and its punctuation.
//!
//! This is the rule that keeps the tokenizer a decision rather than a commitment. An index is
//! derived: change how text is cut and it can be rebuilt from the store itself, with no
//! re-ingest and nothing to go and fetch again. A store that held lemmas instead could not be
//! rebuilt at all — the case, the punctuation and (for a language that has to be segmented) the
//! word boundaries would already be gone, and no later pass could put them back.
//!
//! # No write-ahead log
//!
//! A store may well sit on a FUSE mount, and WAL mode does not survive that possibility: the
//! log's index is a shared-memory file that every connection
//! `mmap`s and expects to be coherent between processes. A FUSE filesystem is under no
//! obligation to give them that, and the failure is silent — two writers each see their own
//! index. So a store stays on the rollback journal SQLite starts in, which needs nothing but
//! the ability to create a sibling file, and concurrent writers wait on [`BUSY_TIMEOUT`].

use std::{io, path::Path};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

/// How long a connection waits for another writer before giving up.
///
/// A rollback-journal store serializes writers, and these executables are invoked once per
/// command: the contention that happens is two commands overlapping by a moment, not a queue.
/// Waiting is therefore almost always the right answer, and a bound is still needed because the
/// other side may be a process that died holding the lock.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Every table a store is made of — `queries/init.sql`.
///
/// A file rather than a string literal, because what it describes is a file format: a store
/// outlives this build, and somebody holding one will want to read it with `sqlite3` or write a
/// tool that does.
///
/// See the module docs for which columns each kind fills. `chunk` is created empty by both —
/// there is no embedder in this workspace yet, and choosing a chunk size before there is a
/// model to size chunks for would be deciding on no evidence.
pub const SCHEMA: &str = include_str!("../queries/init.sql");

/// What this schema is; refuse a file written by a newer one.
pub const SCHEMA_VERSION: &str = "1";

/// One item written — `queries/upsert.sql`.
///
/// **One statement for both kinds, and the difference between them is the data.** `mem`
/// binds a null `path`, so `on conflict(path)` can never fire — a null collides with nothing in
/// a `unique` index — and every call inserts, which is why the same sentence can be remembered
/// twice. `index` binds the path the document was given under, so a second `ingest` of it
/// lands on the conflict clause and updates in place.
///
/// The rowid has to survive that update: `item_fts` is joined to it, and a replacement that
/// renumbered the row would leave the text filed under a number nothing points at. `returning
/// rowid` hands back the one to file the text under, whether the row was new or already there.
///
/// A conflict on `id` is deliberately *not* handled. It is a UUID, so a collision is not a
/// second write of the same thing — it is a bug, and an error is the right answer to it.
pub const UPSERT: &str = include_str!("../queries/upsert.sql");

/// The text of one item — `queries/fts_write.sql`, `queries/fts_clear.sql`.
///
/// Two statements because FTS5 has no upsert: a row is cleared and written again. Both kinds
/// write text the same way, whatever they then do with the row beside it, so the pair lives
/// here rather than twice.
pub const FTS_WRITE: &str = include_str!("../queries/fts_write.sql");
pub const FTS_CLEAR: &str = include_str!("../queries/fts_clear.sql");

const VERSION_KEY: &str = "schema_version";
const KIND_KEY: &str = "kind";

/// Make the store at `path`, as a store of `kind`.
///
/// [`AlreadyExists`](io::ErrorKind::AlreadyExists) when there is already a file there. Making a
/// store is the one thing that brings one into being, so being handed a name that is taken is
/// an ambiguity and not a detail to smooth over: the caller either meant a store they already
/// have — in which case they wanted the command that writes to one — or they meant a new one
/// and named the wrong file.
///
/// **Nothing usable is left behind on failure.** A file that exists but has no schema in it
/// would make every retry of the same command fail as "already exists" — a store that does not
/// exist, standing in the way of creating itself.
pub fn create(path: &Path, kind: &str) -> io::Result<Connection> {
    // The absence of the file *is* the check, so it is made by the creation rather than by a
    // `try_exists` before it: two calls racing for one name both see nothing there, and only an
    // exclusive create makes one of them lose.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;

    // A closure so that every step below can `?` and still be undone in one place. From here on
    // the file exists, and each of these can fail on it.
    let installed = (|| -> io::Result<Connection> {
        // No `SQLITE_OPEN_CREATE`: the file was made a line ago. Asking SQLite to make it again
        // would turn a path that went missing in between — a race, a tree unmounted underneath —
        // into a second empty store rather than the error it is.
        let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;

        // No `if not exists`, and no transaction around the statements: the file is one this
        // call made and nothing else has seen it, so there is nothing to collide with and
        // nothing a half-written schema could damage. What plays the part of a rollback is the
        // removal below, which undoes the file rather than the statements in it.
        conn.execute_batch(SCHEMA).map_err(sql_error)?;
        for (key, value) in [(VERSION_KEY, SCHEMA_VERSION), (KIND_KEY, kind)] {
            conn.execute("insert into meta (key, value) values (?1, ?2)", (key, value))
                .map_err(sql_error)?;
        }
        Ok(conn)
    })();

    installed.inspect_err(|_| {
        std::fs::remove_file(path).ok();
    })
}

/// Open the store that is already at `path`, insisting it is a `kind` of this schema version.
///
/// [`NotFound`](io::ErrorKind::NotFound) when there is no file there. Every command but the one
/// that creates a store is about what is in it, and what is in it was put there by somebody:
/// creating one here would turn a misspelled name into an empty store that answers every search
/// with nothing, while what the caller meant sits in the file they meant to name.
///
/// [`InvalidData`](io::ErrorKind::InvalidData) for a file that is not one of these — somebody
/// else's SQLite database, a schema this build does not speak, or **a store of the other
/// kind**. All three are refused here rather than left to fail three statements later, where
/// the message would be about SQL and not about the file that was named.
pub fn open(path: &Path, kind: &str) -> io::Result<Connection> {
    // Asked of the filesystem before SQLite, for two reasons that happen to have one answer.
    // SQLite reports a file that is not there and a file it may not read with the same code,
    // which would make "no such store" the message for a permission problem; and it says so in
    // words about SQL rather than about the file that was named. So the ordinary case is
    // answered here, and anything stranger keeps SQLite's own words.
    if !path.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "there is no store here; `init` makes one",
        ));
    }

    let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);

    match said(&conn, VERSION_KEY)? {
        Some(found) if found == SCHEMA_VERSION => {}
        // Both versions, because neither alone tells the caller which side is old: a store from
        // a newer build and a build newer than the store are the same sentence read in opposite
        // directions, and what they do about it differs.
        Some(found) => {
            return Err(invalid(format!(
                "this store is schema version {found}; this one speaks {SCHEMA_VERSION}"
            )));
        }
        None => return Err(invalid("not a store: it does not say which schema it is".into())),
    }

    match said(&conn, KIND_KEY)? {
        Some(found) if found == kind => Ok(conn),
        // The whole reason `kind` is written down: the tables are the same either way, so this
        // is the only thing that can tell the caller they named the wrong file rather than
        // handing them rows whose columns are all null.
        Some(found) => Err(invalid(format!("this store's kind is {found}; {kind} was asked for"))),
        None => Err(invalid("not a store: it does not say what kind it is".into())),
    }
}

/// Whether `path` is a store of `kind` at this schema version, decided by looking rather than by
/// opening for writing.
///
/// What a `list` sifts a directory with, and what a `drop` asks before it deletes anything. It
/// answers `false` for everything it is unsure about — a file that is not SQLite, one that is
/// somebody else's database, one that cannot be read, one of the other kind — because both
/// callers treat a `false` as "leave it alone", which is the safe reading of every one of those.
pub fn is_store(path: &Path, kind: &str) -> bool {
    if !path.is_file() {
        return false;
    }
    // Read-only, so that sifting a directory cannot create or journal anything in it.
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    matches!(said(&conn, VERSION_KEY), Ok(Some(v)) if v == SCHEMA_VERSION)
        && matches!(said(&conn, KIND_KEY), Ok(Some(k)) if k == kind)
}

/// What a store says about itself under `key`, or `None` for a file that does not say.
///
/// Asked of the catalogue before it is asked of `meta`, because "no such table" arrives as the
/// same error code as a dozen unrelated things: a file that was never a store would otherwise be
/// reported as a broken one.
pub fn said(conn: &Connection, key: &str) -> io::Result<Option<String>> {
    let ours: bool = conn
        .query_row(
            "select count(*) from sqlite_master where type = 'table' and name = 'meta'",
            [],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )
        .map_err(sql_error)?;
    if !ours {
        return Ok(None);
    }
    conn.query_row("select value from meta where key = ?1", (key,), |row| {
        row.get(0)
    })
    .optional()
    .map_err(sql_error)
}

/// Open one connection, with the pragmas a store that may sit on a FUSE mount needs.
pub fn connect(path: &Path, flags: OpenFlags) -> io::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags).map_err(sql_error)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_error)?;
    // Off by default in SQLite, and `chunk` declares a reference it means.
    conn.execute_batch("pragma foreign_keys = on")
        .map_err(sql_error)?;
    Ok(conn)
}

/// A SQLite failure as these crates answer in.
///
/// Two codes are worth a kind of their own, because they are the two a caller can act on: a file
/// that is not there, and a tree that will not be written to. Everything else is a database
/// error whose own message says more than a kind would.
pub fn sql_error(e: rusqlite::Error) -> io::Error {
    use rusqlite::ErrorCode;
    match &e {
        rusqlite::Error::SqliteFailure(f, _) => match f.code {
            ErrorCode::CannotOpen => io::Error::new(io::ErrorKind::NotFound, e),
            ErrorCode::ReadOnly => io::Error::new(io::ErrorKind::PermissionDenied, e),
            _ => io::Error::other(e),
        },
        _ => io::Error::other(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        dir.path().join(name)
    }

    /// `kind` is written down because the tables are the same either way: nothing else could
    /// tell a store of one kind from a store of the other.
    #[test]
    fn a_new_store_says_which_schema_and_kind_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = at(&dir, "s.db");
        let conn = create(&path, "mem").expect("a store can be made");
        assert_eq!(said(&conn, VERSION_KEY).unwrap().as_deref(), Some("1"));
        assert_eq!(said(&conn, KIND_KEY).unwrap().as_deref(), Some("mem"));
        // The shared tables are there, and a row can be written under either kind's columns.
        conn.execute(
            "insert into item (id, written_at) values ('u', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("a mem row");
        conn.execute(
            "insert into item (path, title, mtime, len, written_at)
             values ('/a.md', 'a.md', 1, 2, '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("an index row");
        drop(conn);

        open(&path, "mem").expect("its own kind opens");
        let e = open(&path, "index").expect_err("the other kind does not");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(is_store(&path, "mem"));
        assert!(!is_store(&path, "index"));
    }

    /// A nullable `unique` column takes any number of nulls, which is what lets one schema hold
    /// both "the same sentence twice is two memories" and "a path is a document's identity".
    #[test]
    fn nulls_do_not_collide_but_a_repeated_path_does() {
        let dir = tempfile::tempdir().unwrap();
        let conn = create(&at(&dir, "s.db"), "mem").unwrap();

        for _ in 0..3 {
            conn.execute("insert into item (written_at) values ('t')", [])
                .expect("a memory with no path");
        }
        conn.execute("insert into item (path, written_at) values ('/a.md', 't')", [])
            .expect("a document");
        conn.execute("insert into item (path, written_at) values ('/a.md', 't')", [])
            .expect_err("the same path twice");
    }

    /// Both versions in the message, because neither alone says which side is old.
    #[test]
    fn a_store_from_another_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = at(&dir, "newer.db");
        let conn = create(&path, "mem").unwrap();
        conn.execute("update meta set value = '99' where key = ?1", (VERSION_KEY,))
            .unwrap();
        drop(conn);

        let e = open(&path, "mem").expect_err("99 is not a schema this speaks");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("99"), "{e}");
        assert!(e.to_string().contains(SCHEMA_VERSION), "{e}");
        assert!(!is_store(&path, "mem"), "nor one to sift into a list");
    }

    /// One statement, two behaviours, and the data is what picks. This is the whole of why
    /// `mem` and `index` do not each need a write of their own.
    #[test]
    fn one_upsert_inserts_for_a_memory_and_updates_for_a_document() {
        let dir = tempfile::tempdir().unwrap();
        let conn = create(&at(&dir, "s.db"), "mem").unwrap();
        let mut up = conn.prepare(UPSERT).unwrap();
        let write = |up: &mut rusqlite::Statement<'_>, id, path, title, mtime, len, at| -> i64 {
            up.query_row(
                (id, path, title, mtime, len, at),
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };

        // A memory: no path, so nothing to conflict with. The same text twice is two rows.
        let a = write(&mut up, Some("u1"), None::<&str>, "", 0, 0, "t");
        let b = write(&mut up, Some("u2"), None::<&str>, "", 0, 0, "t");
        assert_ne!(a, b, "nulls do not collide, so both were inserted");

        // A document: the path is its identity, so the second write lands on the conflict
        // clause — same row, new stamp.
        let c1 = write(&mut up, None, Some("/a.md"), "a.md", 100, 10, "t1");
        let c2 = write(&mut up, None, Some("/a.md"), "a.md", 200, 20, "t2");
        assert_eq!(c1, c2, "the rowid survives, which is what item_fts is filed under");

        let (mtime, len, at): (i64, i64, String) = conn
            .query_row(
                "select mtime, len, written_at from item where path = '/a.md'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((mtime, len, at.as_str()), (200, 20, "t2"), "and it was updated");
        assert_eq!(
            conn.query_row("select count(*) from item", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3,
            "two memories and one document"
        );
    }

    #[test]
    fn a_store_is_never_made_over_something_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = at(&dir, "taken.db");
        std::fs::write(&path, b"someone else's").unwrap();

        let e = create(&path, "mem").expect_err("the name is taken");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"someone else's",
            "and what was there is untouched"
        );
    }

    #[test]
    fn a_store_that_is_not_there_is_not_made_by_opening_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = at(&dir, "missing.db");
        let e = open(&path, "mem").expect_err("there is no store");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
    }

    #[test]
    fn a_file_that_is_not_a_store_is_refused_as_one() {
        let dir = tempfile::tempdir().unwrap();

        let other = at(&dir, "other.db");
        Connection::open(&other)
            .unwrap()
            .execute_batch("create table something_else (id integer primary key)")
            .unwrap();
        let e = open(&other, "mem").expect_err("not one of ours");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(!is_store(&other, "mem"));

        // Not a database at all: SQLite's own complaint, and not a claim that the file is fine.
        let prose = at(&dir, "notes.txt");
        std::fs::write(&prose, b"dear diary").unwrap();
        open(&prose, "mem").expect_err("prose is not a store");
        assert!(!is_store(&prose, "mem"));
    }

    /// A directory is not a store, and asking must not create or journal anything.
    #[test]
    fn is_store_is_false_for_anything_it_cannot_read_as_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_store(dir.path(), "mem"));
        assert!(!is_store(&at(&dir, "nothing-here.db"), "mem"));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "asking left nothing behind"
        );
    }
}
