//! Locking that survives a panic somewhere else.

use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, ignoring poisoning.
///
/// The locks in this crate nest (node → child, node → file data), so honouring
/// the poison is worse than ignoring it: `.unwrap()` on a poisoned child panics
/// *while the parent's lock is held*, poisoning the parent in turn, and one panic
/// walks to the root until every later operation on the volume panics too. Each
/// lock guards a single insert/remove/resize, so a poisoned one means another
/// thread panicked, not that the data is half-written.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
