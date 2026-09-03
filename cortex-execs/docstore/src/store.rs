//! The store: one SQLite file holding what a store knows about itself, a row per indexed
//! file, and the full-text index over their bodies.
//!
//! # One file, and why that decides most of this
//!
//! A store is named the way a document is — `docstore ingest notes.db /docs` — because it
//! lives in a cortex tree, beside whatever else the session works on. Everything the index
//! needs is therefore in that one file, and copying the file copies the index.
//!
//! That is the whole of what `docstore` changes about `cortex-exec-index`, whose store is a
//! named directory under a host root. It could not have been a file in the tree: tantivy
//! `mmap`s its segments and holds a lock file, and under FUSE-T a mount is an NFS one, where
//! both are exactly the operations that behave differently. SQLite on the rollback journal
//! asks for neither, which is why `memstore` already keeps its stores in the tree.
//!
//! # No write-ahead log
//!
//! The file usually sits on a mounted cortex tree, which is a FUSE mount, and WAL mode does
//! not survive that assumption: the log's index is a shared-memory file that every connection
//! `mmap`s and expects to be coherent between processes. A FUSE filesystem is under no
//! obligation to give them that, and the failure is silent. So the store stays on the
//! rollback journal SQLite starts in, and concurrent writers wait on [`BUSY_TIMEOUT`].
//!
//! # The body lives in the FTS table
//!
//! `item` holds what a file *is* — its path, its name, the stamp it was read under —
//! and `item_fts` holds the text. Not an external-content table: `snippet()` needs the
//! text on the FTS5 side to cut around a match, and an external-content table would keep it
//! in `item` and make every update a two-sided ritual. So the two are joined by a rowid
//! this crate assigns to both, which is the arrangement [`Store::apply`] maintains.

use std::{io, path::Path, sync::Mutex};

use cortex_exec_storebase::sqlite::{self, sql_error};
use rusqlite::Connection;

use crate::ingest::Stamp;

/// What this crate writes into `meta.kind`, and what it insists on when opening.
///
/// The tables are `storebase`'s and `memstore` keeps the same ones, so this is the only thing
/// that tells a store of one kind from a store of the other — see
/// [`cortex_exec_storebase::sqlite`].
const KIND: &str = "docstore";

/// A document written or replaced is [`sqlite::UPSERT`] with a path bound, which is what makes
/// it a document rather than a memory: `path` is `unique`, so a second `ingest` of it lands on
/// the conflict clause and updates in place, keeping the rowid `item_fts` is filed under.
///
/// `item.id` is left null — that column is `memstore`'s. What makes a document is its path.
/// Every row that has a path — which in a shared table is what "every document" means, since a
/// row with none is `memstore`'s. The whole set and not a filtered slice, because the filter is
/// [`under`](crate::ingest::under) and it is component-wise: `notes-other/a.md` is not under
/// `notes`, where a SQL `like 'notes%'` would say it was. Doing the prefix test in SQL would
/// mean either escaping `%` and `_` out of every path or keeping a second spelling of the
/// rule, and this reads three small columns with no body among them.
const HELD: &str = include_str!("../queries/held.sql");

/// One document removed — `queries/forget.sql`. The file it was built from is untouched.
const FORGET: &str = include_str!("../queries/forget.sql");

/// The documents nearest a query, nearest first — `queries/search.sql`.
///
/// `bm25()` ascending is best first: FTS5 scores a better match as a more negative number. It
/// is negated on the way out so that the column a caller reads keeps the sense the one
/// `cortex-exec-index` printed had — larger is better — because the same number meaning the
/// opposite thing in the same position is the kind of change nothing warns about.
///
/// The rowid after it is a tiebreak and not a second opinion about nearness: two documents the
/// ranking cannot separate would otherwise come back in whatever order the index walked them,
/// and a search that answers the same question two ways is one nobody can script against.
const SEARCH: &str = include_str!("../queries/search.sql");

/// A document on its way into the store.
pub(crate) struct Writing<'a> {
    pub workspace: &'a Path,
    pub title: String,
    pub body: String,
    pub stamp: Stamp,
}

/// A document the store holds.
pub(crate) struct Held {
    pub rowid: i64,
    pub path: String,
    pub stamp: Stamp,
}

/// One answer: where it is, how well it matched, and enough of it to tell.
pub(crate) struct Hit {
    pub path: String,
    pub title: String,
    pub score: f64,
    pub snippet: String,
}

/// An open store.
///
/// Every method here is synchronous and blocks — this is SQLite. Keeping that off the task
/// the call arrived on is the caller's job, and [`exec`](crate::exec) does it: every
/// call into this type goes through `spawn_blocking`.
#[derive(Debug)]
pub(crate) struct Store {
    conn: Mutex<Connection>,
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
    /// for a file that is not one of these — see [`sqlite::open`].
    pub fn try_from_file(path: impl AsRef<Path>) -> io::Result<Store> {
        Ok(Store {
            conn: Mutex::new(sqlite::open(path.as_ref(), KIND)?),
        })
    }

    /// Whether `path` is a `docstore` store — what `list` sifts a directory with, and what
    /// `drop` asks before it deletes anything.
    ///
    /// It has to ask about the *kind* and not only the schema, because a `memstore` file has the
    /// same tables: without that, `list` would count somebody's memories as documents and `drop`
    /// would delete them.
    pub fn looks_like_one(path: &Path) -> bool {
        sqlite::is_store(path, KIND)
    }

    /// How many documents it holds — what `list` reports.
    pub fn count(&self) -> io::Result<u64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // `count(*)` is an `i64` in SQLite and is never negative, so the widening is safe.
        conn.query_row("select count(*) from item where path is not null", [], |row| {
            row.get::<_, i64>(0)
        })
            .map(|n| n as u64)
            .map_err(sql_error)
    }

    /// Write these documents and forget those, in one transaction.
    ///
    /// **One call and not two, because `sync` does both** and a store between them is a tree
    /// that half-existed: a reader would see the new bytes of one file beside a document for a
    /// file that is gone. `cortex-exec-index` has the same property by committing its writer
    /// once for both halves, and it is worth keeping.
    ///
    /// All of them or none, for the same reason at a smaller scale. What produced a batch is
    /// one walk of one set of arguments, and a store left holding half of it is one nobody can
    /// tell from a store that was given half — the report would say what was written, and the
    /// next `sync` would decide what to re-read from a state that never was.
    ///
    /// The written documents share a `written_at`: what is being timestamped is the call, and a
    /// batch written in one transaction happened at one moment.
    pub fn apply(&self, writing: &[Writing<'_>], forgetting: &[i64]) -> io::Result<()> {
        let written_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;
        {
            let mut upsert = tx.prepare(sqlite::UPSERT).map_err(sql_error)?;
            let mut clear = tx.prepare(sqlite::FTS_CLEAR).map_err(sql_error)?;
            let mut write = tx.prepare(sqlite::FTS_WRITE).map_err(sql_error)?;
            let mut forget = tx.prepare(FORGET).map_err(sql_error)?;

            for doc in writing {
                let path = doc.workspace.to_string_lossy();
                let rowid: i64 = upsert
                    .query_row(
                        (
                            // The null that says "this is a document": identity is the path.
                            None::<&str>,
                            path.as_ref(),
                            &doc.title,
                            // SQLite counts in `i64` and a `Stamp` is `u64`, so the two
                            // wide halves are swapped here and swapped back on the way out.
                            // The round trip is exact — it is the same bits read two ways —
                            // which is all `unchanged_from` needs, since it only ever compares
                            // a stamp to another stamp.
                            doc.stamp.mtime as i64,
                            doc.stamp.len as i64,
                            &written_at,
                        ),
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;

                // Cleared before it is written, because FTS5 has no upsert and a second insert
                // under one rowid is a second copy of the document rather than a replacement.
                clear.execute((rowid,)).map_err(sql_error)?;
                write
                    .execute((rowid, &doc.title, &doc.body))
                    .map_err(sql_error)?;
            }

            for rowid in forgetting {
                forget.execute((rowid,)).map_err(sql_error)?;
                // By hand, because `item_fts` is a virtual table and nothing cascades into
                // one. `chunk` does cascade — `foreign_keys` is on, see `connect`.
                clear.execute((rowid,)).map_err(sql_error)?;
            }
        }
        tx.commit().map_err(sql_error)
    }

    /// Every document the store holds, with the stamp it was written under.
    ///
    /// The whole table — see [`HELD`] for why the prefix test is not asked of SQL.
    pub fn held(&self) -> io::Result<Vec<Held>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(HELD).map_err(sql_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Held {
                    rowid: row.get(0)?,
                    path: row.get(1)?,
                    stamp: Stamp {
                        mtime: row.get::<_, i64>(2)? as u64,
                        len: row.get::<_, i64>(3)? as u64,
                    },
                })
            })
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// The documents nearest `query`, nearest first, and at most `limit` of them.
    ///
    /// `query` is already an FTS5 expression — [`as_expression`] is what turns what a caller
    /// typed into one.
    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<Hit>> {
        // SQLite counts in `i64`, and a `usize` that will not fit one is a caller asking for
        // more documents than a store could hold — which `i64::MAX` is also the answer to.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(SEARCH).map_err(sql_error)?;
        let rows = stmt
            .query_map((query, limit), |row| {
                Ok(Hit {
                    path: row.get(0)?,
                    title: row.get(1)?,
                    score: row.get(2)?,
                    snippet: row.get(3)?,
                })
            })
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }
}

/// What a caller typed, as something FTS5 will accept.
///
/// **Every word is quoted, and that is the whole of the escaping.** FTS5 reads a pattern as an
/// expression in a small language of its own — `AND`, `OR`, `NOT`, `NEAR`, `*`, `:`, `-` and
/// parentheses all mean something in it — and a query written by an agent that was never told
/// that language would otherwise be a syntax error rather than a search. Quoting says "these
/// are words"; doubling is FTS5's own escape for a quote inside a quoted string.
///
/// Joined with `OR`, which is what `cortex-exec-index` did before it: every term is a hard
/// requirement under `AND`, so a question asked in a sentence — the way a person asks one —
/// would answer nothing at all the moment one of its words was absent. Under `OR` the document
/// holding most of them is simply the top line, and `bm25` does the work the filter was doing
/// badly. The cost is a long tail, which the limit makes a non-issue.
///
/// Once each, because `bm25` adds up a contribution per term of the pattern: a word repeated in
/// the question would otherwise count twice and outweigh what the question is about.
///
/// An empty answer for a query with no words in it — punctuation, whitespace, nothing at all.
/// There is no question there for the index to answer, and saying so is truer than refusing.
pub(crate) fn as_expression(query: &str) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|word| seen.insert(*word))
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}
