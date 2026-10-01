//! Locking that survives a panic somewhere else.

use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, ignoring poisoning.
///
/// Locks nest (directory node → child), so unwrapping a poisoned child panics while
/// the parent is held, poisoning it too, until every operation on the volume panics. Each lock
/// guards a single insert/remove/resize, so poison means another thread panicked, not that the
/// data is half-written.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
