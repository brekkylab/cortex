//! What a backend has to be able to do, and what it hands back.

use futures_core::future::BoxFuture;

/// One hit: where it can be read, and enough of it to judge whether to.
///
/// Both halves are the backend's to produce, and that is the whole reason this type is so
/// small. A path is spelled by the rules of the tree the backend serves, and a record is
/// spelled by the format of the file that path names — neither is something a command over
/// several backends could work out, and a command that tried would be a second implementation
/// of every lane's layout.
#[derive(Debug)]
pub struct Hit {
    /// Tree-relative, from the root of the store this backend searches. The command prefixes
    /// the path the store was mounted at, so what a reader sees is a path in *their* tree.
    pub path: String,
    /// The hit as the file at `path` spells it — one line of it, for a line-oriented file.
    ///
    /// Bytes and not a string: this is copied out of a file's own format, and a command that
    /// decoded it would have to guess at an encoding nobody asked it about.
    ///
    /// It has to end with a newline. The command prints `<path>\t<record>` and reads the result
    /// back with `cut -f1`, so a record that does not terminate its own line runs the next hit's
    /// path onto it and every hit after the first is lost to the pipeline — silently, and with
    /// nothing in the exit code to say so. A backend whose format is not line-oriented has to
    /// choose a line for this. The command appends one if a backend does not, because it cannot
    /// check a contract it does not implement, but a record arriving without one is a defect in
    /// the backend.
    pub record: Vec<u8>,
}

/// A store whose service has an index worth asking.
///
/// Deliberately one method. Everything else a hit needs — resolving a name, spelling a path,
/// rendering a record — is knowledge about one lane, and belongs with that lane's backend.
///
/// Not every store implements this, and that is the point. Slack refuses `search.messages` to a
/// bot token; an object store has no text index whatsoever. A store with no index simply has no
/// backend here, and is not asked — which is not the same as being answered with an empty
/// result, and never says "nothing there" about a place nobody looked.
///
/// It is also not the same as being *named*. The fan-out knows the backends it was handed and
/// nothing else, so a mount registered with none is invisible to it: the report says what was
/// asked. Only `--in` can name an unsearchable place, because there the reader spelled it.
///
/// Which stores those are is a fact about somebody else's API on a date, and it moves. Discord
/// offered bots no message search until it shipped one in March 2026 — and this trait is why
/// that cost a backend rather than a redesign: all that changed was whether one type implements
/// it.
pub trait Searchable: Send + Sync {
    /// Hits for `query`, most relevant or most recent first, at most `count` of them.
    ///
    /// The query is the service's own syntax, passed through untouched. A common dialect over
    /// several indexes is the one thing this crate refuses to build: an index's operators are
    /// how its users already search, its semantics are its own (whose `from:` is a handle and
    /// whose is an address substring), and a translation layer would have to claim they agree.
    fn search<'a>(&'a self, query: &'a str, count: usize) -> BoxFuture<'a, SearchResult>;

    /// One line about what this backend is, for the report a fan-out prints.
    fn describe(&self) -> &str {
        ""
    }
}

/// What a backend answers with: the hits, or why it could not look.
///
/// A failure is a `String` rather than an error type, because the command does nothing with it
/// but say it. Every backend's failures are its service's own — an expired token, a scope never
/// granted, an index this credential may not read — and flattening them here keeps this crate
/// from growing an opinion about services it does not know.
pub type SearchResult = Result<Vec<Hit>, String>;
