//! What one mount is allowed to spend.
//!
//! These were constants, which was right while a mount was a person's own tree on their own
//! machine. A host serving many people is a different question with the same numbers in it:
//! every mount holds its own caches, so "how much does one cost" becomes "how much does a
//! thousand cost", and that is a decision for whoever runs the process rather than for this
//! file. So the numbers are still here as the defaults, and every one of them can be moved.
//!
//! Each field carries the reasoning that picked its default, because that is what a reader
//! about to change one needs — not the value, which they can see.

use chrono::NaiveDate;
use std::time::Duration;


/// The most bytes one open attachment may hold.
const WINDOW: u64 = 8 << 20;

/// How long anything fetched stays reusable.
const TTL: Duration = Duration::from_secs(15);

/// Assembled text one store keeps.
const TEXT_BUDGET: u64 = 32 << 20;

/// The budgets one [`MessengerFs`](super::MessengerFs) reads within.
///
/// [`Default`] is what this lane chose for a single mount, and a consumer that never mentions
/// this gets exactly the behaviour it always had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessengerLimits {

    /// The most bytes one open attachment may hold, and so both of the things that number
    /// decides: below it an attachment is fetched whole at `open`, above it a window at a time.
    /// Default 8 MiB.
    ///
    /// One number for both because it is really one rule — a ceiling on what a handle costs in
    /// memory. A mount serving several readers holds one of these at a time, so an attachment
    /// nobody bounded is a mount a single 2 GB upload can stop.
    ///
    /// Measurement sets a floor and not the value: a reader asks in 128 KiB pieces at most
    /// (32 KiB through FUSE-T), so a window orders smaller than this spends the round trip it
    /// saved on the very next read. What picks the value is a trade this lane has rather than an
    /// object store: the *only* shape whose length can be checked against the listing is the
    /// whole one — the check that catches a refused token answering 200 with a login page. So
    /// raising this verifies more of the real population and costs that much memory per open
    /// file; lowering it drops screenshots and reports into the unverified path to save memory.
    pub window: u64,

    /// How long anything fetched stays reusable. Default 15 seconds.
    ///
    /// One number for every kind, because they are all the same trade: a `stat` followed by an
    /// `open` is two calls into this volume for one thing a reader is doing, and without a
    /// window in which the second is free every `ls -l` pays for the whole listing twice.
    ///
    /// It is freshness and not correctness: a longer one serves a message that arrived in the
    /// meantime a little late, a shorter one pays for the same day twice in one `find`.
    pub ttl: Duration,

    /// Assembled text — days and threads, rendered — this store keeps at once. Default 32 MiB.
    ///
    /// Counted in bytes and not in entries, because entries are not comparable: a quiet day is
    /// a hundred bytes and a busy one is megabytes, so a count bounds nothing worth bounding.
    /// Past the budget the least recently used scope is dropped, which costs the request that
    /// assembled it if somebody comes back.
    ///
    /// It is a ceiling on the *rest*, not on every case: a single day larger than the whole
    /// budget stays while it is the only thing held, because dropping it would refetch the same
    /// day for every chunk a kernel reads — the cost the attachment window exists to avoid,
    /// paid by text instead.
    ///
    /// **Per store.** A host with a thousand mounts multiplies this by a thousand, and that is
    /// the number to set against the machine — either here, or by bounding how many mounts it
    /// keeps alive.
    pub text_budget: u64,

    /// The day the date axis ends at, or `None` for the real clock.
    ///
    /// Pinned, a mount is reproducible: the same conversation lists the same months today and
    /// next year. That is worth something to a host serving a frozen archive, and it is what
    /// lets a test assert a year list without the assertion expiring — `Utc::now()` in the axis
    /// means every such test has a date after which it can never pass again.
    ///
    /// It is also the only way to reach a month that is somehow ahead of this clock. Messages
    /// dated in the future exist — imported archives carry them, and so does a skewed
    /// timestamp — and with the axis stopping at the real today they are in no month any
    /// listing names.
    pub today: Option<NaiveDate>,
}

impl Default for MessengerLimits {
    fn default() -> Self {
        MessengerLimits {
            window: WINDOW,
            ttl: TTL,
            text_budget: TEXT_BUDGET,
            today: None,
        }
    }
}
