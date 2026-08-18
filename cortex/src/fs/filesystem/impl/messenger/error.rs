//! What a source can fail with, and what the tree is supposed to do about it.
//!
//! # Why the class is a field and not a question asked later
//!
//! Every messenger API in this lane reports a failure the same way — an operation, a
//! machine-readable code, sometimes a detail — and every one of them draws the same line
//! through those codes: a failure that applies to *one conversation* is not the same kind of
//! thing as one that applies to *all* of them. A bot that was never invited to one channel
//! must not fail the whole tree; a dead token served as an empty workspace presents it as a
//! complete one.
//!
//! What varies is only which codes fall on which side. So the source translates its own
//! vocabulary into [`ErrorClass`] once, where it builds the error, and everything downstream
//! — the tree's decision, the errno a filesystem answers with — is written here and shared.
//! The alternative, a `is_read_denied()` per source, is the same policy copied once per
//! platform, with nothing to notice when the fourth copy is subtly wrong.
//!
//! Classifying from a formatted message rather than a code is what makes a rename upstream
//! into a silent behaviour change, which is why [`ApiError::code`] is kept whole.

use std::io;

/// How far a failure reaches — the one question the tree has to answer about an error.
///
/// Deliberately about *reach* and not about cause. "Not in that channel" and "that channel
/// was deleted" have nothing in common as causes and are the same thing here: this one
/// conversation, and nothing else, is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// One conversation is not readable by this credential. The rest of the workspace is
    /// unaffected, so the tree serves that conversation empty rather than failing.
    ConversationDenied,

    /// A whole *kind* of conversation is not readable — a scope the install never granted.
    ///
    /// Not [`ConversationDenied`](Self::ConversationDenied), and the difference is the point:
    /// absorbed per conversation, a missing scope renders a section as "all of these exist
    /// and none has any history", which is a claim the tree cannot support. Only a caller
    /// that can narrow the request — a listing asking one kind at a time — may absorb it.
    ScopeMissing,

    /// The credential itself is not usable. Nothing is readable, so nothing may be served as
    /// empty.
    Unauthenticated,

    /// Something the source has no better name for. Propagates, because a code this crate
    /// has not seen is not one to guess the reach of.
    Other,
}

impl ErrorClass {
    /// Whether the tree may serve one conversation empty on this, rather than failing.
    ///
    /// The trait's absorb rule as a function. It was prose in a doc comment, which every
    /// source had to read and none had to obey.
    pub fn is_conversation_denied(self) -> bool {
        matches!(self, ErrorClass::ConversationDenied)
    }
}

/// A failure the messenger itself reported, with the code it named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// What was being asked — a method name, an endpoint. For the message only.
    pub op: String,
    /// The platform's machine-readable code, as text: `not_in_channel`, `Forbidden`, `50001`.
    /// Kept whole so a reader can look it up in the platform's own documentation.
    pub code: String,
    /// Whatever the platform said beyond its code — the needed vs granted scopes, a delay.
    pub detail: Option<String>,
    /// How far this reaches. The source decides it; nothing downstream re-derives it.
    pub class: ErrorClass,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.op, self.code)?;
        if let Some(s) = &self.detail {
            write!(f, " ({s})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// What a call to a source can fail with.
///
/// Two cases and not one, because the tree acts on the difference: an [`Api`](Self::Api)
/// failure carries a class it may be absorbed under, while an [`Io`](Self::Io) failure is the
/// network and applies to nothing in particular — which is what keeps "the connection reset"
/// out of the per-conversation soft-fail set.
#[derive(Debug)]
pub enum SourceError {
    /// The messenger answered, and said no.
    Api(ApiError),
    /// The call never got an answer to read.
    Io(std::io::Error),
}

impl SourceError {
    /// An `Io` failure carrying `msg`, for what a source detects itself: a body that is not
    /// the length the listing promised, a url pointing somewhere the token may not go.
    pub fn io(msg: impl std::fmt::Display) -> Self {
        SourceError::Io(std::io::Error::other(msg.to_string()))
    }

    /// The class behind this, when the messenger named one. A transport failure has none,
    /// which is what stops it being absorbed as a quiet conversation.
    pub fn class(&self) -> Option<ErrorClass> {
        match self {
            SourceError::Api(e) => Some(e.class),
            SourceError::Io(_) => None,
        }
    }

    /// Whether the tree may serve one conversation empty on this. See
    /// [`ErrorClass::is_conversation_denied`].
    pub fn is_conversation_denied(&self) -> bool {
        self.class().is_some_and(ErrorClass::is_conversation_denied)
    }

    /// Whether a scope the install never granted is what failed.
    ///
    /// Asked by the one caller that can do something about it: a listing that requested
    /// several kinds of conversation at once and can ask again for fewer. Absorbing it
    /// anywhere else turns a section into "these all exist and none has any history".
    pub fn is_scope_missing(&self) -> bool {
        self.class() == Some(ErrorClass::ScopeMissing)
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Api(e) => write!(f, "{e}"),
            SourceError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SourceError::Api(e) => Some(e),
            SourceError::Io(e) => Some(e),
        }
    }
}

impl From<reqwest::Error> for SourceError {
    fn from(e: reqwest::Error) -> Self {
        SourceError::io(e)
    }
}

impl From<SourceError> for io::Error {
    /// Flatten a source failure into the errno-shaped error a filesystem answers with.
    ///
    /// Only the two classes that are about the *credential* get a kind of their own, as
    /// [`PermissionDenied`](io::ErrorKind::PermissionDenied): userspace skips an `EACCES`
    /// subtree and aborts on `EIO`, so a workspace one scope short stays traversable instead of
    /// taking a whole `find` down with it.
    ///
    /// [`ConversationDenied`](ErrorClass::ConversationDenied) has no kind here because it
    /// should never arrive — the tree absorbs it, serving that conversation empty. Reaching this
    /// point means nobody did, so it keeps its message rather than being translated into a
    /// permission problem that would then be reported for the whole mount.
    fn from(e: SourceError) -> Self {
        match e {
            SourceError::Io(e) => e,
            SourceError::Api(api) => match api.class {
                ErrorClass::ScopeMissing | ErrorClass::Unauthenticated => {
                    io::Error::new(io::ErrorKind::PermissionDenied, api.to_string())
                }
                ErrorClass::ConversationDenied | ErrorClass::Other => {
                    io::Error::other(api.to_string())
                }
            },
        }
    }
}

/// What every method on a [`MessengerSource`](super::MessengerSource) hands back.
pub type SourceResult<T> = std::result::Result<T, SourceError>;

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
