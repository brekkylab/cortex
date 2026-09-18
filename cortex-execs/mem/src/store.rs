//! The memory store: one SQLite file holding what a store knows about itself, a row per
//! memory, and the full-text index over their text.
//!
//! # One file, and why that decides most of this
//!
//! A store is named the way a document is — `mem insert notes.db "..."` — because it lives
//! in a cortex tree, beside whatever else the session works on. Everything a memory needs is
//! therefore in that one file. Nothing here reaches a service, and copying the file copies the
//! memories.
//!
//! # No write-ahead log
//!
//! The file usually sits on a mounted cortex tree, which is a FUSE mount, and WAL mode does not
//! survive that assumption: the log's index is a shared-memory file that every connection
//! `mmap`s and expects to be coherent between processes. A FUSE filesystem is under no
//! obligation to give them that, and the failure is silent. So the store stays on the rollback
//! journal SQLite starts in, and concurrent writers wait on
//! [`BUSY_TIMEOUT`](cortex_exec_storebase::sqlite::BUSY_TIMEOUT).
//!
//! # The text is stored as written, and FTS5 cuts it
//!
//! `item_fts` holds the memory itself, not a pre-cut list of terms, and `porter unicode61`
//! turns it into terms inside SQLite. That is what makes `bm25()` rank on stems — a search for
//! `mounting` finds `mounts` — and it is the same shape `index` uses, so the two crates
//! index text one way rather than two.
//!
//! **What is stored is the original.** That is the rule, and it is what keeps the door open: a
//! store whose content is the text as the caller wrote it can be re-indexed under any tokenizer
//! later, from itself, with no re-ingest and nothing lost. A store that held lemmas instead
//! could not — the case, the punctuation and (for a language that needs segmenting) the word
//! boundaries would already be gone, and no rebuild could put them back.
//!
//! # Multilingual, and why it is not here yet
//!
//! `unicode61` splits on character class, which is the right rule for text that puts spaces
//! between its words and no rule at all for text that does not: `서울에서` is one token to it,
//! so a search for `서울` finds nothing. A language that needs segmenting needs a segmenter —
//! `charabia` is the one this workspace has used — and there is no way to reach one from
//! inside FTS5 here, because neither `rusqlite` nor `libsqlite3-sys` exposes the `fts5_api`
//! that registering a tokenizer goes through.
//!
//! The shape that would carry one is a third FTS column, `terms`, holding what a segmenter cut
//! and matched alongside `body`. `bm25(item_fts, 1.0, 1.0, 0.0)` keeps ranking on the text the
//! caller wrote, so the extra column only adds matches, and `body` goes on answering snippets
//! from the original. It costs a new
//! [`SCHEMA_VERSION`](cortex_exec_storebase::sqlite::SCHEMA_VERSION) and a rebuild from the text
//! this store already holds.

use std::{collections::HashSet, io, path::Path, sync::Mutex};

use cortex_exec_storebase::sqlite::{self, sql_error};
use rusqlite::OptionalExtension as _;

use crate::memory::Memory;

/// What this crate writes into `meta.kind`, and what it insists on when opening.
///
/// The tables are `storebase`'s and `index` keeps the same ones, so this is the only thing
/// that tells a store of one kind from a store of the other — see
/// [`cortex_exec_storebase::sqlite`].
const KIND: &str = "mem";

/// One memory, as the store holds it.
///
/// Kept apart from [`Memory`](crate::memory::Memory) by this crate's own rule — a memory is not a
/// row, and neither the id nor the date is the memory's to carry. So a write takes [`Memory`] and
/// a read answers with this.
///
/// The only reason `id` leaves this process is that [`Store::delete`] or [`Store::update`] will
/// later be called with it. That is also why it is a UUID rather than a rowid, and that judgement
/// is one this crate made from the start.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Record {
    /// The row's id — the UUID in `item.id`.
    pub id: String,
    /// The memory itself, as it was written.
    pub text: String,
    /// When this row arrived in the store, RFC 3339.
    pub written_at: String,
}

/// A memory as a row is [`sqlite::UPSERT`] with a null `path`, which is what makes it a memory
/// rather than a document: nulls do not collide, so every call inserts and the same sentence can
/// be remembered twice. `index` binds a path into the same statement and gets an upsert.
///
/// The id and the date are bound rather than defaulted in SQL, because both are decided per
/// batch and not per row: [`Store::insert`] gives every memory of one call the same
/// `written_at`, which a column default of `current_timestamp` could not do — it would time each
/// row by when this loop reached it.
/// The memories nearest a query, nearest first — `queries/search.sql`.
///
/// `bm25()` ascending is best first: FTS5 scores a better match as a more negative number, so
/// the ordering that reads as backwards is the one that puts the nearest memory on the first
/// line. The rowid after it is not a second opinion about nearness but a tiebreak — two memories
/// the ranking cannot separate would otherwise come back in whatever order the index walked
/// them, and a search that answers the same question two ways is one nobody can test or script
/// against. Oldest first among equals, since that is what a rowid is.
///
/// **There is a join, and there once was not.** The text is in `item_fts`, so the *sentence* a
/// search answers with needs nothing of `item`. What came to be needed is the row's id, and that
/// is in `item` — correcting or forgetting a memory where it was found means having a name to
/// call it by, and the text cannot be that name, since it allows duplicates. A join on rowid is
/// the cheapest thing SQLite does, and `limit` already bounds how many rows it walks.
const SEARCH: &str = include_str!("../queries/search.sql");

/// Everything the store holds, newest first — `queries/list.sql`.
///
/// Unlike [`SEARCH`], there is a join. A listing answers with rows, so it needs the id, and the id
/// is in `item`.
///
/// The second sort key is there because [`Store::insert`] gives one batch a single `written_at` —
/// five lines written together are not separated by that column. Descending rowid puts the later
/// of them on top, and that is what makes a listing answer the same way when it is asked twice.
const LIST: &str = include_str!("../queries/list.sql");

/// Delete one row by id, and answer with where to delete its text — `queries/delete.sql`.
///
/// It is `returning rowid` because `item_fts` is joined by rowid and FTS5 has no cascade. What
/// deletes the text is `storebase`'s [`sqlite::FTS_CLEAR`] — the very statement `index` uses when
/// it re-ingests, and a store of one kind deleting text differently from a store of the other is
/// the file format ceasing to be one format.
///
/// That it is `where id = ?` is what makes this statement `mem`'s. `index` would delete by `path`.
const DELETE: &str = include_str!("../queries/delete.sql");

/// The row an id points at — its rowid and its date — `queries/row_of.sql`.
///
/// An update begins here for two reasons. The text hangs off `item_fts` by rowid, so rewriting it
/// needs that number; and the record to answer with needs the original `written_at` — an update is
/// not an arrival, so that column does not change.
const ROW_OF: &str = include_str!("../queries/row_of.sql");

/// An open memory store.
///
/// Every method here is synchronous and blocks — this is SQLite. Nothing here hides that: the
/// program a store is opened by is doing this one thing and has nothing else to get on with.
///
/// [`Send`] + [`Sync`] through the mutex, which is what lets a caller hold one in an
/// [`Arc`](std::sync::Arc) and hand it to a blocking thread per call. A `Connection` is not
/// itself `Sync`, and serializing is not a cost worth avoiding: a rollback-journal file
/// serializes writers anyway.
#[derive(Debug)]
pub struct Store {
    /// The one connection, taken under a lock by whatever asks the file a question.
    conn: Mutex<rusqlite::Connection>,
}

impl Store {
    /// Make the store at `path`.
    ///
    /// [`AlreadyExists`](io::ErrorKind::AlreadyExists) when there is already a file there, and
    /// nothing half-made left behind when the schema will not install — see
    /// [`sqlite::create`], which is where both of those rules live.
    pub fn try_new(path: impl AsRef<Path>) -> io::Result<Store> {
        Ok(Store {
            conn: Mutex::new(sqlite::create(path.as_ref(), KIND)?),
        })
    }

    /// Open the store that is already at `path`.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) for no file, [`InvalidData`](io::ErrorKind::InvalidData)
    /// for somebody else's database or a schema this build does not speak — see
    /// [`sqlite::open`].
    pub fn try_from_file(path: impl AsRef<Path>) -> io::Result<Store> {
        Ok(Store {
            conn: Mutex::new(sqlite::open(path.as_ref(), KIND)?),
        })
    }

    /// Write these memories.
    ///
    /// All of them or none. What produced a batch is one `insert`, and a caller who named five
    /// memories meant five: there is no half of that worth keeping. A store left holding three
    /// of them would also be a store nobody can tell from one that was told three, since a
    /// memory carries nothing about the call it arrived on.
    ///
    /// They share a `written_at` for the same reason: what is being timestamped is the call,
    /// and a batch written in one call happened at one moment. Ordering two rows of it by a
    /// microsecond would be recording the order this loop ran in.
    ///
    /// What comes back is the rows that were written, in the order they were given. A caller has
    /// no other way to learn the ids this function made — the text allows duplicates, so they
    /// could not be found afterwards either — so they are answered with rather than dropped.
    /// This is what `--json` carries.
    pub fn insert(&self, memories: &[Memory]) -> io::Result<Vec<Record>> {
        // A poisoned lock is a panic that happened while somebody held this connection, and it
        // is not a reason to refuse the file: what a poisoned lock protects is data whose
        // invariants a panic may have left half-applied, and the invariants here are SQLite's
        // own — a transaction in flight when the panic unwound was rolled back by its own
        // destructor, on the way out.
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;

        let written_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut written = Vec::with_capacity(memories.len());
        {
            let mut row = tx.prepare(sqlite::UPSERT).map_err(sql_error)?;
            let mut text = tx.prepare(sqlite::FTS_WRITE).map_err(sql_error)?;

            for memory in memories {
                let id = uuid::Uuid::new_v4().to_string();
                let rowid: i64 = row
                    .query_row(
                        (
                            &id,
                            // The null that says "this is a memory": no path, so nothing to
                            // conflict with, so no update — see `sqlite::UPSERT`.
                            None::<&str>,
                            "", // title: a memory has no name
                            0,  // mtime
                            0,  // len
                            &written_at,
                        ),
                        |r| r.get(0),
                    )
                    .map_err(sql_error)?;
                // The empty `title`: a memory has no name, and an FTS5 column with no tokens
                // in it contributes nothing to `bm25` and matches nothing.
                text.execute((rowid, "", &memory.text)).map_err(sql_error)?;
                written.push(Record {
                    id,
                    text: memory.text.clone(),
                    written_at: written_at.clone(),
                });
            }
        }

        tx.commit().map_err(sql_error)?;
        Ok(written)
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
    /// # A query with no words in it
    ///
    /// Punctuation, whitespace, nothing at all: no memories, and no error. There is no question
    /// here for the index to answer, and "no memories are near this" is a true answer to it.
    /// Refusing instead would be this function deciding that the caller made a mistake, on the
    /// evidence of a string it cannot read the intent of.
    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<Record>> {
        let Some(expression) = as_expression(query) else {
            return Ok(Vec::new());
        };

        // SQLite counts in `i64`, and a `usize` that will not fit one is a caller asking for
        // more memories than a store could hold — which `i64::MAX` is also the answer to.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(SEARCH).map_err(sql_error)?;
        let found = stmt
            .query_map((&expression, limit), |row| {
                Ok(Record {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    written_at: row.get(2)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<Record>, _>>()
            .map_err(sql_error)?;
        Ok(found)
    }

    /// Everything the store holds, newest first, and at most `limit` of them.
    ///
    /// **No `limit` means all of them.** That is unlike [`Store::search`], which has a default
    /// bound, and being unlike it is right. A search's bound is an answer to the fact that
    /// nearness becomes a formality further down the list, but a listing has no ranking — what
    /// the store holds *is* the answer, and cutting it is for the caller to do when they want it
    /// cut.
    ///
    /// SQLite reads a negative `limit` as "no limit", so `None` goes down as `-1`.
    pub fn list(&self, limit: Option<usize>) -> io::Result<Vec<Record>> {
        let limit = match limit {
            Some(n) => i64::try_from(n).unwrap_or(i64::MAX),
            None => -1,
        };

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(LIST).map_err(sql_error)?;
        let found = stmt
            .query_map((limit,), |row| {
                Ok(Record {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    written_at: row.get(2)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<Record>, _>>()
            .map_err(sql_error)?;
        Ok(found)
    }

    /// Forget the memories with these ids. What comes back is the ids that were actually deleted.
    ///
    /// **An id that is not there is not a failure.** A delete's post-condition is "that memory is
    /// not in the store", and being called with an id that is not there means it is already true.
    /// The answer a caller reads is that the returned list is shorter than what they passed, and
    /// that is enough.
    ///
    /// One transaction — for [`Store::insert`]'s reason: a store where one call to forget five
    /// left three of them forgotten is a state nobody ever asked for.
    pub fn delete(&self, ids: &[String]) -> io::Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;

        let mut gone = Vec::new();
        {
            let mut row = tx.prepare(DELETE).map_err(sql_error)?;
            let mut text = tx.prepare(sqlite::FTS_CLEAR).map_err(sql_error)?;

            for id in ids {
                let rowid: Option<i64> = row
                    .query_row((id,), |r| r.get(0))
                    .optional()
                    .map_err(sql_error)?;
                if let Some(rowid) = rowid {
                    text.execute((rowid,)).map_err(sql_error)?;
                    gone.push(id.clone());
                }
            }
        }

        tx.commit().map_err(sql_error)?;
        Ok(gone)
    }

    /// Correct the memory with this id so that it says `text`. `None` when there is no such id.
    ///
    /// # `item` does not change
    ///
    /// A memory's text is in `item_fts.body` and not in `item` — that is what let
    /// [`Store::search`] answer without a join. So an update is nothing more than rewriting one
    /// FTS row, and since FTS5 has no upsert, it deletes and writes. Both statements are
    /// `storebase`'s — the very pair `index`'s idempotent re-ingest needed.
    ///
    /// `mem` leaves `path`, `title`, `mtime` and `len` at their defaults, so there is nothing on
    /// the `item` side to touch.
    ///
    /// # `written_at` does not change
    ///
    /// **An update is a correction, not an arrival.** That column says when this row *arrived* in
    /// the store, and pushing it forward on every correction would leave nowhere to answer "when
    /// did this store come to know this" from. Recording the correction time separately would take
    /// one more column, and that schema is shared with `index`, so
    /// [`SCHEMA_VERSION`](cortex_exec_storebase::sqlite::SCHEMA_VERSION) goes up — not a price
    /// worth paying for one convenience.
    ///
    /// A listing is newest first, so one side effect follows, and it is the one worth having: a
    /// corrected memory does not jump to the top.
    pub fn update(&self, id: &str, text: &str) -> io::Result<Option<Record>> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;

        let found: Option<(i64, String)> = tx
            .prepare(ROW_OF)
            .map_err(sql_error)?
            .query_row((id,), |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(sql_error)?;

        let Some((rowid, written_at)) = found else {
            return Ok(None);
        };

        tx.prepare(sqlite::FTS_CLEAR)
            .map_err(sql_error)?
            .execute((rowid,))
            .map_err(sql_error)?;
        tx.prepare(sqlite::FTS_WRITE)
            .map_err(sql_error)?
            .execute((rowid, "", text))
            .map_err(sql_error)?;

        tx.commit().map_err(sql_error)?;
        Ok(Some(Record {
            id: id.to_owned(),
            text: text.to_owned(),
            written_at,
        }))
    }
}

/// What a caller typed, as something FTS5 will accept.
///
/// **Every word is quoted, and that is the whole of the escaping.** FTS5 reads a pattern as an
/// expression in a small language of its own — `AND`, `OR`, `NOT`, `NEAR`, `*`, `:`, `-` and
/// parentheses all mean something in it — and a query written by an agent that was never told
/// that language would otherwise be a syntax error rather than a search. Quoting says "these are
/// words"; doubling is FTS5's own escape for a quote inside a quoted string.
///
/// Once each, because `bm25` adds up a contribution per term of the pattern: a word repeated in
/// the question would otherwise count twice and outweigh what the question is about.
///
/// `index` has the same function, and they stay apart on purpose — two consumers agreeing
/// today is not a rule, and the first time one of them needs a different query language the
/// shared one would have to be argued with.
fn as_expression(query: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|word| seen.insert(*word))
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a store says about itself, read off the file rather than off the handle that made
    /// it: what `init` leaves behind is read by the next command, in another process.
    fn meta(path: &Path) -> std::collections::BTreeMap<String, String> {
        let conn = sqlite::connect(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("the file opens");
        let mut stmt = conn.prepare("select key, value from meta").unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    /// What this crate has to get right about `meta` is the one key that is its own: which kind
    /// of store it just made. The version beside it is `storebase`'s, and tested there.
    #[test]
    fn a_new_store_says_which_kind_it_is() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");

        Store::try_new(&path).expect("a store can be made");
        assert_eq!(meta(&path).get("kind").map(String::as_str), Some(KIND));
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
        let conn = sqlite::connect(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("the file opens");
        let mut stmt = conn
            .prepare(
                "select item.id, item_fts.body, item.written_at
                 from item join item_fts on item_fts.rowid = item.rowid
                 order by item.rowid",
            )
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
        // `insert` are two memories, and a later command names one of them.
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

    /// The point of writing terms at all: a memory is found by a term of it, and by a term
    /// rather than by the string it was written as.
    #[test]
    fn a_memory_is_found_by_its_terms() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &[
                "User switched to oat milk",
                "User met a friend in Seoul last Tuesday",
            ],
        );

        let found = |query: &str| store.search(query, 10).expect("the store answers");

        assert_eq!(
            found("oat milk")
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            ["User switched to oat milk"]
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
            found.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
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
            found.first().map(|r| r.text.as_str()),
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
            found.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
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
                store
                    .search("tea", 10)
                    .expect("the store answers")
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>(),
                ["User drinks tea", "User drinks tea"]
            );
        }
    }

    /// A word repeated in the question does not count twice. `bm25` adds up a contribution per
    /// term of the pattern, so a long passage asked as one question would otherwise be decided
    /// by whichever common word it happened to repeat, and not by what it is about.
    ///
    /// The corpus is chosen so that the two orders differ if the repetition is passed through:
    /// doubling `oat` lifts the memory that holds only `oat` above the one that holds only
    /// `milk`.
    #[test]
    fn a_word_repeated_in_a_question_is_asked_once() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &["oat milk", "milk chocolate bar", "oat porridge bowl"],
        );

        let once = store.search("oat milk", 10).expect("the store answers");
        assert_eq!(
            once.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["oat milk", "milk chocolate bar", "oat porridge bowl"]
        );
        assert_eq!(
            store.search("oat oat milk", 10).expect("the store answers"),
            once,
            "saying `oat` twice is asking the same question"
        );
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
            store
                .search("NOT almond", 10)
                .expect("the store answers")
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
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

    /// What a search answers with is a row now — the id comes out with it, and that id can be
    /// deleted by.
    #[test]
    fn a_search_answers_with_rows_that_can_be_addressed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        let written = store
            .insert(&[Memory {
                text: "oat milk in the fridge".into(),
            }])
            .expect("written");

        let found = store.search("oat", 10).expect("a search");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, written[0].id);
        assert_eq!(found[0].text, "oat milk in the fridge");
        assert_eq!(found[0].written_at, written[0].written_at);
    }

    /// What was written comes back as it was, in the order it was given. The ids differ and the
    /// dates match — `insert`'s rule that one batch is one moment.
    #[test]
    fn insert_answers_with_what_it_wrote() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");

        let written = store
            .insert(&[
                Memory {
                    text: "오트밀크로 바꿨다".into(),
                },
                Memory {
                    text: "차를 마신다".into(),
                },
            ])
            .expect("the batch is written");

        assert_eq!(written.len(), 2);
        assert_eq!(written[0].text, "오트밀크로 바꿨다");
        assert_eq!(written[1].text, "차를 마신다");
        assert_ne!(written[0].id, written[1].id, "each row has its own id");
        assert_eq!(
            written[0].written_at, written[1].written_at,
            "one call is one moment"
        );
    }

    /// Nothing to write is a write of nothing, and a store that is unchanged by it. `insert`
    /// with no text after the store is a line a script can produce without checking whether it
    /// had anything to say — not a case every caller has to special-case first.
    #[test]
    fn writing_no_memories_writes_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");
        let store = Store::try_new(&path).expect("a store can be made");

        store.insert(&[]).expect("nothing is a batch too");
        assert!(stored(&path).is_empty());
    }

    /// The text and the row are one write. A batch is one `insert`, and a store holding half
    /// of it could not say which half.
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
                "create trigger no_second before insert on item
                 when (select count(*) from item) >= 1
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
        let conn = sqlite::connect(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let indexed: i64 = conn
            .query_row("select count(*) from item_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(indexed, 0, "and so did what was indexed under it");
    }

    /// What indexing the text rather than a list of terms buys: `porter` stems, so a question
    /// asked in one form finds a memory written in another. A pre-cut term list cannot do this —
    /// there is no stemmer to run once the words have already been chosen.
    #[test]
    fn a_memory_is_found_by_a_word_in_another_form() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = holding(
            dir.path(),
            &["User switched to oat milk because of mounting allergies"],
        );

        for asked in ["mounting", "mount", "allergies", "allergy", "switched", "switch"] {
            assert_eq!(
                store.search(asked, 10).expect("the store answers").len(),
                1,
                "{asked} did not reach the memory"
            );
        }
        assert!(
            store.search("zygote", 10).expect("the store answers").is_empty(),
            "and a word that is not there still finds nothing"
        );
    }

    /// The text comes back as it was written — the store holds the original, not what a
    /// tokenizer made of it. That is what lets the index be rebuilt under another tokenizer
    /// later without a re-ingest.
    #[test]
    fn a_memory_comes_back_exactly_as_it_was_written() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let written = "User met a friend on Tuesday — CAFE, not cafe!";
        let store = holding(dir.path(), &[written]);

        assert_eq!(
            store
                .search("friend", 10)
                .expect("the store answers")
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            [written],
            "case and punctuation survive"
        );
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

    /// Newest first. A batch shares one `written_at`, so within it the order is descending rowid
    /// — the later line is the higher one.
    #[test]
    fn list_answers_newest_first() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");

        store
            .insert(&[Memory {
                text: "첫째".into(),
            }])
            .expect("written");
        store
            .insert(&[
                Memory {
                    text: "둘째".into(),
                },
                Memory {
                    text: "셋째".into(),
                },
            ])
            .expect("written");

        let all = store.list(None).expect("a listing");

        // The second batch first, and within it the later row first.
        assert_eq!(
            all.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["셋째", "둘째", "첫째"]
        );
    }

    /// No bound means all of them. That is where it differs from `search`, and `list` has no
    /// ranking, so all of them *is* the answer.
    #[test]
    fn list_without_a_limit_answers_with_everything() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        let many: Vec<Memory> = (0..50)
            .map(|n| Memory {
                text: format!("메모리 {n}"),
            })
            .collect();
        store.insert(&many).expect("written");

        assert_eq!(store.list(None).expect("a listing").len(), 50);
        assert_eq!(store.list(Some(3)).expect("a listing").len(), 3);
    }

    /// An empty store is an empty listing, and not a failure.
    #[test]
    fn list_of_an_empty_store_is_empty() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        assert!(store.list(None).expect("a listing").is_empty());
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

    /// A deleted memory is off the listing, and **out of the search too** — the evidence that
    /// `item_fts` went with it. If only the `item` row had been deleted, this test catches it.
    #[test]
    fn a_deleted_memory_is_gone_from_the_index_too() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        let written = store
            .insert(&[
                Memory {
                    text: "oat milk in the fridge".into(),
                },
                Memory {
                    text: "tea in the cupboard".into(),
                },
            ])
            .expect("written");

        let gone = store.delete(&[written[0].id.clone()]).expect("deleted");

        assert_eq!(gone, vec![written[0].id.clone()]);
        assert_eq!(store.list(None).expect("a listing").len(), 1);
        assert!(
            store.search("oat", 10).expect("a search").is_empty(),
            "the text left the index with the row"
        );
    }

    /// An id that is not there is not a failure. A delete's post-condition is "it is not there",
    /// and it is already satisfied.
    #[test]
    fn deleting_what_is_not_there_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        store
            .insert(&[Memory {
                text: "차를 마신다".into(),
            }])
            .expect("written");

        let gone = store.delete(&["없는-id".to_string()]).expect("no failure");

        assert!(
            gone.is_empty(),
            "nothing was deleted, and that is an answer"
        );
        assert_eq!(store.list(None).expect("a listing").len(), 1);
    }

    /// A batch is all of it or none — `insert`'s rule. Giving one id twice is not a failure here
    /// either, only a second lookup that finds nothing, so what this looks at is that the commit
    /// happened whole.
    #[test]
    fn a_batch_delete_is_one_transaction() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        let written = store
            .insert(&[
                Memory {
                    text: "하나".into(),
                },
                Memory { text: "둘".into() },
                Memory { text: "셋".into() },
            ])
            .expect("written");

        let gone = store
            .delete(&[written[0].id.clone(), written[2].id.clone()])
            .expect("deleted");

        assert_eq!(gone.len(), 2);
        assert_eq!(
            store
                .list(None)
                .expect("a listing")
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            ["둘"]
        );
    }

    /// What `a_batch_delete_is_one_transaction` cannot catch: that test deletes two ids and only
    /// looks at whether both went — lift `delete`'s transaction out entirely and each statement
    /// simply autocommits, and it passes just the same. This one uses the same trick
    /// `a_batch_that_cannot_be_finished_writes_none_of_itself` uses on `insert`: plant a trigger
    /// that forces a failure partway through the batch, and look at whether the store comes back
    /// exactly as it was before it.
    ///
    /// The trigger fires once `item` is down to two rows — in a three-line store it lets the
    /// first delete through (3 → 2) and blocks the moment the second is attempted inside the same
    /// transaction (the count before that removal being already 2). If `delete` committed per
    /// statement with no transaction, the first delete would already be on disk, and this test
    /// would fail on seeing a listing of two rather than three.
    #[test]
    fn a_batch_delete_that_cannot_be_finished_undoes_none_of_itself() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("notes.sqlite");
        let store = Store::try_new(&path).expect("a store can be made");
        let written = store
            .insert(&[
                Memory {
                    text: "하나".into(),
                },
                Memory { text: "둘".into() },
                Memory { text: "셋".into() },
            ])
            .expect("written");

        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                "create trigger no_second_delete before delete on item
                 when (select count(*) from item) <= 2
                 begin select raise(abort, 'no'); end",
            )
            .unwrap();
        }

        store
            .delete(&[written[0].id.clone(), written[2].id.clone()])
            .expect_err("the second row of the batch is refused");

        let remaining = store.list(None).expect("a listing");
        assert_eq!(
            remaining.len(),
            3,
            "the first row's delete went back with the second: {remaining:?}"
        );
        assert_eq!(
            remaining
                .iter()
                .map(|r| r.text.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            ["하나", "둘", "셋"].into_iter().collect(),
        );

        let conn = sqlite::connect(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let indexed: i64 = conn
            .query_row("select count(*) from item_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, 3, "what was indexed came back with what named it");
    }

    /// After a correction the old terms no longer find it and the new ones do — the evidence that
    /// `item_fts` was actually rewritten. If only `item` had been touched, this test catches it.
    #[test]
    fn an_updated_memory_is_reindexed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        let written = store
            .insert(&[Memory {
                text: "oat milk in the fridge".into(),
            }])
            .expect("written");

        store
            .update(&written[0].id, "almond milk in the fridge")
            .expect("updated")
            .expect("the row is there");

        assert!(
            store.search("oat", 10).expect("a search").is_empty(),
            "the old terms left"
        );
        assert_eq!(
            store.search("almond", 10).expect("a search")[0].text,
            "almond milk in the fridge"
        );
    }

    /// An update is a correction and not an arrival — the id and the date both stay.
    #[test]
    fn an_update_keeps_the_id_and_the_date() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");
        let written = store
            .insert(&[Memory {
                text: "첫 문장".into(),
            }])
            .expect("written");

        let after = store
            .update(&written[0].id, "고친 문장")
            .expect("updated")
            .expect("there");

        assert_eq!(after.id, written[0].id);
        assert_eq!(after.written_at, written[0].written_at);
        assert_eq!(after.text, "고친 문장");
    }

    /// An update against an id that is not there has to say it did nothing, unlike `delete`. The
    /// post-condition is "that memory says this", and it was not satisfied.
    #[test]
    fn updating_what_is_not_there_answers_with_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");

        assert_eq!(
            store.update("없는-id", "무엇이든").expect("no failure"),
            None
        );
    }

    /// Multi-line Markdown makes the round trip byte for byte.
    ///
    /// This pins that the crate does nothing to the text — no normalizing, no sanitizing, no
    /// tidying of lines. An editor that reads and writes this store breaks when this breaks.
    #[test]
    fn markdown_survives_a_round_trip_byte_for_byte() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::try_new(dir.path().join("notes.mem")).expect("a new store");

        let body = "## 마감\n\n분기 마감은 **11월 15일**.\n\n| 채널 | NCNR |\n|---|---|\n| Luxshare | 80% |\n\n```sh\nmem list notes.mem\n```";

        let written = store
            .insert(&[Memory { text: body.into() }])
            .expect("written");
        assert_eq!(written[0].text, body);

        let back = store.list(None).expect("a listing");
        assert_eq!(back[0].text, body, "what came back is what went in");
    }
}
