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
//! * **recall** comes from whatever index the service already has — one query, whatever its
//!   pages cost, and it reaches content the tree would have to walk to find;
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
//! # Where the stores come from
//!
//! From the mount table, and from nowhere else. [`Search::over`] reads
//! [`WorkFs::stores`](cortex::fs::WorkFs::stores), so mounting *is* the registration: a store
//! that answers [`FileSystem::index`](cortex::fs::FileSystem::index) is searchable from the
//! moment it is in the tree, and one that does not is named as unsearched rather than asked.
//!
//! That is not a convenience. A hit is only useful because its path opens, and it opens only
//! because it is prefixed with where the store actually is. A command told that path separately
//! is a command that can be told a different one, and a wrong prefix is indistinguishable
//! downstream from a file that was deleted — hits that name nothing, a zero exit code, and
//! nothing anywhere saying why. Taking the path and the index from the same table leaves no
//! second spelling to keep in step.
//!
//! # Why an index is an `Option` on the store and not a method every store writes
//!
//! Not every service has an index a credential may ask. Slack refuses `search.messages` to a
//! bot token, and an object store has no text index whatsoever. A trait method some stores
//! could only ever fail is the same mistake as a directory that is always empty — it looks like
//! a feature and answers like a fault. So the capability is
//! [`FileSystem::index`](cortex::fs::FileSystem::index), `None` by default, and a store that
//! has none is never asked rather than asked and failed.
//!
//! # Three answers, not two
//!
//! Which is why the whole mount table is read and not a list of the stores that can be asked.
//! A reader looking at the report has to be able to tell these apart:
//!
//! ```text
//! # chat/slack   12 hits (messages)      asked, and here is what it had
//! # docs/notion   0 hits (titles only)   asked, and it has none
//! # mail/work    could not search: …     asked, and it would not answer
//! # files/s3      no index               nobody looked here
//! ```
//!
//! The last two are the pair worth the trouble. A refusal is a **fault** — a token somebody can
//! renew, a scope somebody can grant — and it forces a non-zero exit, because a store that could
//! not look has not said the thing is absent. Having no index is a **fact**: there is nothing to
//! fix, it moves no exit code, and what it calls for is `grep`, which reads the files rather
//! than asking about them.
//!
//! A fan-out built from a list of backends could not draw that last line at all. The store was
//! never handed over, so there was no place to report — and "nothing found" would have been said
//! about somewhere nobody read.
//!
//! # What it does not replace
//!
//! An index cannot enumerate ("every message in this channel that day"), cannot be composed,
//! and ranks by its own rules — which is why hits are grouped by store and never merged into
//! one ranking. Use it to find where to look; use the files to read what is there.

mod exec;

pub use exec::{Search, usage, wants_help};
