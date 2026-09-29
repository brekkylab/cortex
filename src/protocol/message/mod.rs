//! The wire: JSON-RPC 2.0's object model, encoded as BSON.
//!
//! A session is a sequence of [`Message`]s: requests one way, their responses back. They
//! are real JSON-RPC objects, shown here as BSON Extended JSON:
//!
//! ```text
//! {"jsonrpc":"2.0","id":2,"method":"exec","params":{"cmd":["sh","-c","ls"]}}
//! {"jsonrpc":"2.0","id":2,"result":{"code":0,"stdout":<Binary>,"stderr":<Binary>,"truncated":false}}
//! {"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"timed out after 1000ms"}}
//! {"jsonrpc":"2.0","method":"quit"}
//! ```
//!
//! **The object model is the spec's; the encoding is not.** Member presence decides a
//! message's shape, `id` pairs a response with its request, and the error codes are
//! JSON-RPC 2.0's. The bytes are BSON documents, so a peer needs a BSON codec rather than
//! an off-the-shelf JSON-RPC library (see *Why the codec is BSON*).
//!
//! ```text
//! [u32 len][BSON document]   — a frame
//! ```
//!
//! Framing lives outside this module (see [`MAX_PAYLOAD`]). The header is redundant,
//! since a BSON document's first four bytes are its own length; [`stdio`](super::stdio)
//! says what retiring it would take.
//!
//! - `message` — the envelope: [`Message`] and its three shapes, the [`RequestId`] that
//!   pairs a response with its request, and their serde impls.
//! - `call` — [`Call`] and [`Response`], with one file per method holding its params and
//!   result types ([`InitCall`]/[`InitResp`], [`ExecCall`]/[`ExecResp`], ...). A method's
//!   answer is written in its call's vocabulary (an [`ImageSource`] asked for is an
//!   [`ImageSource`] confirmed), so the two halves stay in step.
//! - `notification` — [`Notification`], the three unanswered methods, and a member-less
//!   type apiece for what each carries.
//! - `method` — [`Method`], a method's wire name.
//! - `error` — [`Error`] and its codes.
//! - `utils` — `bytes` (how a byte payload reaches the wire) and `flatten` (how a value
//!   writes its members into an object someone else opened).
//!
//! [`Call`], [`Response`] and [`Notification`] each write their own `params`/`result`, so
//! adding a method does not touch the envelope.
//!
//! # What the protocol is
//!
//! | Method | `params` | `result` | Errors |
//! |---|---|---|---|
//! | `version` | [`VersionCall`] | [`VersionResp`] | — |
//! | `build_image` | [`BuildImageCall`] | [`BuildImageResp`] | [`INVALID_PARAMS`](Error::INVALID_PARAMS) |
//! | `remove_image` | [`RemoveImageCall`] | [`RemoveImageResp`] | [`INVALID_PARAMS`](Error::INVALID_PARAMS) |
//! | `list_images` | [`ListImagesCall`] | [`ListImagesResp`] | — |
//! | `init` | [`InitCall`] | [`InitResp`] | [`INVALID_PARAMS`](Error::INVALID_PARAMS), [`UNSUPPORTED_MOUNT`](Error::UNSUPPORTED_MOUNT), [`UNSUPPORTED_NETWORK`](Error::UNSUPPORTED_NETWORK), [`UNSUPPORTED_MACHINE`](Error::UNSUPPORTED_MACHINE) |
//! | `exec` | [`ExecCall`] | [`ExecResp`] | [`TIMED_OUT`](Error::TIMED_OUT), [`NOT_EXECUTABLE`](Error::NOT_EXECUTABLE), [`BOOT_FAILED`](Error::BOOT_FAILED), [`MOUNT_FAILED`](Error::MOUNT_FAILED) |
//! | `read` | [`ReadCall`] | [`ReadResp`] | [`NOT_FOUND`](Error::NOT_FOUND), [`IS_A_DIRECTORY`](Error::IS_A_DIRECTORY), [`IO_FAILED`](Error::IO_FAILED), [`BOOT_FAILED`](Error::BOOT_FAILED), [`MOUNT_FAILED`](Error::MOUNT_FAILED) |
//! | `write` | [`WriteCall`] | [`WriteResp`] | [`NOT_FOUND`](Error::NOT_FOUND), [`IS_A_DIRECTORY`](Error::IS_A_DIRECTORY), [`IO_FAILED`](Error::IO_FAILED), [`BOOT_FAILED`](Error::BOOT_FAILED), [`MOUNT_FAILED`](Error::MOUNT_FAILED) |
//! | `start` | — | *(notification — no response)* | — |
//! | `stop` | — | *(notification — no response)* | — |
//! | `quit` | — | *(notification — no response)* | — |
//!
//! Every request gets exactly one response, correlated by `id`; notifications have no
//! `id` and nothing answers them. **All methods are the client's:** a server never asks.
//!
//! # Why booting is not a method
//!
//! **Anything that needs a booted session boots one** (`exec`, `read`, `write`), so a
//! client that never sends `start` or `stop` still works, and `stop` followed by `exec`
//! still runs the `exec`. The pair is **resource management only**: it changes what the
//! far end holds and when it pays for it, never what a session can do.
//!
//! - **`stop` gives occupancy back.** A guest, a mounted tree and a scratch directory are
//!   memory, descriptors and disk, worth holding only while something runs. An idle
//!   client returns them for the price of one boot later.
//! - **`start` hides the cold start.** A backend with a kernel to boot would otherwise
//!   charge it to the first command; sent early, it overlaps with the client's other work.
//!
//! Neither is answered: the client's next step is the same either way, and a failed boot
//! is indistinguishable from one not yet attempted, since the next call that needs it
//! retries and reports [`BOOT_FAILED`](Error::BOOT_FAILED) to that caller.
//!
//! `init` is a call because it is not about resources: it says what the session *is*
//! (its trees and where each appears, what commands run in and may reach). Its response
//! is the one thing a client can act on before asking for work: a server is there, speaks
//! this protocol, and accepted the description.
//!
//! # Why the codec is BSON
//!
//! **Self-describing.** `{"method":..,"params":..}` is serde adjacent tagging, and
//! `result` xor `error` is decided by which member is *present*; both need a deserializer
//! that can look ahead, which postcard and bincode cannot. A `result` may also arrive
//! before the `method` that types it, so it is held as a [`Bson`](bson::Bson) value first.
//!
//! **A byte type.** Exec output and file contents are most of the traffic and are not
//! text. JSON would need base64 (1.37×, plus decoding); BSON's `Binary` carries them as
//! themselves.
//!
//! ## What that costs
//!
//! An off-the-shelf JSON-RPC library: a peer needs a BSON codec. That matters little,
//! since such a peer already needs bespoke framing (see [`MAX_PAYLOAD`]) and both ends
//! live in this workspace.
//!
//! Not frame size where it matters. BSON writes array indices as keys (`cmd` becomes
//! `{"0":"ls"}`) and names as C strings, so a control frame like `stop` is ~20 bytes
//! *larger* than JSON; the large, frequent output frames shrink. MessagePack and CBOR
//! beat BSON on both; BSON wins on being self-delimiting (which can retire the framing
//! layer) and on `doc!`/Extended JSON keeping the wire readable to people and tests.
//!
//! # Why errors carry codes
//!
//! `error` is the only failure channel, and a requester branches on its numeric `code`
//! and shows its `message`; there is no separate "timed out" message shape. An `error`
//! also cannot be mistaken for a command's exit status (e.g. 126/127, which any command
//! can `exit()` with), because it carries none.
//!
//! # Why an execution is one request and one response
//!
//! Nothing arrives between an `exec` and its answer, and the server never issues a
//! request, so neither end needs a pending table or a read loop that must not block, and
//! [`Client`](crate::console::Client) can take `&mut self` (one call outstanding, checked
//! by the borrow).
//!
//! The `result` is the [`ExecResp`] itself, unwrapped, so later additions are new
//! *members*, which older peers ignore; a tagged alternative would be a shape they fail on.
//!
//! # Why a result rather than a stream
//!
//! All output arrives at once when the command is over; there are no chunks. The caller
//! is an agent that cannot act on partial output before its next inference, so streaming
//! would buy unobserved latency at the price of a second shape for every ending and a
//! second code path in every consumer. JSON-RPC also has no spelling for it: a request
//! has one response.
//!
//! Consequences:
//!
//! - **An execution takes no input.** [`ExecCall`] carries no stdin; stage input with a
//!   [`WriteCall`] beforehand and collect output files with a [`ReadCall`].
//! - **A command that never ends produces nothing.** `tail -f` has no result, so set
//!   [`timeout_ms`](ExecCall::timeout_ms); without one it never answers.
//!
//! Output is bounded by [`MAX_PAYLOAD`], which suits an agent: it cannot read 64 MiB
//! either.
//!
//! # What is not text
//!
//! Exec output and file contents on [`ReadCall`]/[`WriteCall`] are raw `Vec<u8>` (BSON
//! `Binary`, subtype `Generic`), because they are program bytes. A command's name and
//! arguments must be UTF-8, since an argv is a list of `String`s by the time anything runs
//! it. A [`path`](ReadCall::path) is a `String` because both ends must interpret it, and
//! the executor maps it onto whatever filesystem it has.

mod call;
mod error;
mod message;
mod method;
mod notification;
mod utils;

pub use call::*;
pub use error::*;
pub use message::*;
pub use method::*;
pub use notification::*;

/// The version of this protocol, which `version` answers with.
pub const PROTOCOL_VERSION: &str = "1";
