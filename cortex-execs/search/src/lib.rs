//! `search` — the indexes behind the mounted stores, answered as paths into this tree.
//!
//! # What this is for
//!
//! A store in this crate's world is a tree of files, so a reader filters with the tools it
//! already has: `grep`, `jq`, a glob. That holds for *tokens* and not for *requests*. Walking a
//! synthesized tree costs a call per directory it invents — a day of chat, a page, a thread —
//! so `grep -r` over a mount is thousands of them and a rate limit answers before the search
//! does.
//!
//! This is the other half, and the division is the whole design:
//!
//! * **recall** comes from whatever index the service already has — one request, and it reaches
//!   content the tree would have to walk to find;
//! * **precision** comes from the tree, because the hits are *paths*, and `grep` over a handful
//!   of named files says things no index can express.
//!
//! ```text
//! search 'pricing' | cut -f1 | sort -u | tr '\\n' '\\0' | xargs -0 grep -nHE 'A/B ?test'
//! ```
//!
//! That pipeline is why this crate does not grow a query language. An index is asked for plain
//! words, which every index can do; anything sharper happens locally, where it is portable
//! already.
//!
//! # Why a command and not a method on the stores
//!
//! Not every service has an index a credential may ask. Slack refuses `search.messages` to a
//! bot token, and an object store has no text index whatsoever. A
//! trait method some stores could only ever fail is the same mistake as a directory that is
//! always empty — it looks like a feature and answers like a fault. A store with no index has
//! no [`Searchable`] backend, and a fan-out *reports* that rather than answering nothing.
//!
//! # What it does not replace
//!
//! An index cannot enumerate ("every message in this channel that day"), cannot be composed,
//! and ranks by its own rules — which is why hits are grouped by store and never merged into
//! one ranking. Use it to find where to look; use the files to read what is there.

mod exec;
mod searchable;

pub mod backend;

pub use exec::{Search, usage};
pub use searchable::{Hit, SearchResult, Searchable};
