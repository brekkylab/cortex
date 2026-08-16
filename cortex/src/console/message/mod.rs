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
//! The split inside here is the envelope, the methods, and the two things that are
//! neither:
//!
//! - `message` — the envelope every object shares: [`Message`] and its three
//!   shapes, the [`RequestId`] that pairs a response with its request, and the serde
//!   impls that put all of it on the wire.
//! - `method` — the methods and what each of them carries, one file apiece ([`Init`],
//!   [`Exec`] and [`Progress`], the file plane's [`Read`] and [`Write`], the three that
//!   carry nothing). [`Method`] names them; [`Call`] is the four that are answered and
//!   [`Notification`] the three that are not.
//! - `outcome` — how something ended: [`Outcome`], and the [`Error`] and codes that
//!   are the second half of it.
//!
//! A `Call` and a `Notification` each write and read their own `params`, so adding
//! a method means touching the side it belongs to and not the envelope. Which of the
//! two a method is, is the whole of what JSON-RPC's `id` decides, and it is why those
//! are the types the envelope names — while *what* each method carries is one file of
//! its own, so that changing a method is one place to look.
//!
//! # What the protocol is
//!
//! | Method | `params` | `result` | Errors |
//! |---|---|---|---|
//! | `init` | [`Init`] | [`InitResult`] | [`INVALID_PARAMS`](Error::INVALID_PARAMS), [`UNSUPPORTED_WORKFS`](Error::UNSUPPORTED_WORKFS) |
//! | `exec` | [`Exec`] | [`Progress`] | [`TIMED_OUT`](Error::TIMED_OUT), [`NOT_EXECUTABLE`](Error::NOT_EXECUTABLE), [`BOOT_FAILED`](Error::BOOT_FAILED), [`MOUNT_FAILED`](Error::MOUNT_FAILED) |
//! | `read` | [`Read`] | [`ReadResult`] | [`NOT_FOUND`](Error::NOT_FOUND), [`IS_A_DIRECTORY`](Error::IS_A_DIRECTORY), [`IO_FAILED`](Error::IO_FAILED), [`BOOT_FAILED`](Error::BOOT_FAILED), [`MOUNT_FAILED`](Error::MOUNT_FAILED) |
//! | `write` | [`Write`] | [`WriteResult`] | [`NOT_FOUND`](Error::NOT_FOUND), [`IS_A_DIRECTORY`](Error::IS_A_DIRECTORY), [`IO_FAILED`](Error::IO_FAILED), [`BOOT_FAILED`](Error::BOOT_FAILED), [`MOUNT_FAILED`](Error::MOUNT_FAILED) |
//! | `start` | — | *(notification — no response)* | — |
//! | `stop` | — | *(notification — no response)* | — |
//! | `quit` | — | *(notification — no response)* | — |
//!
//! Every request gets exactly one response, correlated by `id`; the three notifications
//! have no `id` and nothing answers them. **Every one of them is the client's:** there
//! is no method a server issues, which is what [`Progress`] is for.
//!
//! # Why booting is not a method
//!
//! **Anything that needs a booted session boots one** — an `exec`, a `read`, a `write` —
//! so a client that sends neither `start` nor `stop` still works, and one that sends
//! `stop` and then an `exec` gets the `exec`.
//!
//! That leaves the pair as **the protocol's resource management, and nothing else**.
//! Neither changes what a session can do; both change what the far end is holding, and
//! when it paid to hold it:
//!
//! - **`stop` gives occupancy back.** What booting took — a guest, a socket, a scratch
//!   directory — is memory, descriptors and disk on the far end, and it is only worth
//!   anything while something is running. A client that knows it will be idle hands them
//!   back and takes them again for the price of one boot when it has work.
//! - **`start` hides the cold start.** A backend with a kernel to bring up makes the
//!   first command pay for that inside its own latency. A client that says this as soon
//!   as it has a console pays for it in parallel with whatever else it is doing, and the
//!   command that follows finds a session already up.
//!
//! Neither is a question, which is why neither is answered: what a client does next is
//! the same either way, and a session that failed to boot behaves like one that has not
//! booted yet, since the next call that needs one tries again. A failure reaches whoever
//! asked for that call, as [`BOOT_FAILED`](Error::BOOT_FAILED).
//!
//! `init` is the exception and is a call, because it is not about resources. It says what
//! the session *is* — the delegated names, and the tree it works in — and its response is
//! the one thing a client can act on before it has asked for any work: that there is a
//! server on the far end, that it speaks this protocol, that it has taken what it was told,
//! and where it will put the tree. That last part is why the answer is read rather than
//! merely awaited: every path in the session afterwards is spelled under it.
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
//! **A byte type.** An [`ExecResult`]'s output and a file's contents are the bulk of
//! what this channel carries and neither is text. JSON has no way to say so, which
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
//! [`is_human_readable`](serde::Serializer::is_human_readable) is the obvious way for
//! one helper to serve a textual codec and a binary one, and `method`'s `bytes` module
//! has why it cannot be used: `params` and `result` pass through a `Bson` value before
//! they reach the wire, and `bson`'s value-level serializer reports itself
//! human-readable, so the branch would quietly restore base64 at the one place the byte
//! type was the point.
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
//! client runs the name and sends another `exec` carrying what it got as an
//! [`ExecCmd::Resume`], whose answer is the next `Progress`.
//!
//! Carrying on is a shape of that method's `cmd` and not a `resume` method of its own,
//! because the two would be the same request under two names: an execution the server is
//! holding, and the output it was waiting for. What the client has to say is *this is where
//! the last one got to*, which is what an `exec` is for — and one method fewer is one fewer
//! place for the two ends to disagree about which of them a response is answering.
//!
//! So one channel, one end that asks, one end that answers — and one [`Exec`] type, since
//! a delegated call is the same shape as any other execution request: a command,
//! output, a code at the end. One codec for the whole system, and a delegated call
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
//! - **An execution takes no input.** An exchange — read the prompt, then answer it —
//!   is not expressible, which is the same trade in the other direction and would be
//!   incoherent to make differently. An [`Exec`] carries none, so what a command is to
//!   read goes where it will find it with a [`Write`] beforehand, and what it leaves
//!   behind comes back with a [`Read`].
//! - **A command that never ends produces nothing.** `tail -f` has no result to
//!   send, so [`timeout_ms`](Exec::timeout_ms) is what ends it. Without a timeout
//!   such an execution simply never answers, which is why one is worth setting.
//!
//! And output is now bounded by [`MAX_PAYLOAD`] rather than unbounded, which for
//! this caller is a feature: an agent cannot read 64 MiB either.
//!
//! # What is not text
//!
//! The output on an [`ExecResult`] and the file contents on a [`Read`] or a [`Write`]
//! are raw `Vec<u8>`, because those are program bytes and nothing may touch them. A
//! command's name and arguments are required to be UTF-8: they have to become the
//! `String`s an [`Executable`](crate::executable::Executable) takes, so a name that
//! could not be one would have nowhere to go. A [`path`](Read::path) is a `String` for
//! the practical version of the same reason — the executor turns it into a path for
//! whatever filesystem it has, and it is the one member of a file call that both ends
//! have to read rather than carry.
//!
//! On the wire those payloads are BSON `Binary`, subtype `Generic` — themselves, at
//! 1.0×. Getting that is why the codec is BSON and not JSON, which has no byte type and
//! would have made them base64 at best (1.37×) or `[104,105,10]` at worst (4×).

mod message;
mod method;
mod outcome;

pub use message::*;
pub use method::*;
pub use outcome::*;
