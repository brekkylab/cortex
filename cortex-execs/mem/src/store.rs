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

use crate::memory::{Memory, terms};

/// How long a connection waits for another writer before giving up.
///
/// A rollback-journal store serializes writers, and `mem` is invoked once per command: the
/// contention that happens is two commands overlapping by a moment, not a queue. Waiting is
/// therefore almost always the right answer, and a bound is still needed because the other side
/// may be a process that died holding the lock.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What this crate's schema is; refuse a file written by a newer one.
const SCHEMA_VERSION: &str = "1";

/// Every table a store is made of — `queries/init.sql`.
///
/// The statements are a file rather than a string literal, like every other query here. What
/// they describe is a file format: a store outlives this build, and somebody holding one will
/// want to read it with `sqlite3` or write a tool that does. A schema that can be opened, read
/// and diffed on its own is worth more to them than one spelled inside Rust quotes, and keeping
/// it out here is also what keeps the argument below from being interleaved with the DDL it is
/// about.
///
/// # `meta`
///
/// What holds for the whole file, and in practice which schema wrote it. Read before anything
/// else is, which is what lets [`Store::try_from_file`] refuse somebody else's database by
/// looking rather than by failing at a column three statements later.
///
/// # `memory`
///
/// One row per memory: what was remembered, what it is called from outside, and when it was
/// written. The text is the memory as [`Memory`] defines it, and the other two columns are the
/// facts a row has that a memory does not — which is exactly the split
/// [`memory`](crate::memory) argues.
///
/// `id` is a UUID and not the rowid, because it leaves the process: it is what a later `mem
/// delete` or `mem history` is handed back, and a rowid is a number this file assigns and a
/// re-import reassigns. `rowid` is nevertheless named and declared, rather than left implicit,
/// because the term index below refers to memories by it — and SQLite renumbers the rowids of
/// any table without an explicit integer primary key when the file is `VACUUM`ed. An index
/// pointing at renumbered rows is not a store that fails; it is a store that answers with the
/// wrong memories.
///
/// # `memory_fts_index`
///
/// The inverted index over [`terms`], one FTS5 row per memory under the same rowid.
/// It is what makes "the memories nearest a query" a question SQLite can answer by term at all,
/// and it brings `bm25()` with it — the ranking a keyword half of a search needs, which a table
/// of `(memory, term)` pairs would leave to be hand-written against statistics it would also
/// have to keep.
///
/// `content=''` because the terms are an index and not data. What the store holds is the text;
/// the terms are what one function makes of it, and are re-derivable from the text by running it
/// again — which is also the only honest rebuild, since the answer moves when the tokenizer's
/// dictionaries do. `contentless_delete=1` so that forgetting a memory is a `delete` and not a
/// row that stays findable after the memory it named is gone.
///
/// `tokenize='ascii'` because by the time FTS5 sees the terms there is no tokenizing left to do:
/// charabia has already cut the text by its language's own rules and normalized what it cut, and
/// the terms arrive here joined by spaces. All that is wanted is to split them apart again on
/// those spaces, and `ascii` is the tokenizer that leaves every byte above `0x7f` alone —
/// `unicode61` would be a second opinion about word boundaries, applied to text that is no
/// longer sentences.
const SCHEMA: &str = include_str!("../queries/init.sql");

/// One memory as a row — `queries/insert.sql`.
///
/// The id and the date are bound rather than defaulted in SQL, because both are decided per
/// batch and not per row: [`Store::insert`] gives every memory of one reading the same
/// `written_at`, which a column default of `current_timestamp` could not do — it would time each
/// row by when this loop reached it.
const INSERT: &str = include_str!("../queries/insert.sql");

/// The terms one memory is found by — `queries/index.sql`.
///
/// A second statement and not a trigger on `memory`, because nothing SQLite can see produces
/// these: the terms are what charabia made of the text, and a trigger would have to read them
/// off a column the row would then have to carry. There is no such column on purpose — see
/// [`SCHEMA`] on why the terms are an index and not data — so the two writes are two statements,
/// held together by the transaction around them instead.
const INDEX: &str = include_str!("../queries/index.sql");

/// The memories nearest a query, nearest first — `queries/search.sql`.
///
/// `bm25()` ascending is best first: FTS5 scores a better match as a more negative number, so
/// the ordering that reads as backwards is the one that puts the nearest memory on the first
/// line. The rowid after it is not a second opinion about nearness but a tiebreak — two memories
/// the ranking cannot separate would otherwise come back in whatever order the index walked
/// them, and a search that answers the same question two ways is one nobody can test or script
/// against. Oldest first among equals, since that is what a rowid is.
///
/// The join is how a contentless index is read at all: `memory_fts_index` holds no copy of the
/// text, so what a match produces is rowids, and the memories are on the other side of them.
const SEARCH: &str = include_str!("../queries/search.sql");

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

            // No `if not exists`, and no transaction around the statements: the file is one this
            // call made and nothing else has seen it, so there is nothing to collide with and
            // nothing a half-written schema could damage. What plays the part of a rollback is
            // the removal below, which undoes the file rather than the statements in it.
            conn.execute_batch(SCHEMA).map_err(sql_error)?;
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

    /// Write these memories, and index each under the terms it is found by.
    ///
    /// All of them or none. What produced a batch is one reading of one conversation — the
    /// extraction answers with the memories in it and nothing else, so there is no half of that
    /// answer worth keeping. A store left holding three of five would also be a store nobody
    /// can tell from one that was told three, since a memory carries nothing about the
    /// conversation it came out of.
    ///
    /// They share a `written_at` for the same reason: what is being timestamped is the reading,
    /// and a batch written in one call happened at one moment. Ordering two rows of it by a
    /// microsecond would be recording the order this loop ran in.
    ///
    /// The terms are taken here rather than accepted from the caller, which is what keeps a
    /// memory's index and its text from being able to disagree: [`terms`] is the only way into
    /// `memory_fts_index`, and [`Store::search`] cuts a query with the same function.
    pub fn insert(&self, memories: &[Memory]) -> io::Result<()> {
        // Before the lock, because this is the one expensive thing here: charabia is walking
        // text against a dictionary, and doing it while holding the file's write lock would make
        // every other writer wait on work that has nothing to do with the file.
        let indexed: Vec<(&str, String)> = memories
            .iter()
            .map(|memory| (memory.text.as_str(), terms(&memory.text).join(" ")))
            .collect();

        // A poisoned lock is a panic that happened while somebody held this connection, and it
        // is not a reason to refuse the file: what a poisoned lock protects is data whose
        // invariants a panic may have left half-applied, and the invariants here are SQLite's
        // own — a transaction in flight when the panic unwound was rolled back by its own
        // destructor, on the way out.
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;

        let written_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        {
            let mut row = tx.prepare(INSERT).map_err(sql_error)?;
            let mut terms = tx.prepare(INDEX).map_err(sql_error)?;

            for (text, indexed) in &indexed {
                row.execute((uuid::Uuid::new_v4().to_string(), text, &written_at))
                    .map_err(sql_error)?;
                // The rowid the line above assigned, which is what the index entry has to be
                // filed under: `memory_fts_index` holds no copy of the memory and is only ever read
                // back through it.
                terms
                    .execute((tx.last_insert_rowid(), indexed))
                    .map_err(sql_error)?;
            }
        }

        tx.commit().map_err(sql_error)
    }

    /// The memories nearest `query`, nearest first, and at most `limit` of them.
    ///
    /// # What "nearest" means here
    ///
    /// A memory is near a query when it holds the query's terms, and nearer the more of them it
    /// holds and the rarer they are — which is what `bm25` computes, and the reason the terms
    /// are asked for as `or` and not `and`. Every term is a hard requirement under `and`, so a
    /// question asked in a sentence — the way a person asks one — would answer nothing at all
    /// the moment one of its words was not in the store, and the memory that answered it
    /// perfectly except for `Tuesday` would be indistinguishable from a store that knew nothing.
    /// Under `or` that memory is simply the top line, and the ranking does the work the filter
    /// was doing badly.
    ///
    /// The cost of `or` is a long tail: a common word matches memories that have nothing to do
    /// with the question, ranked low but present. `limit` is what makes that a non-issue rather
    /// than a flaw — an answer is the first few lines, and the tail is never reached.
    ///
    /// # A query with no terms in it
    ///
    /// Punctuation, whitespace, nothing at all: no memories, and no error. There is no question
    /// here for the index to answer — a term is what [`terms`] made of the text, and it made
    /// none — and "no memories are near this" is a true answer to it. Refusing instead would be
    /// this function deciding that the caller made a mistake, on the evidence of a string it
    /// cannot read the intent of.
    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<String>> {
        // Cut by the same function that cut the memories, which is the whole contract between
        // the two halves: what charabia does to `서울에서` on the way in is what it does to
        // `서울` on the way out, and neither side is ever compared to a string as typed.
        let asked = terms(query);
        if asked.is_empty() {
            return Ok(Vec::new());
        }

        // Every term quoted, because FTS5 reads the pattern as an expression in a small language
        // of its own — `AND`, `NOT`, `*`, `:` and parentheses all mean something in it. A term is
        // text and not an expression, and a caller searching for `NOT` means the word. Quoting is
        // how FTS5 is told so, and doubling is its own escape for a quote inside a quoted string.
        let expression = asked
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");

        // SQLite counts in `i64`, and a `usize` that will not fit one is a caller asking for
        // more memories than a store could hold — which `i64::MAX` is also the answer to.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(SEARCH).map_err(sql_error)?;
        let found = stmt
            .query_map((&expression, limit), |row| row.get(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(sql_error)?;
        Ok(found)
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

    /// A store to write to, and the memories in it read back the way another command would.
    fn stored(path: &Path) -> Vec<(String, String, String)> {
        let conn = connect(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("the file opens");
        let mut stmt = conn
            .prepare("select id, text, written_at from memory order by rowid")
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    /// A store holding these memories, in this order.
    fn holding(dir: &Path, texts: &[&str]) -> Store {
        let store = Store::try_new(dir.join("notes.sqlite")).expect("a store can be made");
        let memories: Vec<Memory> = texts
            .iter()
            .map(|text| Memory {
                text: (*text).to_string(),
            })
            .collect();
        store.insert(&memories).expect("the memories are written");
        store
    }

    /// A memory goes in as a row and comes back as one, with the two facts the store gave it:
    /// a name of its own, and when it was written.
    #[test]
    fn a_memory_is_written_with_an_id_and_a_date() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");
        let store = Store::try_new(&path).expect("a store can be made");

        store
            .insert(&[
                Memory {
                    text: "User switched to oat milk".into(),
                },
                Memory {
                    text: "User lives in Seoul".into(),
                },
            ])
            .expect("the memories are written");

        let rows = stored(&path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "User switched to oat milk");
        assert_eq!(rows[1].1, "User lives in Seoul");

        // A name of the store's giving, and a different one per row: two memories out of one
        // conversation are two memories, and a later command names one of them.
        assert_ne!(rows[0].0, rows[1].0);
        assert!(
            rows.iter()
                .all(|(id, ..)| uuid::Uuid::parse_str(id).is_ok()),
            "an id that leaves the process is a UUID: {rows:?}"
        );

        // One reading of one conversation, so one moment — and a spelling anything else can
        // read back.
        assert_eq!(rows[0].2, rows[1].2);
        assert!(
            chrono::DateTime::parse_from_rfc3339(&rows[0].2).is_ok(),
            "written_at is RFC 3339: {:?}",
            rows[0].2
        );
    }

    /// The point of writing terms at all: a memory is found by a term of it, and the terms are
    /// the language's own — a Korean noun with its particle still on it in the text, asked for
    /// without one.
    #[test]
    fn a_memory_is_found_by_its_terms() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &[
                "User switched to oat milk",
                "User met a friend 서울에서 last Tuesday",
            ],
        );

        let found = |query: &str| store.search(query, 10).expect("the store answers");

        assert_eq!(found("oat milk"), ["User switched to oat milk"]);
        assert_eq!(
            found("서울"),
            ["User met a friend 서울에서 last Tuesday"],
            "the particle came off the noun on the way in, and there is none on the way out"
        );
        // A term of both is both, and a term of neither is nothing: what is being asked of the
        // index is which memories hold the terms, not which hold the string.
        assert_eq!(found("User").len(), 2, "lowercased on both sides");
        assert!(found("almond").is_empty());
    }

    /// What a ranking is for: the memory that holds more of the question comes first, and the
    /// one that shares a single common word with it is last rather than absent.
    #[test]
    fn the_nearest_memory_is_the_first_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &[
                "User drinks coffee every morning",
                "User switched to oat milk",
                "User switched to oat milk in coffee after an almond allergy",
            ],
        );

        let found = store
            .search("oat milk in coffee", 10)
            .expect("the store answers");
        assert_eq!(
            found,
            [
                "User switched to oat milk in coffee after an almond allergy",
                "User switched to oat milk",
                "User drinks coffee every morning",
            ],
            "every memory holds a term of the question, and they are ordered by how much of it"
        );
    }

    /// A question asked the way a person asks one. Under `and` every word would be a
    /// requirement and `Tuesday` alone would answer nothing; the memory that answers it is the
    /// first line instead.
    #[test]
    fn a_question_is_answered_by_what_is_nearest_and_not_only_by_what_holds_all_of_it() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &["User met a friend 서울에서 last Tuesday", "User drinks tea"],
        );

        let found = store
            .search("which friend did the user meet in 서울", 10)
            .expect("the store answers");
        assert_eq!(
            found.first().map(String::as_str),
            Some("User met a friend 서울에서 last Tuesday"),
            "not one memory holds every term of that question: {found:?}"
        );
    }

    /// The bound is part of the answer: what comes back is the nearest `limit`, and it is the
    /// nearest that survive the cut rather than whichever the index reached first.
    ///
    /// Which one that is, for a question of a single word, is decided by how much of each
    /// memory the word accounts for — `bm25` weighs a term against the length of what holds it,
    /// so the statement that is *about* coffee outranks the longer one that mentions it. Both
    /// hold the term exactly once, so nothing but the length separates them.
    #[test]
    fn a_search_answers_with_at_most_what_was_asked_for() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &[
                "User switched to oat milk in coffee after an almond allergy",
                "User drinks tea",
                "User drinks coffee",
            ],
        );

        let found = store.search("coffee", 1).expect("the store answers");
        assert_eq!(
            found,
            ["User drinks coffee"],
            "one memory, and the nearest of the two that hold the term"
        );
        assert!(
            store.search("drinks", 0).unwrap().is_empty(),
            "none is none"
        );
    }

    /// Two memories a ranking cannot separate come back in the order they were written, every
    /// time — a search that answered the same question two ways could not be scripted against.
    #[test]
    fn memories_the_ranking_cannot_separate_are_ordered_by_when_they_were_written() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(dir.path(), &["User drinks tea", "User drinks tea"]);

        for _ in 0..4 {
            assert_eq!(
                store.search("tea", 10).expect("the store answers"),
                ["User drinks tea", "User drinks tea"]
            );
        }
    }

    /// Nothing to ask for is no memories and no error: a term is what the tokenizer made of the
    /// text, it made none, and "nothing is near this" is a true answer to that.
    #[test]
    fn a_query_with_no_terms_in_it_finds_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(dir.path(), &["User drinks tea"]);

        for query in ["", "   ", "!!! ..."] {
            assert!(
                store
                    .search(query, 10)
                    .expect("no terms is not an error")
                    .is_empty(),
                "{query:?} asks for nothing"
            );
        }
    }

    /// FTS5 reads its pattern as an expression in a language of its own, and a query is not one:
    /// a caller who typed `NOT` meant the word, and one who typed a quote meant a quote.
    #[test]
    fn a_query_that_looks_like_an_expression_is_read_as_words() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &["User said NOT to recommend almond milk", "User drinks tea"],
        );

        assert_eq!(
            store.search("NOT almond", 10).expect("the store answers"),
            ["User said NOT to recommend almond milk"]
        );
        // Nothing here matches; what is being asserted is that asking does not fail.
        for query in [r#"a "quoted" phrase"#, "(unbalanced", "star*", "col:on"] {
            store
                .search(query, 10)
                .unwrap_or_else(|e| panic!("{query:?} is words and not syntax: {e}"));
        }
    }

    /// A store with nothing in it answers every question with nothing, which is the one thing
    /// `init` promises and `search` must not turn into an error.
    #[test]
    fn an_empty_store_answers_with_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(dir.path(), &[]);
        assert!(
            store
                .search("oat milk", 10)
                .expect("a store answers")
                .is_empty()
        );
    }

    /// Nothing to write is a write of nothing, and a store that is unchanged by it. A
    /// conversation carrying no memories is the extraction's own answer, and this is the
    /// caller's line for it — not a case every caller has to special-case first.
    #[test]
    fn writing_no_memories_writes_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");
        let store = Store::try_new(&path).expect("a store can be made");

        store.insert(&[]).expect("nothing is a batch too");
        assert!(stored(&path).is_empty());
    }

    /// The index and the rows are one write. A batch is one reading of one conversation, and a
    /// store holding half of it could not say which half.
    #[test]
    fn a_batch_that_cannot_be_finished_writes_none_of_itself() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");
        let store = Store::try_new(&path).expect("a store can be made");

        // A trigger, because there is no argument to `insert` that fails halfway — which is
        // the property being asserted, and so has to be broken from underneath. What it stands
        // in for is any write the file refuses on the second row: a disk that filled, a store
        // somebody made read-only between the two statements.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                "create trigger no_second before insert on memory
                 when (select count(*) from memory) >= 1
                 begin select raise(abort, 'no'); end",
            )
            .unwrap();
        }

        store
            .insert(&[
                Memory {
                    text: "User switched to oat milk".into(),
                },
                Memory {
                    text: "User lives in Seoul".into(),
                },
            ])
            .expect_err("the second row is refused");

        assert!(
            stored(&path).is_empty(),
            "the first row went back with the second"
        );
        let conn = connect(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let indexed: i64 = conn
            .query_row("select count(*) from memory_fts_index", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(indexed, 0, "and so did what was indexed under it");
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
