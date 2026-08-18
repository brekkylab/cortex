//! When to try a throttled request again.
//!
//! Here rather than in whichever source needed it first, because none of it mentions a
//! platform: every messenger API this lane reads answers 429 with a `Retry-After`, and the
//! question "given that header, that many retries so far, and whether the one long wait is
//! spent — how long now, if at all" has the same answer for all of them. It is arithmetic,
//! not an abstraction, which is why it is shared from the first source rather than extracted
//! from the second.
//!
//! What is *not* here is anything about how a platform reports being throttled. Slack says
//! it twice — a 429, and `{"ok": false, "error": "ratelimited"}` inside a 200 — and Discord
//! carries bucket headers; recognizing either is the source's job, and both then ask
//! [`next_wait`] the same question.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Retries a source may spend on the ordinary budget. A listing fans out per-conversation
/// calls, so a burst can trip a per-method limit; a bounded retry keeps a transient limit
/// from failing a whole read.
pub(super) const MAX_RETRIES: u32 = 5;

/// Cap on an ordinary retry wait. These calls sit behind a filesystem op the caller blocks
/// on, so the budget stays short. Raising it means moving the wait off that path, not just
/// raising the number.
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(16);

/// Ceiling on a `Retry-After` this lane will sit out (see [`next_wait`]). Rate-limit delays
/// run seconds to a minute; anything past this is not worth holding a blocked reader for,
/// however patient the server asks us to be.
pub(super) const MAX_HONORED_WAIT: Duration = Duration::from_secs(60);

/// Jitter ceiling, recomputed per wait so concurrent readers don't retry in lockstep.
pub(super) const JITTER_MAX_MS: u64 = 1000;

/// Jitter for one retry, in `0..=JITTER_MAX_MS`.
///
/// Taken from the clock's sub-second bits rather than a random number generator. What this
/// needs is for two readers that tripped the same limit together to stop retrying together,
/// and two tasks reaching this line arrive at different nanoseconds — so the requirement is
/// decorrelation, not randomness, and this crate counts its dependencies (see `Cargo.toml`).
/// A clock before the epoch yields no jitter rather than failing: the backoff floor still
/// holds, which is the part that must not be skipped.
pub(super) fn jitter_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % (JITTER_MAX_MS + 1))
        .unwrap_or(0)
}

/// Exponential backoff with jitter for retry `n` (0-based).
pub(super) fn backoff_delay(n: u32) -> Duration {
    let base = Duration::from_secs(1u64 << n.min(16));
    (base + Duration::from_millis(jitter_ms())).min(MAX_BACKOFF)
}

/// The delay before the next attempt, and whether it spends the one long wait.
/// `None` = stop retrying.
///
/// # Exponential only where the server said nothing
///
/// A `Retry-After` is the server naming the moment the window rolls, so it is obeyed as sent
/// and does not grow across retries: inflating it would spend throughput waiting past a limit
/// that has already lifted. Growth is for *not knowing*, which is the header-less path.
///
/// Jitter, though, applies to both — and matters more here. Readers that tripped one limit
/// together are told the same delay, so obeying it exactly wakes them in perfect lockstep to
/// trip it again; the header-less path was already spread out by their differing retry
/// counts. It is only ever *added*, never subtracted, so a wait never ends before the moment
/// the server named.
///
/// A `Retry-After` longer than [`MAX_BACKOFF`] cannot be waited out on the ordinary budget:
/// no arrangement of five sub-cap sleeps outlasts the window the server named, so every
/// retry lands back inside it and the read is spent for nothing. Such a delay is therefore
/// sat out **once**, in full, and only up to [`MAX_HONORED_WAIT`]; after that the read fails
/// while the window is presumably still open, which the error says.
pub(super) fn next_wait(
    asked: Option<Duration>,
    retries: u32,
    long_spent: bool,
) -> Option<(Duration, bool)> {
    let spread = |d: Duration| d + Duration::from_millis(jitter_ms());
    match asked {
        Some(d) if d > MAX_BACKOFF => {
            // Checked against what the server asked for, then padded: the ceiling is a
            // judgement about the server's number, not about our own jitter.
            (!long_spent && d <= MAX_HONORED_WAIT).then(|| (spread(d), true))
        }
        Some(d) => (retries < MAX_RETRIES).then(|| (spread(d), false)),
        None => (retries < MAX_RETRIES).then(|| (backoff_delay(retries), false)),
    }
}

/// The `Retry-After` delay on a response, if it carries one as delta-seconds.
///
/// The HTTP-date form is not honored (treated as absent → falls back to
/// [`backoff_delay`]): no messenger API in this lane sends it, and a date needs a clock
/// agreement this has no way to check.
pub(super) fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod retry_tests;
