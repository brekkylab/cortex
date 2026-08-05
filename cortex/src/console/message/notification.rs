//! The methods nothing answers.
//!
//! A [`Notification`] is a method with no `id`, and so no response, no error and
//! no result — which makes it the right shape for exactly one kind of thing: what
//! is true whether or not the other end acknowledges it. There is one today.
//!
//! Its own file rather than a corner of [`call`](super::call)'s because the
//! difference between the two is the whole of what JSON-RPC's `id` decides, and a
//! method being added should have to choose a side.

use bson::Bson;
use serde::de;
use serde::ser::SerializeMap;

use super::Method;

/// A method that is not answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    /// **client → server, last.** The session is over; exit.
    ///
    /// There is nothing a process can say after this that a closed channel does
    /// not say better — which is exactly why it is a notification. Sending it at
    /// all is what lets the other end tell a finished session from a peer that
    /// died.
    Quit,
}

impl Notification {
    pub fn method(&self) -> Method {
        match self {
            Notification::Quit => Method::Quit,
        }
    }

    /// Writes this notification's `params` into the object being serialized.
    ///
    /// Nothing carries any today, and the match is what makes that a decision
    /// rather than an omission: a notification added later cannot compile without
    /// saying what it sends.
    pub(super) fn serialize_params<M: SerializeMap>(&self, _map: &mut M) -> Result<(), M::Error> {
        match self {
            Notification::Quit => Ok(()),
        }
    }

    /// The notification a `method` and its `params` name.
    ///
    /// Reached only for a message that carried no `id`, which is what says nothing
    /// will answer it — so a request's method arriving here is a peer that has
    /// asked for something and left no way to be told.
    pub(super) fn from_params<E: de::Error>(method: Method, _params: Bson) -> Result<Self, E> {
        match method {
            Method::Quit => Ok(Notification::Quit),
            _ => Err(E::custom(format!("{method} is a request and needs an id"))),
        }
    }
}
