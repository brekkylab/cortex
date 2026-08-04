//! The wire: JSON-RPC 2.0.
//!
//! One channel carries a whole session as a sequence of [`Message`]s — requests one
//! way, the responses answering them back — and they are real JSON-RPC objects, not a
//! shape that resembles them:
//!
//! ```text
//! {"jsonrpc":"2.0","id":2,"method":"exec","params":{"cmd":["sh","-c","ls"]}}
//! {"jsonrpc":"2.0","id":2,"result":{"code":0,"stdout":"YQo=","stderr":"","truncated":false}}
//! {"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"timed out after 1000ms"}}
//! {"jsonrpc":"2.0","method":"quit"}
//! ```
//!
//! so an end that is not this crate — and one day one may not be — can use an
//! off-the-shelf library rather than a bespoke implementation of ours.
//!
//! ```text
//! [u32 len][serialized Message]   — a frame
//! ```
//!
//! Framing is separate on purpose and lives outside this module: a serialized
//! `Message` does not know where it ends. A length prefix is the cheapest thing
//! that does — see [`MAX_PAYLOAD`].
//!
//! The split inside here is mostly the spec's own, between an object that is
//! answered and one that is not:
//!
//! - `message` — the envelope every object shares: [`Message`] and its three
//!   shapes, [`Method`], the [`RequestId`] that pairs a response with its request,
//!   and the serde impls that put all of it on the wire.
//! - `call` — the methods something answers: [`Call`], and the `params` and
//!   `result` they carry ([`Start`], [`Exec`], [`Progress`], [`ExecResult`]).
//! - `notification` — the methods nothing answers: [`Notification`].
//! - `outcome` — how something ended: [`Outcome`], and the [`Error`] and codes that
//!   are the second half of it.
//!
//! A `Call` and a `Notification` each write and read their own `params`, so adding
//! a method means touching the side it belongs to and not the envelope.
//!
//! `outcome` is the one that is not one of the spec's shapes, and it earned its place
//! by being on both sides of the request/response line: a response carries an outcome,
//! and so does the `resume` reporting a delegated call that ran in the client. It was
//! part of the envelope for as long as it was only ever a response's.
//!
//! # What the protocol is
//!
//! | Method | `params` | `result` | Errors |
//! |---|---|---|---|
//! | `start` | [`Start`] | `null` | [`BOOT_FAILED`](Error::BOOT_FAILED) |
//! | `exec` | [`Exec`] | [`Progress`] | [`TIMED_OUT`](Error::TIMED_OUT), [`NOT_EXECUTABLE`](Error::NOT_EXECUTABLE), [`NOT_STARTED`](Error::NOT_STARTED) |
//! | `resume` | [`Outcome`] | [`Progress`] | [`INVALID_REQUEST`](Error::INVALID_REQUEST) |
//! | `stop` | — | `null` | [`STOP_FAILED`](Error::STOP_FAILED) |
//! | `quit` | — | *(notification — no response)* | — |
//!
//! Every request gets exactly one response, correlated by `id`. `quit` is a
//! notification: no `id`, nothing answers it. **Every one of them is the client's:**
//! there is no method a server issues, which is what [`Progress`] is for.
//!
//! # Why the codec is now fixed
//!
//! JSON-RPC's own structure is what fixes it. `{"method":.., "params":..}` is
//! serde's adjacent tagging and `result` xor `error` is decided by which field is
//! *present* — both need a deserializer that can look ahead, which
//! non-self-describing codecs like postcard and bincode cannot do. A response's
//! `result` type also depends on the method its `id` was issued for, so reading
//! one means holding it as a value until the pending request identifies it, which
//! is what [`Outcome`] is.
//!
//! Self-describing is not the same as textual: CBOR and MessagePack would work,
//! and have a native byte type. So the `bytes` helper still asks the codec rather
//! than assuming, and base64 is the answer only because JSON is the answer.
//!
//! # Why errors carry codes
//!
//! An `error` is the only failure channel here, and a numeric `code` is what makes
//! it usable: a requester branches on the code and shows the `message`. This is
//! why there is no separate "timed out" or "no such executable" message shape —
//! they are codes, and adding a fifth outcome shape to say what a number already
//! says would only be a second thing to keep in sync.
//!
//! It is also what a code is *for* in JSON-RPC, and what the old wire could not
//! have: 127 for a program that was not there and 126 for one that could not be
//! started are codes any command can reach by an ordinary `exit()`, so neither
//! ever proved the failure was the server's. An `error` cannot be mistaken for a
//! command's own exit status, because it does not carry one.
//!
//! # Why execution runs both ways without a request going both ways
//!
//! A delegated executable's behaviour lives in the client, so a command inside the server
//! that invokes one by name cannot be finished by the server alone. The obvious spelling
//! is for the server to send an `exec` of its own — and that makes every end of the
//! channel both a requester and an answerer, with a pending table each and a read loop
//! that must never block on work only it can unblock.
//!
//! [`Progress`] is the spelling that does not. The server *answers* with a
//! [`Delegated`](Progress::Delegated): a complete, ordinary response to the request the
//! client is already waiting on, meaning *not finished, and here is what I need*. The
//! client runs the name and says so with `resume`, whose answer is the next `Progress`.
//!
//! So one channel, one end that asks, one end that answers — and one [`Exec`] type, since
//! a delegated call is the same shape as any other execution request: a command, some
//! input, output, a code at the end. One codec for the whole system, and a delegated call
//! that is not a wire of its own to be translated into this one.
//!
//! What it costs is that delegated calls are served one at a time; [`Progress`] has that,
//! and why it is latency rather than a deadlock.
//!
//! # Why a result rather than a stream
//!
//! An execution is one request and one answer: everything it wrote arrives at
//! once, in an [`ExecResult`], when it is over. There are no output chunks and no
//! way to watch a command work.
//!
//! That is a real capability given up, and it is given up on purpose. The caller
//! is an agent, and an agent cannot do anything with a partial answer — it does
//! not run its next inference until the command has finished, so output that
//! arrives early arrives to nobody. Streaming would buy latency that nothing
//! observes, at the price of a second shape for every ending, a second code path
//! in every consumer, and an interactive protocol whose only user is a machine
//! that cannot be interactive. It also has no JSON-RPC spelling: a request has one
//! response, and anything else would be notifications a peer has to reassemble.
//!
//! Two things follow, and both are consequences rather than accidents:
//!
//! - **Input goes with the request.** [`Exec::stdin`] is the whole of it, sent up
//!   front. An exchange — read the prompt, then answer it — is not expressible,
//!   which is the same trade in the other direction and would be incoherent to
//!   make differently.
//! - **A command that never ends produces nothing.** `tail -f` has no result to
//!   send, so [`timeout_ms`](Exec::timeout_ms) is what ends it. Without a timeout
//!   such an execution simply never answers, which is why one is worth setting.
//!
//! And output is now bounded by [`MAX_PAYLOAD`] rather than unbounded, which for
//! this caller is a feature: an agent cannot read 64 MiB either.
//!
//! # What is not text
//!
//! [`stdin`](Exec::stdin) and the output on an [`ExecResult`] are raw `Vec<u8>`,
//! because they are program bytes and nothing may touch them. A command's name and
//! arguments are required to be UTF-8: they have to become the `String`s an
//! [`Executable`](crate::executable::Executable) takes, so a name that could not be
//! one would have nowhere to go.
//!
//! In JSON those payloads are base64, because JSON has no byte type and would
//! otherwise write `[104,105,10]` — four characters per byte, for the bulk of what
//! this channel carries.

mod call;
mod message;
mod notification;
mod outcome;

pub use call::*;
pub use message::*;
pub use notification::*;
pub use outcome::*;
