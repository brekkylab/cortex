//! The messenger lane as a backend: a chat service's index, as paths into its tree.
//!
//! Everything here is lane knowledge — which file a message's line is in, whose name goes on
//! it, how a line is spelled. That is why it is a backend and not part of the command: the
//! command has no business knowing any of it, and the next lane's answers to the same three
//! questions are different.

use cortex::BoxFuture;
use cortex::fs::{Conversation, MessengerSource, SearchHit, chat_path, render_line};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{Hit, SearchResult, Searchable};

/// A messenger source that can be asked its service's own index.
///
/// Declared here rather than in the lane for the reason [`SearchHit`] gives: the lane's trait
/// is the set of questions every source answers, and this is the one only some can.
pub trait MessengerIndex: Send + Sync {
    fn search<'a>(
        &'a self,
        query: &'a str,
        count: usize,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>>;
}

/// How long a fetched roster or listing stays reusable. Five minutes.
///
/// Longer than the tree's fifteen seconds because the gap it exists to cover is a different
/// one. There, the pair is a `stat` and the `open` behind it — two syscalls of one command,
/// which is why seconds are enough. Here it is two *searches*, and what sits between them is
/// whoever is reading deciding what to ask next. Fifteen seconds would expire in that gap and
/// buy nothing.
///
/// Freshness and not correctness, as it is there: what goes stale is a display name, and the
/// id it belongs to is on the same line either way.
const REUSE: Duration = Duration::from_secs(300);

/// Something fetched, and when.
struct Held<T> {
    value: T,
    at: Instant,
}

/// The messenger backend over one source.
pub struct Messenger<S> {
    source: S,
    /// The roster and the conversation listing, held between calls.
    ///
    /// Held *here* because nothing below holds them: a source keeps nothing between calls, and
    /// the tree's cache belongs to the mount, which is a different object over the same
    /// credential. Without this, two searches a second apart pay for the roster twice — and on
    /// a large workspace the roster is most of what a search costs.
    ///
    /// A `Mutex` because [`Executable::exec`](cortex::exec::Executable::exec) takes `&self`,
    /// the same reason the mount's cache is one. Contention is not a concern: a fan-out asks
    /// its stores one at a time.
    names: Mutex<Option<Held<HashMap<String, String>>>>,
    convs: Mutex<Option<Held<HashMap<String, Conversation>>>>,
}

impl<S: MessengerSource + MessengerIndex> Messenger<S> {
    pub fn new(source: S) -> Self {
        Messenger {
            source,
            names: Mutex::new(None),
            convs: Mutex::new(None),
        }
    }

    /// What `slot` holds, if it was filled recently enough.
    fn held<T: Clone>(slot: &Mutex<Option<Held<T>>>) -> Option<T> {
        let guard = slot.lock().expect("a lock");
        guard
            .as_ref()
            .filter(|h| h.at.elapsed() < REUSE)
            .map(|h| h.value.clone())
    }

    fn hold<T>(slot: &Mutex<Option<Held<T>>>, value: T) {
        *slot.lock().expect("a lock") = Some(Held {
            value,
            at: Instant::now(),
        });
    }

    /// Author id to name, for the same reason the tree resolves them: a line nobody can `grep`
    /// for by person is a line that has to be read to be understood.
    ///
    /// Empty when the roster cannot be read, and then the name is simply absent from the line —
    /// which is what an absent `name` already means in this format. One missing grant does not
    /// turn a search into a failure.
    ///
    /// Two failures reach here and no others. A dead credential does not: the index is asked
    /// first and propagates, so the store reports that it could not search at all. What is left
    /// is an install granted `search:read` and not `users:read`, where every hit comes back
    /// nameless every time and the cause is a scope table away — and a roster walk the service
    /// broke off, which a source answers all-or-nothing, so past two hundred members a throttle
    /// that outlives its retries costs every name rather than the pages it did not reach. Both
    /// are more legible than the partial roster they replaced: all of the hits missing a name
    /// reads as a fault, where five of twenty reads as five people who left.
    async fn roster(&self) -> HashMap<String, String> {
        if let Some(held) = Self::held(&self.names) {
            return held;
        }
        let built = match self.source.users().await {
            Ok(users) => users.into_iter().map(|u| (u.id, u.name)).collect(),
            Err(_) => HashMap::new(),
        };
        // Held even when it came back empty: a failure that repeats every call is not worth
        // asking about once per search, and the emptiness is what the caller renders either way.
        Self::hold(&self.names, built.clone());
        built
    }

    /// The listing's own record per conversation, which is the authority on the human half of a
    /// directory name — an index may answer with less, a one-to-one DM with no display name for
    /// one. Empty when it cannot be read, and then each hit's own record names its path.
    ///
    /// A path built that way still opens: `<name>__<id>` is addressed by the id, and the tree
    /// resolves a conversation directory by the half after `__`. What differs is the readable
    /// half, so a hit's path can be spelled unlike the same directory in `ls`.
    async fn listing(&self) -> HashMap<String, Conversation> {
        if let Some(held) = Self::held(&self.convs) {
            return held;
        }
        let built = match self.source.conversations().await {
            Ok(convs) => convs.into_iter().map(|c| (c.id.0.clone(), c)).collect(),
            Err(_) => HashMap::new(),
        };
        // Held even when it came back empty: a failure that repeats every call is not worth
        // asking about once per search, and the emptiness is what the caller renders either way.
        Self::hold(&self.convs, built.clone());
        built
    }
}

impl<S: MessengerSource + MessengerIndex> Searchable for Messenger<S> {
    fn search<'a>(&'a self, query: &'a str, count: usize) -> BoxFuture<'a, SearchResult> {
        Box::pin(async move {
            let found = MessengerIndex::search(&self.source, query, count).await?;
            if found.is_empty() {
                return Ok(Vec::new());
            }

            // Once for the whole answer rather than per hit, which is why they are fetched
            // before the map and not inside it. Neither is cheap and neither is kept: a
            // roster is `ceil(members / 200)` requests, and a source holds nothing between
            // calls — the tree's cache belongs to the mount, not to this. So two searches in
            // one second pay for the roster twice, and on a large workspace that is most of
            // what a search costs.
            let names = self.roster().await;
            let listed = self.listing().await;

            Ok(found
                .iter()
                .map(|hit| {
                    let conv = listed.get(&hit.conv.id.0).unwrap_or(&hit.conv);
                    // A reply is served under the day its thread was started; anything else
                    // under its own.
                    let day = hit.thread_started.unwrap_or(hit.msg.ts);
                    let mut msg = hit.msg.clone();
                    if msg.from.name.is_none()
                        && let Some(n) = names.get(&msg.from.id)
                    {
                        msg.from.name = Some(n.clone());
                    }
                    Hit {
                        path: chat_path(conv, &msg, day),
                        record: render_line(&msg),
                    }
                })
                .collect())
        })
    }

    fn describe(&self) -> &str {
        "messages"
    }
}

#[cfg(feature = "slack")]
impl MessengerIndex for cortex::fs::SlackSource {
    fn search<'a>(
        &'a self,
        query: &'a str,
        count: usize,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, String>> {
        Box::pin(async move {
            cortex::fs::SlackSource::search(self, query, count)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
#[path = "messenger_tests.rs"]
mod messenger_tests;
