//! Tests for the lane's error policy.
//!
//! No platform appears here, which is the point: what is asserted is what *every* source's
//! failures mean once classified, so a source added later inherits these rather than
//! re-deciding them. Which of its own codes fall into which class is that source's test.

use super::*;

fn api(class: ErrorClass) -> SourceError {
    SourceError::Api(ApiError {
        op: "conversations.history".into(),
        code: "some_code".into(),
        detail: None,
        class,
    })
}

/// The one question the tree asks. Only the narrowest class answers yes — everything wider
/// would have a conversation served as empty when the truth is that nothing is readable.
#[test]
fn only_a_conversation_denial_may_be_absorbed() {
    assert!(api(ErrorClass::ConversationDenied).is_conversation_denied());
    for class in [
        ErrorClass::ScopeMissing,
        ErrorClass::Unauthenticated,
        ErrorClass::Other,
    ] {
        assert!(
            !api(class).is_conversation_denied(),
            "{class:?} must propagate"
        );
    }
}

/// A transport failure has no class at all. It reaches the same classifier as an API error
/// and must fall out of the absorb set, or a network blip is served as an empty conversation
/// — a wrong answer that looks exactly like a right one.
#[test]
fn a_transport_failure_is_never_absorbed() {
    let e = SourceError::io("connection reset");
    assert!(e.class().is_none());
    assert!(!e.is_conversation_denied());
}

/// `find`/`rsync` skip an `EACCES` subtree and abort on `EIO`, so a workspace one scope short
/// has to answer the first — while a class this crate cannot narrow must not have a kind
/// invented for it.
#[test]
fn only_credential_classes_map_to_permission_denied() {
    for class in [ErrorClass::ScopeMissing, ErrorClass::Unauthenticated] {
        let e = io::Error::from(api(class));
        assert_eq!(
            e.kind(),
            io::ErrorKind::PermissionDenied,
            "{class:?} should be EACCES"
        );
    }
    for class in [ErrorClass::ConversationDenied, ErrorClass::Other] {
        let e = io::Error::from(api(class));
        assert_ne!(e.kind(), io::ErrorKind::PermissionDenied, "{class:?}");
        // And the code survives, because that is what a reader looks up.
        assert!(e.to_string().contains("some_code"), "{e}");
    }
    // A transport failure keeps the io::Error it already was.
    let e = io::Error::from(SourceError::io("connection reset"));
    assert!(e.to_string().contains("connection reset"), "{e}");
}

/// A `ConversationDenied` reaching the errno mapping means the tree failed to absorb it. It must
/// not become `PermissionDenied` there: the whole mount would then be reported as forbidden
/// because of one channel a bot was not invited to.
#[test]
fn an_unabsorbed_conversation_denial_does_not_forbid_the_mount() {
    let e = io::Error::from(api(ErrorClass::ConversationDenied));
    assert_ne!(
        e.kind(),
        io::ErrorKind::PermissionDenied,
        "one channel must not read as the workspace being forbidden: {e}"
    );
}

/// The message names what was asked and what came back — the two things a person debugging a
/// mount has to have, and the reason `op` is carried at all.
#[test]
fn a_message_names_the_call_and_the_code() {
    let e = ApiError {
        op: "conversations.history".into(),
        code: "missing_scope".into(),
        detail: Some("needed: channels:history; provided: channels:read".into()),
        class: ErrorClass::ScopeMissing,
    };
    let s = e.to_string();
    assert!(s.contains("conversations.history"), "{s}");
    assert!(s.contains("missing_scope"), "{s}");
    assert!(s.contains("channels:history"), "{s}");
}
