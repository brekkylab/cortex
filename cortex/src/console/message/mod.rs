//! The wire: JSON-RPC 2.0's object model, encoded as BSON.
//!
//! One channel carries a whole session as a sequence of [`Message`]s — requests one
//! way, the responses answering them back — and they are real JSON-RPC objects, not a
//! shape that resembles them. Written the way BSON's own Extended JSON would show
//! them, so that the members are readable:
//!
//! ```text
//! {"jsonrpc":"2.0","id":2,"method":"exec","params":{"cmd":["sh","-c","ls"]}}
//! {"jsonrpc":"2.0","id":2,"result":{"code":0,"stdout":<Binary>,"stderr":<Binary>,"truncated":false}}
//! {"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"timed out after 1000ms"}}
//! {"jsonrpc":"2.0","method":"quit"}
//! ```
//!
//! **The object model is the spec's; the encoding is not.** Which members are present
//! is what decides a message's shape, the `id` pairs a response with its request, and
//! the error codes are the spec's — all of that is JSON-RPC 2.0 and reads as it. But
//! the bytes are BSON documents, so a peer cannot hand a frame to an off-the-shelf
//! JSON-RPC library and have it parse: it needs a BSON codec, and then the semantics
//! above are ordinary. That was a deliberate trade — see *Why the codec is BSON*.
//!
//! ```text
//! [u32 len][BSON document]   — a frame
//! ```
//!
//! Framing is separate on purpose and lives outside this module — see [`MAX_PAYLOAD`].
//! It is also now redundant, because a BSON document's first four bytes are its own
//! length; [`stdio`](super::stdio) has what retiring the header would take.
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
//! # Why the codec is BSON
//!
//! Two requirements, and BSON is what meets both.
//!
//! **Self-describing.** JSON-RPC's own structure demands it. `{"method":.., "params":..}`
//! is serde's adjacent tagging and `result` xor `error` is decided by which field is
//! *present* — both need a deserializer that can look ahead, which non-self-describing
//! codecs like postcard and bincode cannot do. A response's `result` type also depends
//! on the method its `id` was issued for, so reading one means holding it as a value
//! until the pending request identifies it, which is what [`Outcome`] is.
//!
//! **A byte type.** [`stdin`](Exec::stdin) and an [`ExecResult`]'s output are the bulk
//! of what this channel carries and they are not text. JSON has no way to say so, which
//! left base64 — 1.37×, and a spelling that has to be decoded before it is bytes again.
//! BSON has `Binary`, so they travel as themselves.
//!
//! ## What that costs, and what it does not
//!
//! It costs the off-the-shelf JSON-RPC library. That was the reason for JSON, and it is
//! a real capability given up: a peer now needs a BSON codec. It is worth less than it
//! looks, because such a peer already needed bespoke framing (see [`MAX_PAYLOAD`]) and
//! because both ends of this channel are in this workspace today.
//!
//! It does **not** cost frame size in the direction that matters. BSON is not a compact
//! format — it writes array indices as keys (`cmd` becomes `{"0":"ls"}`) and every name
//! as a C string — so a control frame like `stop` is *larger* than its JSON spelling, by
//! about 20 bytes. What shrinks is the frames that carry a command's output, which are
//! the large and frequent ones. MessagePack and CBOR would beat BSON on both, and were
//! the obvious alternatives; BSON won on being self-delimiting, which retires this
//! protocol's one bespoke layer, and on `doc!`/Extended JSON keeping the wire readable
//! to a person and to the tests in here.
//!
//! ## Why the `bytes` helper does not ask the codec
//!
//! It used to, via [`is_human_readable`](serde::Serializer::is_human_readable), so that
//! one helper served a textual codec and a binary one. That branch is gone, and
//! [`call`](self::call)'s `bytes` module has the reason: `params` and `result` pass
//! through a `Bson` value before they reach the wire, and `bson`'s value-level
//! serializer reports itself human-readable, so the branch would quietly restore base64
//! at the one place the byte type was the point.
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
//! On the wire those payloads are BSON `Binary`, subtype `Generic` — themselves, at
//! 1.0×. Getting that is why the codec is BSON and not JSON, which has no byte type and
//! would have made them base64 at best (1.37×) or `[104,105,10]` at worst (4×).

mod call;
mod message;
mod notification;
mod outcome;

pub use call::*;
pub use message::*;
pub use notification::*;
pub use outcome::*;
