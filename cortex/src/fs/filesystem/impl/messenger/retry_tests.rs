//! Tests for the shared retry arithmetic.
//!
//! No requests and no platform: every case is a decision about a delay, which is the whole
//! reason this is a module of its own rather than a section of one source.

use super::*;

/// Retry `n` waits in `[2^n s, 2^n s + 1s]`, capped at [`MAX_BACKOFF`]. The jitter floor
/// matters as much as the ceiling: without it, the concurrent readers that tripped this limit
/// together would retry together and trip it again.
///
/// Looped, because the jitter comes from the clock: one pass could land on any single value
/// and prove nothing about the bounds.
#[test]
fn backoff_is_exponential_jittered_and_capped() {
    for _ in 0..100 {
        for n in 0..3u32 {
            let base = Duration::from_secs(1u64 << n);
            let d = backoff_delay(n);
            assert!(
                d >= base && d <= base + Duration::from_millis(JITTER_MAX_MS),
                "n={n}: {d:?}"
            );
        }
        // 2^4 = 16s already meets the cap, so jitter cannot push past it.
        assert_eq!(backoff_delay(4), MAX_BACKOFF);
        // The shift is clamped, so a large n saturates instead of overflowing.
        assert_eq!(backoff_delay(64), MAX_BACKOFF);
    }
}

/// The jitter has to actually vary, or the decorrelation it exists for does not happen — and
/// a clock-derived value is exactly the kind that can silently return a constant (a coarse
/// `SystemTime`, a truncating conversion). Asserted over the raw source rather than through
/// `backoff_delay`, whose sub-second component a `Duration` comparison would hide.
#[test]
fn the_jitter_varies() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        seen.insert(jitter_ms());
        if seen.len() > 1 {
            return;
        }
    }
    panic!("jitter_ms returned {seen:?} a thousand times; readers would retry in lockstep");
}

/// A `Retry-After` past the ordinary cap must not be clamped to it: five sub-cap sleeps all
/// land back inside the window the server named, spending 80s of waiting on a certain failure
/// — worse than the 31s the no-header path spends. Such a delay is sat out once, in full, and
/// only up to what a blocked reader can be held for.
/// A honored delay is at least what the server asked for and at most one jitter past it.
/// Never *less*: coming back before the moment the server named is the one thing a
/// `Retry-After` rules out.
fn honored(got: Option<(Duration, bool)>, asked: Duration, long: bool) {
    let (d, spent) = got.expect("a wait was available");
    assert_eq!(spent, long, "spent the wrong budget for {asked:?}");
    assert!(
        d >= asked && d <= asked + Duration::from_millis(JITTER_MAX_MS),
        "{d:?} is not {asked:?} plus at most a jitter"
    );
}

#[test]
fn a_long_retry_after_is_honored_once_and_never_clamped() {
    let secs = Duration::from_secs;
    // Within the cap: the ordinary budget, and the header is obeyed rather than grown.
    honored(next_wait(Some(secs(10)), 0, false), secs(10), false);
    assert_eq!(next_wait(Some(secs(10)), MAX_RETRIES, false), None);

    // Past the cap: sat out in full, not clamped — and it spends the one long wait rather
    // than a retry, so it cannot repeat.
    honored(next_wait(Some(secs(60)), 0, false), secs(60), true);
    assert_eq!(next_wait(Some(secs(60)), 0, true), None);
    // Still available even with the ordinary budget gone: the two are separate.
    honored(next_wait(Some(secs(60)), MAX_RETRIES, false), secs(60), true);

    // Past what a blocked reader may be held for: refused outright. Measured against what the
    // server asked, so our own jitter cannot push a borderline delay over the line.
    assert_eq!(next_wait(Some(secs(3600)), 0, false), None);
    honored(
        next_wait(Some(MAX_HONORED_WAIT), 0, false),
        MAX_HONORED_WAIT,
        true,
    );

    // No header: exponential, capped, on the ordinary budget.
    for n in 0..MAX_RETRIES {
        let (w, long) = next_wait(None, n, false).expect("within budget");
        assert!(!long, "the exponential path must not spend the long wait");
        assert!(w <= MAX_BACKOFF, "{w:?}");
    }
    assert_eq!(next_wait(None, MAX_RETRIES, false), None);
}

/// The header path does not grow. A `Retry-After` names when the window rolls, so waiting
/// longer each time spends throughput on a limit that has already lifted — growth is for the
/// path that does not know, which is the one without a header.
#[test]
fn an_obeyed_delay_does_not_grow_across_retries() {
    let asked = Duration::from_secs(2);
    for n in 0..MAX_RETRIES {
        honored(next_wait(Some(asked), n, false), asked, false);
    }
}

/// And it is spread, which is what the jitter is actually for. Readers that tripped one limit
/// together are told the *same* delay, so obeying it exactly would wake them together to trip
/// it again — a case the header-less path never had, since differing retry counts already
/// separate those.
#[test]
fn readers_told_the_same_delay_do_not_wake_together() {
    let asked = Duration::from_secs(2);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let (d, _) = next_wait(Some(asked), 0, false).expect("a wait");
        seen.insert(d);
        if seen.len() > 1 {
            return;
        }
    }
    panic!("every reader given the same Retry-After waited {seen:?} — perfect lockstep");
}
