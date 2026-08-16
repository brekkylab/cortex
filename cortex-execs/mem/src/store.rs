//! The memory store: one SQLite file, holding the memories, their vectors and their history.
//!
//! # One file, and why that decides most of this
//!
//! A store is named the way a document is — `mem insert notes.sqlite "..."` — because it lives
//! in a cortex tree, beside whatever else the session works on. Everything a memory needs is
//! therefore in that one file: the rows, the vector index over them, and the record of what
//! happened to each. Nothing here reaches a service, and copying the file copies the memory.
//!
//! Nearest-neighbour search is [`sqlite-vec`]'s `vec0` virtual table, registered as a
//! statically linked auto-extension rather than loaded at run time — there is no `.dylib` to
//! ship beside the binary and none to find on a host that has never heard of it.
//!
//! # No write-ahead log
//!
//! The file usually sits on a mounted cortex tree, which is a FUSE mount, and WAL mode does
//! not survive that assumption: the log's index is a shared-memory file that every connection
//! `mmap`s and expects to be coherent between processes. A FUSE filesystem is under no
//! obligation to give them that, and the failure is silent — two writers each see their own
//! index. So the store stays on the rollback journal SQLite starts in, which needs nothing but
//! the ability to create a sibling file, and concurrent writers wait on
//! [`BUSY_TIMEOUT`](self::BUSY_TIMEOUT) rather than racing.
//!
//! [`sqlite-vec`]: https://github.com/asg017/sqlite-vec

use std::{
    io,
    path::Path,
    sync::{Mutex, MutexGuard, Once, PoisonError},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, ToSql, params_from_iter};

/// How long a connection waits for another writer before giving up.
///
/// A rollback-journal store serializes writers, and `mem` is invoked once per command: the
/// contention that happens is two commands overlapping by a moment, not a queue. Waiting is
/// therefore almost always the right answer, and a bound is still needed because the other
/// side may be a process that died holding the lock.
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What this crate's schema is; refuse a file written by a newer one.
const SCHEMA_VERSION: &str = "1";

/// How many vectors `vec0` keeps in one blob.
///
/// A chunk is allocated whole, so it is also the smallest a store can be: at `vec0`'s own
/// default of 1024, a file holding three memories is a megabyte at 256 dimensions and six at
/// the width a hosted model produces. These stores sit in a working tree beside the notes they
/// are about, where a file that size for three sentences is a surprise worth avoiding.
///
/// The scan reads the same bytes either way — chunks are searched one after another, whatever
/// their size — so what smaller chunks cost is more rows to step through, which is a fraction
/// of the work of comparing what is in them.
const CHUNK: usize = 64;

/// Which memories a command is about.
///
/// mem0's three: a `user`, an `agent` acting for them, and a `run` of that agent. They are
/// independent labels rather than a hierarchy — a memory can carry any combination — and the
/// empty string means "not labelled", not "labelled empty", because a `vec0` metadata column
/// cannot hold NULL.
///
/// As a *filter*, an empty field constrains nothing: `mem search` with no `--user` searches
/// every user's memories rather than only the unlabelled ones. So a scope narrows a search
/// exactly as far as it was spelled out, and one that says nothing is the whole store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    pub user: String,
    pub agent: String,
    pub run: String,
}

impl Scope {
    /// The columns and values a non-empty field constrains, in a fixed order.
    fn filters(&self) -> Vec<(&'static str, &str)> {
        [
            ("user_id", self.user.as_str()),
            ("agent_id", self.agent.as_str()),
            ("run_id", self.run.as_str()),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect()
    }
}

/// What happened to a memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Add,
    Update,
    Delete,
}

impl Event {
    /// Its spelling in the file and on stdout — one word, uppercase, as mem0 records it.
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Add => "ADD",
            Event::Update => "UPDATE",
            Event::Delete => "DELETE",
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A memory, as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The id the outside world names this by — a UUID, minted once and never reused.
    ///
    /// Not the rowid the vector table joins on: that number is an index into this file and
    /// means nothing to a command that wrote a memory yesterday, or to a file that was
    /// rebuilt.
    pub id: String,
    pub memory: String,
    pub scope: Scope,
    /// Whatever the caller attached, as a JSON object. `{}` when nothing was.
    pub metadata: String,
    pub created_at: String,
    pub updated_at: Option<String>,
}

/// A memory a search found, and how near it was.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub record: Record,
    /// Cosine similarity: one for the same direction, zero for unrelated, negative for
    /// opposed. The table answers in distance; this is the more familiar way round, and the
    /// one a threshold reads naturally.
    pub score: f32,
}

/// One entry in a memory's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub event: Event,
    pub before: Option<String>,
    pub after: Option<String>,
    pub at: String,
}

/// A change to make, with the vector that goes with it.
///
/// Owned rather than borrowed because this crosses onto a blocking thread, and carrying the
/// vector rather than the text because embedding is the caller's to do: it waits on a service,
/// and the store must not.
#[derive(Clone, Debug)]
pub enum Write {
    Add {
        memory: String,
        vector: Vec<f32>,
    },
    Update {
        id: String,
        memory: String,
        vector: Vec<f32>,
    },
    Delete {
        id: String,
    },
}

/// A [`Write`] the store carried out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub id: String,
    pub event: Event,
    pub memory: String,
}

/// An open memory store.
///
/// Every method here is synchronous and blocks: this is SQLite, and the vector search is a C
/// extension running in the calling thread. The async layer above ([`Memory`]) is what keeps
/// that off a task's thread, so nothing in this module needs to know a runtime exists.
///
/// [`Send`] + [`Sync`] through the mutex, which is what lets that layer hold one in an
/// [`Arc`](std::sync::Arc) and hand it to a blocking thread per call. A `Connection` is not
/// itself `Sync`, and serializing is not a cost worth avoiding: a rollback-journal file
/// serializes writers anyway.
///
/// [`Memory`]: crate::Memory
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
    dims: usize,
}

impl Store {
    /// Open the store at `path`, creating the file and its schema if it is not there.
    ///
    /// `embedder` and `dims` are the identity of the vectors the caller will produce, and they
    /// are written into a new file and checked against an existing one — see [`Embedder`],
    /// which argues why a mismatch has to be refused rather than tolerated.
    ///
    /// [`Embedder`]: crate::Embedder
    pub fn create(path: &Path, embedder: &str, dims: usize) -> io::Result<Store> {
        let conn = connect(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        let store = Store {
            conn: Mutex::new(conn),
            dims,
        };
        store.install(embedder, dims)?;
        Ok(store)
    }

    /// Open an existing store at `path` for writing.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) when there is no such file — the difference from
    /// [`create`](Self::create), and the right one for a change to a memory that is supposed
    /// to be there already: forgetting something out of a store that does not exist should say
    /// so, not make an empty store to not find it in.
    pub fn open(path: &Path, embedder: &str, dims: usize) -> io::Result<Store> {
        let store = Store {
            conn: Mutex::new(connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?),
            dims,
        };
        store.check(embedder, dims)?;
        Ok(store)
    }

    /// Open an existing store at `path`, read-only.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) when there is no such file. A command that only
    /// reads should not bring a store into being: `mem search` against a misspelled name is a
    /// question about a store that does not exist, and answering "no memories" while quietly
    /// creating one is two wrong answers.
    ///
    /// Read-only at the connection, not merely by convention, so a mount that will not be
    /// written to is never asked to be.
    pub fn read(path: &Path, embedder: &str, dims: usize) -> io::Result<Store> {
        let store = Store {
            conn: Mutex::new(connect(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?),
            dims,
        };
        store.check(embedder, dims)?;
        Ok(store)
    }

    /// The width every vector in this store has.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// The `k` memories nearest `vector` within `scope`, nearest first.
    ///
    /// The scope is applied *inside* the nearest-neighbour scan rather than to its results, so
    /// `k` is a count of matching memories: filtering afterwards would answer with fewer than
    /// asked for, or none, whenever another user's memories happened to be nearer.
    pub fn nearest(&self, vector: &[f32], scope: &Scope, k: usize) -> io::Result<Vec<Hit>> {
        if vector.len() != self.dims {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a {}-wide vector cannot be searched in a {}-wide store",
                    vector.len(),
                    self.dims
                ),
            ));
        }

        let filters = scope.filters();
        let mut sql = String::from(
            "select m.uuid, m.memory, m.user_id, m.agent_id, m.run_id, m.metadata, \
                    m.created_at, m.updated_at, v.distance \
               from memory_vectors v join memories m on m.id = v.memory_id \
              where v.embedding match ?1 and k = ?2",
        );
        let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(bytes(vector)), Box::new(k as i64)];
        for (column, value) in &filters {
            params.push(Box::new(value.to_string()));
            sql.push_str(&format!(" and v.{column} = ?{}", params.len()));
        }
        sql.push_str(" order by v.distance");

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(sql_error)?;
        let hits = stmt
            .query_map(params_from_iter(params.iter()), |row| {
                Ok(Hit {
                    record: record(row)?,
                    // Cosine *distance*, which `vec0` defines as one minus the similarity.
                    score: 1.0 - row.get::<_, f64>(8)? as f32,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(hits)
    }

    /// Every memory in `scope`, oldest first, up to `limit`.
    pub fn all(&self, scope: &Scope, limit: usize) -> io::Result<Vec<Record>> {
        let mut sql = String::from(
            "select uuid, memory, user_id, agent_id, run_id, metadata, created_at, updated_at \
               from memories where 1 = 1",
        );
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();
        for (column, value) in scope.filters() {
            params.push(Box::new(value.to_string()));
            sql.push_str(&format!(" and {column} = ?{}", params.len()));
        }
        params.push(Box::new(limit as i64));
        sql.push_str(&format!(" order by id limit ?{}", params.len()));

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql).map_err(sql_error)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), record)
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(rows)
    }

    /// One memory by id, or `None` if the store has no such memory.
    pub fn get(&self, id: &str) -> io::Result<Option<Record>> {
        let conn = self.conn();
        conn.query_row(
            "select uuid, memory, user_id, agent_id, run_id, metadata, created_at, updated_at \
               from memories where uuid = ?1",
            [id],
            record,
        )
        .optional()
        .map_err(sql_error)
    }

    /// Everything that has happened to the memory `id`, oldest first.
    ///
    /// A memory that was deleted still has a history — that is most of the point of keeping
    /// one — so this is not the same question as [`get`](Self::get), and an empty answer means
    /// no memory by that id was ever written.
    pub fn history(&self, id: &str) -> io::Result<Vec<Entry>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "select memory_uuid, event, old_memory, new_memory, created_at \
                   from history where memory_uuid = ?1 order by id",
            )
            .map_err(sql_error)?;
        let entries = stmt
            .query_map([id], |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    event: match row.get::<_, String>(1)?.as_str() {
                        "ADD" => Event::Add,
                        "UPDATE" => Event::Update,
                        _ => Event::Delete,
                    },
                    before: row.get(2)?,
                    after: row.get(3)?,
                    at: row.get(4)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        Ok(entries)
    }

    /// Carry out `writes`, all of them or none.
    ///
    /// One transaction, because the three tables are one fact between them: a memory whose
    /// vector did not make it is invisible to every search, and one whose history entry did not
    /// is a change nobody can account for. A failure part-way leaves the file as it was.
    ///
    /// `scope` and `metadata` label whatever is added. They do not touch an updated memory,
    /// which keeps the labels it was written with — an update restates a fact, and who it was
    /// about did not change.
    ///
    /// The answer lists what was actually done, which can be shorter than `writes`: an
    /// [`Add`](Write::Add) of text this scope already holds word for word is dropped, and so is
    /// a write naming an id the store does not have.
    pub fn apply(
        &self,
        writes: &[Write],
        scope: &Scope,
        metadata: &str,
        now: &str,
    ) -> io::Result<Vec<Applied>> {
        for write in writes {
            let vector = match write {
                Write::Add { vector, .. } | Write::Update { vector, .. } => vector,
                Write::Delete { .. } => continue,
            };
            if vector.len() != self.dims {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "a {}-wide vector cannot be stored in a {}-wide store",
                        vector.len(),
                        self.dims
                    ),
                ));
            }
        }

        let mut conn = self.conn();
        let tx = conn.transaction().map_err(sql_error)?;
        let mut applied = Vec::new();

        for write in writes {
            match write {
                Write::Add { memory, vector } => {
                    // The last stand against storing the same sentence twice. Whoever decided
                    // to add it saw only the nearest few memories, so a duplicate that sat
                    // outside that neighbourhood was never theirs to catch.
                    let held: Option<i64> = tx
                        .query_row(
                            "select id from memories \
                              where memory = ?1 and user_id = ?2 and agent_id = ?3 and run_id = ?4",
                            (memory, &scope.user, &scope.agent, &scope.run),
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(sql_error)?;
                    if held.is_some() {
                        continue;
                    }

                    let id = uuid::Uuid::new_v4().to_string();
                    tx.execute(
                        "insert into memories \
                           (uuid, memory, user_id, agent_id, run_id, metadata, created_at) \
                         values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        (
                            &id,
                            memory,
                            &scope.user,
                            &scope.agent,
                            &scope.run,
                            metadata,
                            now,
                        ),
                    )
                    .map_err(sql_error)?;
                    let rowid = tx.last_insert_rowid();
                    tx.execute(
                        "insert into memory_vectors (memory_id, user_id, agent_id, run_id, embedding) \
                         values (?1, ?2, ?3, ?4, ?5)",
                        (rowid, &scope.user, &scope.agent, &scope.run, bytes(vector)),
                    )
                    .map_err(sql_error)?;
                    log(&tx, &id, Event::Add, None, Some(memory), now)?;
                    applied.push(Applied {
                        id,
                        event: Event::Add,
                        memory: memory.clone(),
                    });
                }

                Write::Update { id, memory, vector } => {
                    let held: Option<(i64, String)> = tx
                        .query_row(
                            "select id, memory from memories where uuid = ?1",
                            [id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(sql_error)?;
                    let Some((rowid, before)) = held else {
                        continue;
                    };

                    tx.execute(
                        "update memories set memory = ?2, updated_at = ?3 where id = ?1",
                        (rowid, memory, now),
                    )
                    .map_err(sql_error)?;
                    tx.execute(
                        "update memory_vectors set embedding = ?2 where memory_id = ?1",
                        (rowid, bytes(vector)),
                    )
                    .map_err(sql_error)?;
                    log(&tx, id, Event::Update, Some(&before), Some(memory), now)?;
                    applied.push(Applied {
                        id: id.clone(),
                        event: Event::Update,
                        memory: memory.clone(),
                    });
                }

                Write::Delete { id } => {
                    let held: Option<(i64, String)> = tx
                        .query_row(
                            "select id, memory from memories where uuid = ?1",
                            [id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(sql_error)?;
                    let Some((rowid, before)) = held else {
                        continue;
                    };

                    tx.execute("delete from memories where id = ?1", [rowid])
                        .map_err(sql_error)?;
                    tx.execute("delete from memory_vectors where memory_id = ?1", [rowid])
                        .map_err(sql_error)?;
                    log(&tx, id, Event::Delete, Some(&before), None, now)?;
                    applied.push(Applied {
                        id: id.clone(),
                        event: Event::Delete,
                        memory: before,
                    });
                }
            }
        }

        tx.commit().map_err(sql_error)?;
        Ok(applied)
    }

    /// The connection, whoever last held it.
    ///
    /// A poisoned lock is not a reason to refuse here: the guarded value is a SQLite
    /// connection, and a panic while one was held cannot leave it half-written — an open
    /// transaction is rolled back when its guard drops, which is the same unwind.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Create the schema if this file has none, then check what is there.
    fn install(&self, embedder: &str, dims: usize) -> io::Result<()> {
        let conn = self.conn();
        conn.execute_batch(&format!(
            "begin;
             create table if not exists mem_meta (
                key   text primary key,
                value text not null
             );
             create table if not exists memories (
                id         integer primary key,
                uuid       text not null unique,
                memory     text not null,
                user_id    text not null default '',
                agent_id   text not null default '',
                run_id     text not null default '',
                metadata   text not null default '{{}}',
                created_at text not null,
                updated_at text
             );
             create index if not exists memories_scope
                on memories (user_id, agent_id, run_id);
             create table if not exists history (
                id          integer primary key,
                memory_uuid text not null,
                event       text not null,
                old_memory  text,
                new_memory  text,
                created_at  text not null
             );
             create index if not exists history_memory on history (memory_uuid);
             create virtual table if not exists memory_vectors using vec0 (
                memory_id integer primary key,
                user_id   text,
                agent_id  text,
                run_id    text,
                embedding float[{dims}] distance_metric=cosine, chunk_size={CHUNK}
             );
             commit;"
        ))
        .map_err(sql_error)?;

        conn.execute(
            "insert or ignore into mem_meta (key, value) values \
               ('schema_version', ?1), ('embedder', ?2), ('dims', ?3)",
            (SCHEMA_VERSION, embedder, dims.to_string()),
        )
        .map_err(sql_error)?;
        drop(conn);

        self.check(embedder, dims)
    }

    /// Refuse a file this build cannot answer questions about.
    fn check(&self, embedder: &str, dims: usize) -> io::Result<()> {
        let conn = self.conn();

        // Asked of the catalogue rather than by reading the table and interpreting the
        // failure: "no such table" arrives as the same error code as a dozen unrelated
        // things, and a corrupt store must not be reported as somebody else's file.
        let ours: bool = conn
            .query_row(
                "select count(*) from sqlite_master where type = 'table' and name = 'mem_meta'",
                [],
                |row| Ok(row.get::<_, i64>(0)? > 0),
            )
            .map_err(sql_error)?;
        if !ours {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a mem store: it has no mem_meta table",
            ));
        }

        let read = |key: &str| -> io::Result<Option<String>> {
            conn.query_row("select value from mem_meta where key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(sql_error)
        };

        let found = (read("schema_version")?, read("embedder")?, read("dims")?);
        let (Some(version), Some(name), Some(width)) = found else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a mem store: its mem_meta table is incomplete",
            ));
        };

        if version != SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("this store is schema version {version}; this mem speaks {SCHEMA_VERSION}"),
            ));
        }
        // The two halves of the same refusal, kept apart because the answers differ: a width
        // this build cannot even bind against, and a width it can bind against and must not.
        if width != dims.to_string() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("this store holds {width}-wide vectors; this embedder makes {dims}-wide"),
            ));
        }
        if name != embedder {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "this store was written by the {name} embedder; \
                     {embedder} would place the same text somewhere else"
                ),
            ));
        }
        Ok(())
    }
}

/// A row of `memories`, in the column order every query here selects.
fn record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: row.get(0)?,
        memory: row.get(1)?,
        scope: Scope {
            user: row.get(2)?,
            agent: row.get(3)?,
            run: row.get(4)?,
        },
        metadata: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// Record one change against `id`.
fn log(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    event: Event,
    before: Option<&str>,
    after: Option<&str>,
    now: &str,
) -> io::Result<()> {
    tx.execute(
        "insert into history (memory_uuid, event, old_memory, new_memory, created_at) \
         values (?1, ?2, ?3, ?4, ?5)",
        (id, event.as_str(), before, after, now),
    )
    .map_err(sql_error)?;
    Ok(())
}

/// A vector as `vec0` takes it: little-endian `f32`s, one after another.
fn bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Open one connection, with the extension registered and the pragmas this store needs.
fn connect(path: &Path, flags: OpenFlags) -> io::Result<Connection> {
    register_vec0();
    let conn = Connection::open_with_flags(path, flags).map_err(sql_error)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_error)?;
    Ok(conn)
}

/// Make `vec0` available to every connection this process opens, once.
///
/// SQLite's auto-extension list is process-wide, which is why this is a [`Once`] and not
/// something a `Store` does per connection: registering the same entry point repeatedly is
/// allowed but pointless, and the list is consulted at every `open` anyway.
fn register_vec0() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is an extension entry point of exactly the shape SQLite
        // calls, and the crate declares it as a plain `fn` — the cast is what its own
        // documented usage does. It is registered before any connection here is opened, and
        // an auto-extension is only ever invoked from `sqlite3_open`.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// A SQLite failure as the rest of the crate answers in.
///
/// Two codes are worth a kind of their own, because they are the two a caller can act on: a
/// file that is not there, and a tree that will not be written to. Everything else is a
/// database error whose own message says more than a kind would.
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

    const DIMS: usize = 4;

    fn store(dir: &tempfile::TempDir) -> Store {
        Store::create(&dir.path().join("m.sqlite"), "test", DIMS).expect("a store can be created")
    }

    fn scope(user: &str) -> Scope {
        Scope {
            user: user.into(),
            ..Scope::default()
        }
    }

    fn add(memory: &str, vector: [f32; DIMS]) -> Write {
        Write::Add {
            memory: memory.into(),
            vector: vector.to_vec(),
        }
    }

    fn applied(store: &Store, writes: &[Write], scope: &Scope) -> Vec<Applied> {
        store
            .apply(writes, scope, "{}", "2026-08-14T00:00:00.000Z")
            .expect("the writes are well formed")
    }

    #[test]
    fn a_memory_is_found_by_a_vector_pointing_its_way() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        applied(
            &store,
            &[
                add("drinks tea", [1.0, 0.0, 0.0, 0.0]),
                add("deploys on friday", [0.0, 1.0, 0.0, 0.0]),
            ],
            &Scope::default(),
        );

        let hits = store
            .nearest(&[0.9, 0.1, 0.0, 0.0], &Scope::default(), 5)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].record.memory, "drinks tea");
        assert!(
            hits[0].score > hits[1].score,
            "the nearer memory scored {} against {}",
            hits[0].score,
            hits[1].score
        );
    }

    /// The scope is part of the scan, not a sieve over its results: asking for one memory in
    /// `bo`'s scope answers with `bo`'s nearest, however many of `ana`'s are nearer.
    #[test]
    fn a_scoped_search_fills_k_with_that_scopes_memories() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        applied(
            &store,
            &[
                add("ana drinks tea", [1.0, 0.0, 0.0, 0.0]),
                add("ana drinks more tea", [0.99, 0.01, 0.0, 0.0]),
            ],
            &scope("ana"),
        );
        applied(
            &store,
            &[add("bo drinks tea", [0.9, 0.1, 0.0, 0.0])],
            &scope("bo"),
        );

        let hits = store
            .nearest(&[1.0, 0.0, 0.0, 0.0], &scope("bo"), 1)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.memory, "bo drinks tea");
    }

    /// An unscoped search is every scope, which is what makes the empty string a filter that
    /// says nothing rather than a label to match.
    #[test]
    fn an_unscoped_search_reaches_every_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        applied(
            &store,
            &[add("ana drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &scope("ana"),
        );
        applied(
            &store,
            &[add("nobody's memory", [0.0, 1.0, 0.0, 0.0])],
            &Scope::default(),
        );

        let hits = store
            .nearest(&[1.0, 1.0, 0.0, 0.0], &Scope::default(), 10)
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn an_update_keeps_the_id_and_moves_the_vector() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let id = applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &Scope::default(),
        )[0]
        .id
        .clone();

        let done = applied(
            &store,
            &[Write::Update {
                id: id.clone(),
                memory: "drinks coffee".into(),
                vector: vec![0.0, 1.0, 0.0, 0.0],
            }],
            &Scope::default(),
        );
        assert_eq!(done[0].event, Event::Update);
        assert_eq!(done[0].id, id);

        let hits = store
            .nearest(&[0.0, 1.0, 0.0, 0.0], &Scope::default(), 1)
            .unwrap();
        assert_eq!(hits[0].record.id, id);
        assert_eq!(hits[0].record.memory, "drinks coffee");
        assert!(hits[0].record.updated_at.is_some());
    }

    /// A deleted memory is gone from both tables — a vector left behind would be found by a
    /// search that could no longer say what it belonged to.
    #[test]
    fn a_delete_takes_the_vector_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let id = applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &Scope::default(),
        )[0]
        .id
        .clone();

        applied(
            &store,
            &[Write::Delete { id: id.clone() }],
            &Scope::default(),
        );
        assert!(store.get(&id).unwrap().is_none());
        assert!(
            store
                .nearest(&[1.0, 0.0, 0.0, 0.0], &Scope::default(), 5)
                .unwrap()
                .is_empty()
        );
    }

    /// Every change is accounted for, including the one that removed the memory the history
    /// is about.
    #[test]
    fn history_survives_the_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let id = applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &Scope::default(),
        )[0]
        .id
        .clone();
        applied(
            &store,
            &[Write::Update {
                id: id.clone(),
                memory: "drinks coffee".into(),
                vector: vec![0.0, 1.0, 0.0, 0.0],
            }],
            &Scope::default(),
        );
        applied(
            &store,
            &[Write::Delete { id: id.clone() }],
            &Scope::default(),
        );

        let entries = store.history(&id).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.event).collect::<Vec<_>>(),
            [Event::Add, Event::Update, Event::Delete]
        );
        assert_eq!(entries[1].before.as_deref(), Some("drinks tea"));
        assert_eq!(entries[1].after.as_deref(), Some("drinks coffee"));
        assert_eq!(entries[2].after, None);
    }

    #[test]
    fn the_same_text_in_the_same_scope_is_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &Scope::default(),
        );
        let again = applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &Scope::default(),
        );

        assert!(again.is_empty());
        assert_eq!(store.all(&Scope::default(), 10).unwrap().len(), 1);
    }

    /// The same sentence about two different people is two facts, not one.
    #[test]
    fn the_same_text_in_another_scope_is_another_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &scope("ana"),
        );
        let bo = applied(
            &store,
            &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
            &scope("bo"),
        );

        assert_eq!(bo.len(), 1);
        assert_eq!(store.all(&scope("bo"), 10).unwrap().len(), 1);
    }

    /// A write naming an id the store has never held changes nothing and is not an error: the
    /// id came from whatever decided the change, and deciding about a memory that has since
    /// gone is a race, not a bug.
    #[test]
    fn a_write_against_an_unknown_id_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let done = applied(
            &store,
            &[
                Write::Delete {
                    id: "no-such-id".into(),
                },
                Write::Update {
                    id: "no-such-id".into(),
                    memory: "x".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                },
            ],
            &Scope::default(),
        );
        assert!(done.is_empty());
    }

    /// All of them or none: the vector, the row and the history entry are one fact between
    /// them, so a batch that fails part-way leaves the file as it was.
    #[test]
    fn a_failed_write_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let err = store
            .apply(
                &[
                    add("drinks tea", [1.0, 0.0, 0.0, 0.0]),
                    Write::Add {
                        memory: "too wide".into(),
                        vector: vec![1.0; DIMS + 1],
                    },
                ],
                &Scope::default(),
                "{}",
                "2026-08-14T00:00:00.000Z",
            )
            .expect_err("a vector of the wrong width cannot be stored");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(store.all(&Scope::default(), 10).unwrap().is_empty());
    }

    #[test]
    fn a_store_reopens_with_what_was_written_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite");
        {
            let store = Store::create(&path, "test", DIMS).unwrap();
            applied(
                &store,
                &[add("drinks tea", [1.0, 0.0, 0.0, 0.0])],
                &Scope::default(),
            );
        }

        let reopened = Store::read(&path, "test", DIMS).unwrap();
        let hits = reopened
            .nearest(&[1.0, 0.0, 0.0, 0.0], &Scope::default(), 5)
            .unwrap();
        assert_eq!(hits[0].record.memory, "drinks tea");
    }

    /// Vectors from another model are not comparable to these, and a search over them would
    /// answer confidently and wrongly. Both halves of the identity are checked.
    #[test]
    fn a_store_refuses_an_embedder_that_did_not_write_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite");
        Store::create(&path, "test", DIMS).unwrap();

        for (embedder, dims) in [("other", DIMS), ("test", DIMS * 2)] {
            let err = Store::read(&path, embedder, dims)
                .expect_err("a store cannot be read with the wrong embedder");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    /// Reading is not a reason to bring a store into being — a misspelled name is a question
    /// about a store that does not exist.
    #[test]
    fn opening_a_store_that_is_not_there_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Store::read(&dir.path().join("absent.sqlite"), "test", DIMS)
            .expect_err("there is no such file");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!dir.path().join("absent.sqlite").exists());
    }

    #[test]
    fn a_file_that_is_not_a_mem_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("create table notes (body text)")
            .unwrap();

        let err = Store::read(&path, "test", DIMS).expect_err("this is somebody else's database");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
