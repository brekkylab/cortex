//! The tree answering its own index: matches in, paths into this tree out.
//!
//! Here rather than beside the command that fans out over several stores, because every line of
//! it is knowledge about *this* layout — which file a message's line is in, whose name goes on
//! it, how a line is spelled. The command has no business knowing any of that, and the next
//! store's answers to the same three questions are different.
//!
//! # Why the tree does this and not the source
//!
//! A [`MessengerIndex`] answers in matches, which carry a conversation and a message and no
//! path at all. Spelling the path is [`chat_path`], and it needs two things the match does not
//! have: the conversation as the *listing* describes it, and the roster that resolves an author
//! id to a name.
//!
//! Both are already here. [`conversations`](MessengerFs::conversations) and
//! [`users`](MessengerFs::users) are the same cached fetches a directory listing goes through,
//! so a search behind a mount somebody has been reading is served out of what that reading
//! already paid for. That is the whole reason this impl is on the tree: a backend beside the
//! tree would hold a second cache over the same credential, and the two would pay for the same
//! roster twice while each thought it was saving one.
//!
//! What it costs is that the reuse window is the store's
//! [`ttl`](super::MessengerLimits::ttl) rather than one chosen for searching. Fifteen seconds
//! suits a `stat` followed by the `open` behind it; two searches with a person deciding what to
//! ask in between are further apart than that, and the second pays the roster again. A consumer
//! that expects interactive searching raises `ttl`, which is the knob for exactly this and is
//! already the store's to spend.

use std::collections::HashMap;

use super::paths::chat_path;
use super::source::{Conversation, MessengerSource, render_line};
use super::messenger::MessengerFs;
use crate::BoxFuture;
use crate::fs::{Hit, SearchResult, Searchable};

impl<S: MessengerSource> Searchable for MessengerFs<S> {
    fn search<'a>(&'a self, query: &'a str, count: usize) -> BoxFuture<'a, SearchResult> {
        Box::pin(async move {
            // Unreachable through `FileSystem::index`, which hands out `Some(self)` only when
            // the source has one. Answered rather than panicked because this is a public trait
            // method and nothing stops a caller holding the store directly.
            let index = self
                .source_index()
                .ok_or_else(|| String::from("this source has no index"))?;

            let found = index.search(query, count).await.map_err(|e| e.to_string())?;
            if found.is_empty() {
                // Before the two fetches below, deliberately: nothing to name is nothing to
                // spend a roster on, and an empty answer is the common one for a narrow query.
                return Ok(Vec::new());
            }

            // Once for the whole answer rather than per hit. Both come from the store's cache
            // when a listing has been through recently, and both are all-or-nothing at the
            // source — so a failure here is total, and total is what makes it legible.
            //
            // Empty on failure rather than fatal, and the two failures that reach here are
            // worth telling apart. A dead credential does not: the index was asked first and
            // propagated, so the store already reported that it could not search. What is left
            // is an install granted `search:read` and not `users:read`, where every hit comes
            // back nameless every time and the cause is a scope table away — and a roster walk
            // the service broke off, which a source answers all-or-nothing, so past two hundred
            // members a throttle that outlives its retries costs every name rather than the
            // pages it did not reach. Both read better than the partial roster they replace:
            // all of the hits missing a name reads as a fault, where five of twenty reads as
            // five people who left.
            let names: HashMap<String, String> = self
                .users()
                .await
                .map(|us| us.into_iter().map(|u| (u.id, u.name)).collect())
                .unwrap_or_default();
            // The listing is the authority on the human half of a directory name; an index may
            // answer with less, a one-to-one DM with no display name for one. A path built from
            // the match alone still opens — `<name>__<id>` is addressed by the id — but it can
            // be spelled unlike the same directory in `ls`, which is a path a reader cannot
            // match up with what they just listed.
            let listed: HashMap<String, Conversation> = self
                .conversations()
                .await
                .map(|cs| cs.into_iter().map(|c| (c.id.0.clone(), c)).collect())
                .unwrap_or_default();

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

#[cfg(test)]
#[path = "search_tests.rs"]
mod search_tests;
