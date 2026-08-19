//! The index both executables share: schema, writer, reader.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use tantivy::schema::{FAST, Field, STORED, STRING, Schema, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

/// The writer's heap. tantivy's floor is 15 MB; this is a few segments' worth of buffering
/// for an index a session builds, not a bulk load.
const WRITER_HEAP: usize = 50_000_000;

/// The fields, looked up once. `Schema::get_field` is a string lookup that cannot fail for a
/// schema we built ourselves, and doing it per document would be that lookup per document.
pub(crate) struct Fields {
    /// The workspace path, which is also the document's identity — `STRING`, so it is one
    /// term and `delete_term` can name exactly one document.
    ///
    /// `FAST` as well, so that walking what the index holds reads a column rather than a
    /// stored document. A stored document carries the body, and decompressing every one of
    /// them to learn a path is a pass over the whole corpus to answer a question about names.
    pub path: Field,
    /// The file name, indexed separately so a query matching a name outranks one matching a
    /// mention in someone else's body.
    pub title: Field,
    pub body: Field,

    /// When the file was last modified and how long it was, as of the ingest that wrote this.
    ///
    /// What makes a `sync` incremental: a file whose pair still matches is one whose bytes
    /// this index already has, so it is not read again. `FAST` for the reason `path` is —
    /// deciding what to re-read must not cost a read of everything.
    pub mtime: Field,
    pub len: Field,
}

/// One tantivy index, opened once and shared by every executable over it.
///
/// [`Arc`] rather than one per executable: `ingest` and `search` are two names for two
/// halves of the same index, and tantivy allows exactly one writer at a time — two stores
/// over one directory would be the second one failing on the lock file.
pub struct Store {
    index: Index,
    /// The writer, made when something first writes and kept afterwards.
    ///
    /// **Lazy, and that is the whole point.** tantivy's writer takes an exclusive lock file
    /// over the directory, so building one eagerly would make every read hold a write lock:
    /// a `search` would fail with `LockBusy` while anything was ingesting, and a store that a
    /// long-lived process had ever read would be one no other process could write to.
    ///
    /// Kept once made, because two overlapping ingests of one store must queue on this lock
    /// rather than race for tantivy's.
    ///
    /// # Kept for the process, and what that costs
    ///
    /// Nothing puts it back, so the first *write* takes this store's lock for as long as the
    /// process lives: a console that ingests once is a console no other process can write
    /// that store under, though any number may read it.
    ///
    /// Releasing it is `*held = None` after the commit in [`writer`](Self::writer)'s callers,
    /// and costs about 20ms a write call (a writer is an arena and a merge thread; measured
    /// against a `list`, which builds none). What it buys is smaller than it looks: tantivy
    /// allows one writer at a time whatever this does, so releasing turns a *permanent*
    /// exclusion into a *transient* one. A second writer still has to find the gap, which
    /// means retry and backoff on its side.
    ///
    /// So the question is not the 20ms, it is whether anything else writes the same store. A
    /// console that owns its indexes wants this as it is; a person running the standalone
    /// binary against a store a session is holding wants it released. There is no consumer
    /// yet to say which, and it is one line either way when there is.
    writer: Mutex<Option<IndexWriter>>,
    reader: IndexReader,
    fields: Fields,
}

/// A held writer. Exists so that making one lazily is still a `&mut IndexWriter` to a caller.
pub(crate) struct Writing<'a>(MutexGuard<'a, Option<IndexWriter>>);

impl std::ops::Deref for Writing<'_> {
    type Target = IndexWriter;
    fn deref(&self) -> &IndexWriter {
        self.0
            .as_ref()
            .expect("a writer, put there by `Store::writer`")
    }
}

impl std::ops::DerefMut for Writing<'_> {
    fn deref_mut(&mut self) -> &mut IndexWriter {
        self.0
            .as_mut()
            .expect("a writer, put there by `Store::writer`")
    }
}

impl Store {
    /// Open the index at `dir`, creating it if it is not there.
    ///
    /// A directory whose schema is not ours is **rebuilt**, not refused: the index is
    /// derived state and the corpus it was built from is still in the tree, so the cheap
    /// answer is to make it again rather than to make a caller delete it by hand.
    pub fn open(dir: &Path) -> io::Result<Arc<Self>> {
        let schema = build_schema();

        let index = match Index::open_in_dir(dir) {
            Ok(index) if has_our_fields(&index.schema()) => index,
            Ok(_) => {
                std::fs::remove_dir_all(dir)?;
                std::fs::create_dir_all(dir)?;
                Index::create_in_dir(dir, schema.clone()).map_err(other)?
            }
            Err(_) => {
                std::fs::create_dir_all(dir)?;
                Index::create_in_dir(dir, schema.clone()).map_err(other)?
            }
        };

        // No writer here: see the field. Opening a store is what a `search` does too.
        //
        // `OnCommitWithDelay` rather than `Manual`: a search that runs right after an ingest
        // should see it, and the searcher is what a reload swaps.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(other)?;

        let schema = index.schema();
        let fields = Fields {
            path: field(&schema, "path")?,
            title: field(&schema, "title")?,
            body: field(&schema, "body")?,
            mtime: field(&schema, "mtime")?,
            len: field(&schema, "len")?,
        };

        Ok(Arc::new(Store {
            index,
            writer: Mutex::new(None),
            reader,
            fields,
        }))
    }

    /// How many documents are in it, as of the last commit — what `list` reports.
    pub(crate) fn num_docs(&self) -> io::Result<u64> {
        Ok(self.searcher()?.num_docs())
    }

    pub(crate) fn index(&self) -> &Index {
        &self.index
    }

    pub(crate) fn fields(&self) -> &Fields {
        &self.fields
    }

    /// The searcher to answer one query from, current as of the last commit.
    ///
    /// Reloaded rather than trusted: `OnCommitWithDelay` is a delay, and an `ingest` that
    /// just returned is exactly the commit a caller expects the next `search` to see.
    pub(crate) fn searcher(&self) -> io::Result<tantivy::Searcher> {
        self.reader.reload().map_err(other)?;
        Ok(self.reader.searcher())
    }

    /// The writer, waiting for whoever holds it, and made on the first call that asks.
    ///
    /// Fails when another process holds the directory's lock, which is the honest answer:
    /// two writers on one index is what tantivy forbids, and this is where a caller hears it
    /// rather than at some later commit.
    ///
    /// A poisoned lock is taken anyway: the data behind it is tantivy's, a panic in one
    /// ingest says nothing about the index's state, and refusing every later ingest over it
    /// would turn one failed call into a dead name.
    pub(crate) fn writer(&self) -> io::Result<Writing<'_>> {
        let mut held = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_none() {
            *held = Some(self.index.writer(WRITER_HEAP).map_err(other)?);
        }
        Ok(Writing(held))
    }
}

fn build_schema() -> Schema {
    let mut schema = Schema::builder();
    schema.add_text_field("path", STRING | STORED | FAST);
    schema.add_text_field("title", TEXT | STORED);
    schema.add_text_field("body", TEXT | STORED);
    schema.add_u64_field("mtime", STORED | FAST);
    schema.add_u64_field("len", STORED | FAST);
    schema.build()
}

fn has_our_fields(existing: &Schema) -> bool {
    ["path", "title", "body", "mtime", "len"]
        .iter()
        .all(|name| existing.get_field(name).is_ok())
}

fn field(schema: &Schema, name: &str) -> io::Result<Field> {
    schema.get_field(name).map_err(other)
}

/// tantivy's error is not an `io::Error` and this crate answers in one, like both halves of
/// cortex. `other` keeps the original as the `source` rather than flattening it to a string.
pub(crate) fn other(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(e)
}
