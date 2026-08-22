//! The memory store: one SQLite file, holding what a store knows about itself and — as the
//! commands that write them arrive — the memories, their vectors and their history.
//!
//! # One file, and why that decides most of this
//!
//! A store is named the way a document is — `mem insert notes.sqlite "..."` — because it lives
//! in a cortex tree, beside whatever else the session works on. Everything a memory needs is
//! therefore in that one file: the rows, the vector index over them, and the record of what
//! happened to each. Nothing here reaches a service, and copying the file copies the memory.
//!
//! # No write-ahead log
//!
//! The file usually sits on a mounted cortex tree, which is a FUSE mount, and WAL mode does not
//! survive that assumption: the log's index is a shared-memory file that every connection
//! `mmap`s and expects to be coherent between processes. A FUSE filesystem is under no
//! obligation to give them that, and the failure is silent — two writers each see their own
//! index. So the store stays on the rollback journal SQLite starts in, which needs nothing but
//! the ability to create a sibling file, and concurrent writers wait on
//! [`BUSY_TIMEOUT`](self::BUSY_TIMEOUT) rather than racing.

use std::{io, path::Path, sync::Mutex};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

/// How long a connection waits for another writer before giving up.
///
/// A rollback-journal store serializes writers, and `mem` is invoked once per command: the
/// contention that happens is two commands overlapping by a moment, not a queue. Waiting is
/// therefore almost always the right answer, and a bound is still needed because the other side
/// may be a process that died holding the lock.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What this crate's schema is; refuse a file written by a newer one.
const SCHEMA_VERSION: &str = "1";

/// An open memory store.
///
/// Every method here is synchronous and blocks: this is SQLite, and a vector search is a C
/// extension running in the calling thread. Keeping that off a task's thread is the caller's
/// job, so nothing in this module needs to know a runtime exists.
///
/// [`Send`] + [`Sync`] through the mutex, which is what lets a caller hold one in an
/// [`Arc`](std::sync::Arc) and hand it to a blocking thread per call. A `Connection` is not
/// itself `Sync`, and serializing is not a cost worth avoiding: a rollback-journal file
/// serializes writers anyway.
#[derive(Debug)]
pub struct Store {
    /// The one connection, taken under a lock by whatever asks the file a question.
    // An expectation and not an allow: `insert` and `search` are what ask, and the day either
    // arrives this becomes an error asking to be deleted.
    #[expect(dead_code, reason = "nothing asks the file a question yet")]
    conn: Mutex<Connection>,
}

impl Store {
    /// Make the store at `path`, and write into it the facts that hold for its whole life.
    ///
    /// [`AlreadyExists`](io::ErrorKind::AlreadyExists) when there is already a file at `path`.
    /// This is the one command that brings a store into being, so being handed a name that is
    /// taken is an ambiguity and not a detail to smooth over: the caller either meant a store
    /// they already have — in which case they wanted `insert` — or they meant a new one and
    /// named the wrong file.
    pub fn try_new(path: impl AsRef<Path>) -> io::Result<Store> {
        let path = path.as_ref();
        // The absence of the file *is* the check, so it is made by the creation rather than by a
        // `try_exists` before it: two `mem init`s racing for one name both see nothing there, and
        // only an exclusive create makes one of them lose.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;

        // A closure so that every step below can `?` and still be undone in one place. From here
        // on the file exists, and each of these can fail on it.
        let installed = (|| -> io::Result<Store> {
            // No `CREATE`: the file was made a line ago. Asking SQLite to make it again would
            // turn a path that went missing in between — a race, a tree unmounted underneath —
            // into a second empty store rather than the error it is.
            let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;

            // No `if not exists`, and no transaction around the two statements: the file is one
            // this call made and nothing else has seen it, so there is nothing to collide with
            // and nothing a half-written schema could damage. What plays the part of a rollback
            // is the removal below, which undoes the file rather than the statements in it.
            conn.execute_batch("create table meta (key text primary key, value text not null)")
                .map_err(sql_error)?;
            conn.execute(
                "insert into meta (key, value) values ('schema_version', ?1)",
                (SCHEMA_VERSION,),
            )
            .map_err(sql_error)?;

            Ok(Store {
                conn: Mutex::new(conn),
            })
        })();

        // Nothing was here a moment ago and nothing usable is here now. Leaving the file behind
        // would make every retry of this exact command fail as "already exists" — a store that
        // does not exist, standing in the way of creating itself.
        installed.inspect_err(|_| {
            std::fs::remove_file(path).ok();
        })
    }

    /// Open the store that is already at `path`.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) when there is no file there. Every command but
    /// `init` is about memories, and memories are in a store somebody made: creating one here
    /// would turn a misspelled name into an empty store that answers every search with nothing,
    /// while the memories the caller meant sit in the file they meant to name.
    ///
    /// [`InvalidData`](io::ErrorKind::InvalidData) for a file that is not this: somebody else's
    /// SQLite database, or a store written by a `mem` whose schema this one does not speak. Both
    /// are refused here rather than left to fail as a missing column three statements later,
    /// where the message would be about SQL and not about the file that was named.
    ///
    /// Opened for writing, because that is what the commands that load a store do to it.
    pub fn try_from_file(path: impl AsRef<Path>) -> io::Result<Store> {
        let path = path.as_ref();

        // Asked of the filesystem before SQLite, for two reasons that happen to have one answer.
        // SQLite reports a file that is not there and a file it may not read with the same code,
        // which would make "no such store" the message for a permission problem; and its text
        // carries the host path, which is on the far side of a mount the caller cannot see. So
        // the ordinary case is answered here, and anything stranger keeps SQLite's own words.
        if !path.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "there is no store here; `mem init` makes one",
            ));
        }

        let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;

        // Asked of the catalogue rather than by reading `meta` and interpreting the failure:
        // "no such table" arrives as the same error code as a dozen unrelated things, so a file
        // that was never a store would otherwise be reported as a broken one.
        let ours: bool = conn
            .query_row(
                "select count(*) from sqlite_master where type = 'table' and name = 'meta'",
                [],
                |row| Ok(row.get::<_, i64>(0)? > 0),
            )
            .map_err(sql_error)?;
        if !ours {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a mem store: it has no meta table",
            ));
        }

        let version: Option<String> = conn
            .query_row(
                "select value from meta where key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;

        match version.as_deref() {
            Some(SCHEMA_VERSION) => Ok(Store {
                conn: Mutex::new(conn),
            }),
            // Both versions, because neither alone tells the caller which side is old: a store
            // from a newer `mem` and a `mem` newer than the store are the same sentence read in
            // opposite directions, and what they do about it differs.
            Some(other) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("this store is schema version {other}; this mem speaks {SCHEMA_VERSION}"),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a mem store: its meta table does not say which schema it is",
            )),
        }
    }
}

/// Open one connection, with the pragmas this store needs.
fn connect(path: &Path, flags: OpenFlags) -> io::Result<Connection> {
    let conn = Connection::open_with_flags(path, flags).map_err(sql_error)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_error)?;
    Ok(conn)
}

/// A SQLite failure as the rest of the crate answers in.
///
/// Two codes are worth a kind of their own, because they are the two a caller can act on: a file
/// that is not there, and a tree that will not be written to. Everything else is a database
/// error whose own message says more than a kind would.
fn sql_error(e: rusqlite::Error) -> io::Error {
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

    /// What a store says about itself, read off the file rather than off the handle that made
    /// it: what `init` leaves behind is read by the next command, in another process.
    fn meta(path: &Path) -> std::collections::BTreeMap<String, String> {
        let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("the file opens");
        let mut stmt = conn.prepare("select key, value from meta").unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn a_new_store_says_which_schema_it_is() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");

        Store::try_new(&path).expect("a store can be made");

        let said = meta(&path);
        assert_eq!(
            said.get("schema_version").map(String::as_str),
            Some(SCHEMA_VERSION)
        );
    }

    /// What `init` writes is what the next command opens.
    #[test]
    fn a_store_that_was_made_here_can_be_opened_again() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");

        Store::try_new(&path).expect("a store can be made");
        Store::try_from_file(&path).expect("and opened again, in another call");
    }

    /// A store that is not there is not made: the name was wrong, and an empty store would
    /// answer every question with nothing while the memories sat where they always were.
    #[test]
    fn a_store_that_is_not_there_is_not_made_by_opening_it() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");

        let e = Store::try_from_file(&path).expect_err("there is no such store");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(!path.try_exists().unwrap(), "opening made nothing");
    }

    /// Somebody else's file is said to be somebody else's, rather than failing later as SQL
    /// about a column nobody named.
    #[test]
    fn a_file_that_is_not_a_store_is_refused_as_one() {
        let dir = tempfile::tempdir().expect("a temporary directory");

        let other = dir.path().join("other.sqlite");
        Connection::open(&other)
            .unwrap()
            .execute_batch("create table something_else (id integer primary key)")
            .unwrap();
        let e = Store::try_from_file(&other).expect_err("this is not a mem store");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("meta"), "{e}");

        // Not a database at all: SQLite's own complaint, and not a claim that the file is fine.
        let prose = dir.path().join("notes.txt");
        std::fs::write(&prose, b"dear diary").unwrap();
        Store::try_from_file(&prose).expect_err("prose is not a store");
    }

    /// A schema this build does not speak is refused, and the refusal says both versions:
    /// which side is old is the caller's next question.
    #[test]
    fn a_store_from_another_schema_is_refused() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("newer.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "create table meta (key text primary key, value text not null);
             insert into meta (key, value) values ('schema_version', '99')",
        )
        .unwrap();
        drop(conn);

        let e = Store::try_from_file(&path).expect_err("99 is not a schema this speaks");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("99"), "{e}");
        assert!(e.to_string().contains(SCHEMA_VERSION), "{e}");
    }

    /// A name that is taken is an ambiguity, and the existing file is left exactly as it was.
    #[test]
    fn a_store_is_never_made_over_something_already_there() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");
        std::fs::write(&path, b"not a store, and not to be lost either").unwrap();

        let e = Store::try_new(&path).expect_err("the name is taken");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"not a store, and not to be lost either"
        );
    }

    /// Nowhere to put it is a failure that names the path, not a store made somewhere else.
    #[test]
    fn a_store_cannot_be_made_in_a_directory_that_is_not_there() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let e = Store::try_new(dir.path().join("nope").join("notes.sqlite"))
            .expect_err("there is no such directory");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    /// A tree that will not be written to is said to be, and nothing is left at the name.
    #[cfg(unix)]
    #[test]
    fn a_store_cannot_be_made_where_nothing_can_be_written() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");

        let mut mode = std::fs::metadata(dir.path()).unwrap().permissions();
        mode.set_mode(0o500);
        std::fs::set_permissions(dir.path(), mode.clone()).unwrap();

        let refused = Store::try_new(&path);

        // Put back before the assertions, so a failing one does not leave a directory the
        // harness cannot clean up.
        mode.set_mode(0o700);
        std::fs::set_permissions(dir.path(), mode).unwrap();

        let e = refused.expect_err("nothing can be written here");
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(!path.try_exists().unwrap(), "nothing was made");
    }
}
