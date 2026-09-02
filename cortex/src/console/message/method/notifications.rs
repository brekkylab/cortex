//! The methods nothing answers, all three of them.
//!
//! One file rather than three, because each of these is a name and a paragraph and
//! nothing else — there is no `params` type to grow, no `result` to pair it with. What
//! they have to say is mostly about *each other*: [`Start`] and [`Stop`] are two ends of
//! the same trade, and [`Quit`] is the one that is not that trade at all.
//!
//! [`Start`] and [`Stop`] are **the protocol's resource management, and only that**.
//! Neither changes what a session can do — a call that needs a booted session boots one,
//! so a client that sends neither runs the same commands to the same results. What they
//! change is what the far end is *holding*, and when it paid to hold it. Neither is a
//! question, which is why neither is answered: what a client does next is the same
//! either way. [`Quit`] is the session ending, which a closed channel says better than
//! any response could.
//!
//! Each is a type with no members rather than no type at all, so that every method has
//! one place where what it carries is written down, and giving one a parameter later is
//! a field and not a shape that did not exist.

use serde::{Deserialize, Serialize};

/// Boot now, so that no command has to. The `params` of `start`, which are none.
///
/// **What it buys is hiding the cold start.** An [`ExecCall`](super::ExecCall), a
/// [`ReadCall`](super::ReadCall) and a [`WriteCall`](super::WriteCall) each need a booted session and
/// each boot one if there is none, so nothing here is required and nothing is unlocked
/// by it. What moves is *who waits*: a backend with a kernel to bring up makes the first
/// command pay for that in its own latency, and a client that says this as soon as it
/// has a console pays for it in parallel with whatever it is doing meanwhile — choosing
/// what to run, waiting on a model, reading a file.
///
/// Which is also why nothing answers it. There is no state a client is entitled to hear
/// about — a session that failed to boot and one that has not booted yet are the same
/// session, since the next call that needs one will try again — so a failure goes to
/// whoever asks for that call, as [`BOOT_FAILED`](super::Error::BOOT_FAILED). That is
/// the end that was waiting on something and the end that can do something about it.
///
/// Sent to a session that is already booted, it does nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start;

/// Release what booting took. The `params` of `stop`, which are none.
///
/// **What it buys is not occupying anything while nothing is being run.** Much what
/// stopping a VM is: the guest goes away, the tree is unmounted, and a scratch directory
/// is cleaned up by whoever made it rather than left for someone to find later. Those are
/// memory, descriptors and disk on the far end, held for as long as the session is booted
/// and useful only while something is running — so a client that knows it will be idle for
/// a while is worth letting say so.
///
/// Optional and reversible, like [`Start`] and for the same reason: the next call that
/// needs a booted session gets one, under the same [`InitCall`](super::InitCall). Handing
/// resources back therefore costs a client nothing but the boot it will pay for again —
/// which is the trade it is making, and the reason this is worth sending when the idle
/// stretch is long and not when it is two commands apart.
///
/// So this is a release and not a close: ending the *process* is [`Quit`], and a session
/// that is merely stopped is still a session.
///
/// Nothing here is about a single execution. To give up on one of those, let its
/// [`timeout_ms`](super::ExecCall::timeout_ms) expire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop;

/// The session is over; exit. The `params` of `quit`, which are none.
///
/// There is nothing a process can say after this that a closed channel does not say
/// better — which is exactly why it is a notification. Sending it at all is what lets
/// the other end tell a finished session from a peer that died.
///
/// A server on its way out releases whatever a [`Stop`] would have released, so an
/// ending is one message and not two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quit;
