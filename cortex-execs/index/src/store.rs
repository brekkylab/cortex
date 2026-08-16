//! The index both executables share: schema, writer, reader.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use tantivy::schema::{Field, STORED, STRING, Schema, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

/// The writer's heap. tantivy's floor is 15 MB; this is a few segments' worth of buffering
/// for an index a session builds, not a bulk load.
const WRITER_HEAP: usize = 50_000_000;

/// The fields, looked up once. `Schema::get_field` is a string lookup that cannot fail for a
/// schema we built ourselves, and doing it per document would be that lookup per document.
pub(crate) struct Fields {
    /// The workspace path, which is also the document's identity — `STRING`, so it is one
    /// term and `delete_term` can name exactly one document.
    pub path: Field,
    /// The file name, indexed separately so a query matching a name outranks one matching a
    /// mention in someone else's body.
    pub title: Field,
    pub body: Field,
}

/// One tantivy index, opened once and shared by every executable over it.
///
/// [`Arc`] rather than one per executable: `ingest` and `search` are two names for two
/// halves of the same index, and tantivy allows exactly one writer at a time — two stores
/// over one directory would be the second one failing on the lock file.
pub struct Store {
    index: Index,
    /// One writer for the process's lifetime, behind a lock. Building one per call is what
    /// makes two overlapping ingests collide, and it re-pays the heap allocation each time.
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
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

        let writer = index.writer(WRITER_HEAP).map_err(other)?;
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
        };

        Ok(Arc::new(Store {
            index,
            writer: Mutex::new(writer),
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

    /// The writer, waiting for whoever holds it.
    ///
    /// A poisoned lock is taken anyway: the data behind it is tantivy's, a panic in one
    /// ingest says nothing about the index's state, and refusing every later ingest over it
    /// would turn one failed call into a dead name.
    pub(crate) fn writer(&self) -> MutexGuard<'_, IndexWriter> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn build_schema() -> Schema {
    let mut schema = Schema::builder();
    schema.add_text_field("path", STRING | STORED);
    schema.add_text_field("title", TEXT | STORED);
    schema.add_text_field("body", TEXT | STORED);
    schema.build()
}

fn has_our_fields(existing: &Schema) -> bool {
    ["path", "title", "body"]
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
