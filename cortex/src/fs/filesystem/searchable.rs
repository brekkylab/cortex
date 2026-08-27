//! What a store's own index answers with, when it has one.
//!
//! Here rather than in the command that fans out over these, because [`FileSystem`] names it:
//! a store is asked for its index the same way it is asked for anything else, and a trait the
//! store contract mentions cannot live in a crate that depends on this one.
//!
//! # Why a store is asked rather than told
//!
//! A tree is a tree because a reader can `grep` it, and that holds for *tokens* and not for
//! *requests*. Walking a synthesized tree costs a call per directory it invents, so `grep -r`
//! over a mount is thousands of them and a rate limit answers before the search does. An index
//! the service already keeps answers the same question in one call, and it reaches content the
//! walk would have had to open every file to find.
//!
//! What comes back is a [`Hit`], which is a *path into this tree* and enough of the file to
//! judge whether to open it. That is the whole division: recall comes from the index, and
//! precision comes from the tree, because a handful of named files is something `grep` can say
//! things about that no index can express.

use crate::BoxFuture;

/// One hit: where it can be read, and enough of it to judge whether to.
///
/// Both halves are the store's to produce, and that is the whole reason this type is so small.
/// A path is spelled by the rules of the tree the store serves, and a record is spelled by the
/// format of the file that path names — neither is something a command over several stores
/// could work out, and a command that tried would be a second implementation of every store's
/// layout.
#[derive(Debug)]
pub struct Hit {
    /// Tree-relative, from the root of the store that answered. Whoever fans out prefixes the
    /// path the store is mounted at, so what a reader sees is a path in *their* tree.
    pub path: String,
    /// The hit as the file at `path` spells it — one line of it, for a line-oriented file.
    ///
    /// Bytes and not a string: this is copied out of a file's own format, and a consumer that
    /// decoded it would have to guess at an encoding nobody asked it about.
    ///
    /// It has to end with a newline. A fan-out prints `<path>\t<record>` and readers pull the
    /// first field back out with `cut -f1`, so a record that does not terminate its own line
    /// runs the next hit's path onto it and every hit after the first is lost to the pipeline —
    /// silently, and with nothing in an exit code to say so. A store whose format is not
    /// line-oriented has to choose a line for this.
    pub record: Vec<u8>,
}

/// What an index answers with: the hits, or why it could not look.
///
/// A failure is a `String` rather than an error type, because a fan-out does nothing with it
/// but say it. Every store's failures are its service's own — an expired token, a scope never
/// granted, an index this credential may not read — and flattening them here keeps the
/// consumer from growing an opinion about services it does not know.
pub type SearchResult = Result<Vec<Hit>, String>;

/// A store's own index, for the stores that have one.
///
/// Deliberately one method. Everything else a hit needs — resolving a name, spelling a path,
/// rendering a record — is knowledge about one store's layout, and belongs with that store.
///
/// Reached through [`FileSystem::index`](super::FileSystem::index), which is `None` by default:
/// not every service has an index a credential may ask. Slack refuses `search.messages` to a
/// bot token; an object store has no text index whatsoever. A trait method some stores could
/// only ever fail is the same mistake as a directory that is always empty — it looks like a
/// feature and answers like a fault — so the capability is an `Option` on the store contract
/// rather than a method every store has to write.
///
/// `None` is a fact and not a failure, and the difference is worth keeping all the way out to a
/// reader. A store that was asked and refused has a fault somebody can fix; a store with no
/// index has nothing to fix and a different way to search it. A fan-out reads
/// [`WorkFs::stores`](crate::fs::WorkFs::stores) rather than a list of backends precisely so it
/// can say which of the two a place was — "nothing found here" and "nobody looked here" are
/// different answers, and only the second is true of a store with no index.
///
/// Which stores those are is a fact about somebody else's API on a date, and it moves. Discord
/// offered bots no message search until it shipped one in March 2026 — and this is why that
/// costs an `index` override rather than a redesign.
///
/// # A store of your own
///
/// Nothing here is sealed and nothing has to be registered anywhere. A store written outside
/// this crate implements [`FileSystem`](super::FileSystem), overrides
/// [`index`](super::FileSystem::index), and is searchable the moment it is mounted — because
/// whatever fans out reads the mount table and nothing else.
///
/// ```
/// use std::io;
/// use std::path::Path;
///
/// use cortex::BoxFuture;
/// use cortex::fs::{Dirent, FileSystem, Hit, SearchResult, Searchable, Stat, WorkFs};
///
/// struct Tickets;
///
/// impl Searchable for Tickets {
///     fn search<'a>(&'a self, query: &'a str, _count: usize) -> BoxFuture<'a, SearchResult> {
///         Box::pin(async move {
///             // Ask your service, then name the file each hit can be read in. The path is
///             // spelled by *your* tree, because you are the one who laid that tree out.
///             Ok(vec![Hit {
///                 path: format!("open/{query}.json"),
///                 record: b"{\"id\":\"T-1\"}\n".to_vec(),
///             }])
///         })
///     }
///
///     fn describe(&self) -> &str {
///         "tickets"
///     }
/// }
///
/// impl FileSystem for Tickets {
///     // The tree itself: `stat`, `list`, `read_at`. A hit is a path, so the files it names
///     // have to be there — an index with no tree under it answers with paths that do not open.
/// #   fn stat<'a>(&'a self, _p: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
/// #       Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
/// #   }
/// #   fn list<'a>(&'a self, _p: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
/// #       Box::pin(async { Ok(Vec::new()) })
/// #   }
/// #   fn read_at<'a>(
/// #       &'a self,
/// #       _p: &'a Path,
/// #       _b: &'a mut [u8],
/// #       _o: u64,
/// #   ) -> BoxFuture<'a, io::Result<usize>> {
/// #       Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
/// #   }
///
///     fn index(&self) -> Option<&dyn Searchable> {
///         Some(self)
///     }
/// }
///
/// let mut work = WorkFs::new();
/// work.mount("tickets", Tickets)?;
///
/// // Mounting was the registration. Nothing else was told where this store is.
/// let mounted = work.stores();
/// assert_eq!(mounted.len(), 1);
/// assert_eq!(mounted[0].0.to_str(), Some("tickets"));
/// assert!(mounted[0].1.index().is_some());
/// # Ok::<(), std::io::Error>(())
/// ```
pub trait Searchable: Send + Sync {
    /// Hits for `query`, most relevant or most recent first, at most `count` of them.
    ///
    /// The query is the service's own syntax, passed through untouched. A common dialect over
    /// several indexes is the one thing this refuses to build: an index's operators are how its
    /// users already search, its semantics are its own (whose `from:` is a handle and whose is
    /// an address substring), and a translation layer would have to claim they agree.
    fn search<'a>(&'a self, query: &'a str, count: usize) -> BoxFuture<'a, SearchResult>;

    /// One line about what this index is, for the report a fan-out prints.
    fn describe(&self) -> &str {
        ""
    }
}
