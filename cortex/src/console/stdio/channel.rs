//! Framing: where one message ends and the next begins.
//!
//! ```text
//! [u32 len][serialized Message]
//! ```
//!
//! A stream of messages has to be cut apart somewhere. Length-prefixing is the
//! cheapest way to say where: the reader learns how much to expect before it reads
//! any of it, so there is no delimiter to search for and therefore none a payload
//! could forge. Big-endian, because that is what every other length on a wire is.
//!
//! # This header is now redundant, and kept anyway for the moment
//!
//! It was here because a serialized message did not know its own length. A BSON
//! document does: its first four bytes are an `int32`, little-endian, of the whole
//! document including those four. So the header duplicates something the payload
//! already carries, and a reader could take the document's own length instead —
//! which would also retire the one part of this protocol that no off-the-shelf peer
//! can guess.
//!
//! Not done yet because it is a wire change and the codec swap already was one. What
//! it takes when it happens: read [`HEADER`] bytes, read them as a little-endian
//! `u32`, check it against [`MAX_PAYLOAD`], and read that many *minus* the four
//! already in hand — the length includes itself, where this one does not. The refusal
//! of a zero-length frame becomes a refusal of anything under five, which is the
//! smallest a document can be (`\x05\x00\x00\x00\x00`, an empty one). The three
//! outcomes [`fill`] distinguishes do not change.
//!
//! # Why the transport is stdin and stdout, and nothing else
//!
//! A console server reads requests on **stdin** and writes responses on
//! **stdout**. Those two descriptors are the protocol's and carry nothing else.
//! **stderr** is free-form: logs, traces, whatever a person debugging wants, and
//! the only place any of it may go.
//!
//! That is the same discipline an MCP stdio server keeps, for the same reason —
//! stdout *is* the framing channel, so one stray `println!` corrupts the stream
//! rather than merely cluttering it.
//!
//! It is a rule and not something enforced here, which is worth saying plainly.
//! Nothing can hold stdout for the process in a way that would turn the mistake into
//! a hang: what an end holds is [`tokio::io::Stdout`], a handle that hands each write
//! to a blocking thread, and a `println!` from elsewhere goes to the same descriptor
//! without passing through it. Two frames of *ours* cannot interleave, because
//! whoever owns the writer serializes them; nothing else is caught by anything.
//!
//! *Which* descriptors an end takes is not this module's. Framing is the same over
//! a pipe, a virtio port or a `Vec<u8>`, whereas taking **the process's** stdin and
//! stdout is a process-wide claim that exactly one end makes, exactly once — so it
//! lives with that end, in [`StdioServer::stdio`](super::StdioServer::stdio).
//!
//! # The two directions are not paired here
//!
//! A frame is written with [`write`] and read with [`read`], each over one
//! descriptor. There is no type holding both, because a pipe pair *is* two
//! independent unidirectional streams and the only thing they share is the framing
//! — which is these two functions. Pairing them in a struct bought nothing and
//! cost two things: every holder carried a type parameter per direction whether it
//! used both or not, and the two halves could no longer be borrowed at once — which
//! is what an end wants when it writes a request and then reads for its answer.
//!
//! So an end keeps whichever halves it has, as its own fields. A requesting end has
//! a child's stdout and stdin; an answering end has its own; something that only
//! reports may hold one.
//!
//! Nothing about a command's own stdio appears here. It is captured wherever the
//! command runs and travels back inside an
//! [`ExecResult`](super::ExecResult) — which is what makes one pair of
//! descriptors enough, where the old wire needed four.
//!
//! # Neither of these is cancel-safe
//!
//! Both take a `&mut` to a descriptor and leave it part way through a frame if the
//! future is dropped mid-await — a header written with no payload behind it, or a
//! payload half consumed. There is no way to resume from that and no way to tell
//! from the descriptor that it happened, so a peer on the other side of one is
//! reading garbage from then on.
//!
//! Which is why nothing here is put in a [`select!`](tokio::select) branch. An end
//! that has to wait on something else as well waits on it *around* a frame and not
//! during one — see [`StdioClient::call`](super::StdioClient::call), which owns its
//! descriptors for the whole of a round trip.
//!
//! # Why buffering is fine
//!
//! Nothing is ever handed over: the descriptors belong to the protocol for the life
//! of the process, so a byte read early is never a byte a command needed. Both ends
//! therefore wrap what they read in a [`BufReader`](tokio::io::BufReader) once, at
//! construction, which is what keeps a header and its payload from being two
//! syscalls every time.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::console::{MAX_PAYLOAD, Message};

/// The length prefix: a big-endian `u32`.
pub const HEADER: usize = 4;

/// Write one frame and flush it.
///
/// Header and payload go out in two calls rather than one, which avoids copying a
/// payload that may be megabytes only to write it once. That costs one rule: a
/// descriptor has a single writer at a time. A writer that ends up in several
/// places wants to be *shared* rather than cloned — two owners would interleave a
/// header with somebody else's payload, where one behind a lock cannot.
pub async fn write<W>(w: &mut W, message: &Message) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let payload =
        bson::serialize_to_vec(message).map_err(|e| bad(format!("serializing a message: {e}")))?;
    if payload.len() > MAX_PAYLOAD {
        return Err(oversized(payload.len(), "to send"));
    }

    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await
}

/// Read one frame. `Ok(None)` is a clean end of channel.
///
/// The length is read first and then exactly that many bytes, so a frame cannot
/// run into the one after it however the payload is spelled.
pub async fn read<R>(r: &mut R) -> io::Result<Option<Message>>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut header = [0u8; HEADER];
    if !fill(r, &mut header).await? {
        return Ok(None);
    }

    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_PAYLOAD {
        return Err(oversized(len, "to accept"));
    }
    // No message serializes to nothing, so a zero-length frame is a peer that
    // has lost its place — better said now than as a parse error.
    if len == 0 {
        return Err(bad("a frame of no bytes cannot be a message".to_string()));
    }

    let mut payload = vec![0u8; len];
    if !fill(r, &mut payload).await? {
        return Err(truncated());
    }

    bson::deserialize_from_slice(&payload)
        .map(Some)
        .map_err(|e| bad(format!("reading a message: {e}")))
}

/// Fill `buf` completely. `Ok(false)` means a clean end of stream *before any
/// byte arrived* — the peer closed between frames, which is how a channel ends.
/// Stopping part way through is corruption, not an ending.
///
/// Written out rather than [`read_exact`](AsyncReadExt::read_exact), which cannot
/// tell those two apart: it reports both as `UnexpectedEof`.
async fn fill<R>(r: &mut R, buf: &mut [u8]) -> io::Result<bool>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]).await? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(truncated()),
            n => filled += n,
        }
    }
    Ok(true)
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "a frame ended mid-way")
}

fn oversized(len: usize, doing: &str) -> io::Error {
    bad(format!(
        "a frame of {len} bytes is more than the {MAX_PAYLOAD} this protocol agrees {doing}"
    ))
}

fn bad(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{Call, Exec, ExecCmd, ExecResult, Init, Notification, Outcome, Progress};

    fn messages() -> Vec<Message> {
        vec![
            Message::Request {
                id: 0,
                call: Call::Init(Init {
                    delegated: vec!["foo".into()],
                }),
            },
            Message::Response {
                id: 0,
                outcome: Outcome::Result(bson::Bson::Null),
            },
            Message::Request {
                id: 2,
                call: Call::Exec(Exec {
                    cmd: ExecCmd::New(vec!["sh".into(), "-c".into(), "echo hi".into()]),
                    ..Exec::default()
                }),
            },
            Message::Response {
                id: 2,
                outcome: Outcome::Result(
                    bson::serialize_to_bson(&Progress::Done(ExecResult {
                        // Not utf-8, and contains the byte a newline-delimited
                        // framing would have had to escape.
                        stdout: vec![0xff, 0x00, b'\n'],
                        ..ExecResult::default()
                    }))
                    .unwrap(),
                ),
            },
            Message::Notification(Notification::Quit),
        ]
    }

    #[tokio::test]
    async fn a_message_survives_a_frame() {
        for message in messages() {
            let mut buf = Vec::new();
            write(&mut buf, &message).await.unwrap();
            assert_eq!(read(&mut buf.as_slice()).await.unwrap().unwrap(), message);
        }
    }

    /// A frame is its length and then exactly that many bytes of payload —
    /// nothing else, and no delimiter.
    #[tokio::test]
    async fn a_frame_is_a_length_and_a_payload() {
        let message = Message::Notification(Notification::Quit);
        let mut buf = Vec::new();
        write(&mut buf, &message).await.unwrap();

        let payload = bson::serialize_to_vec(&message).unwrap();
        assert_eq!(&buf[..HEADER], &(payload.len() as u32).to_be_bytes());
        assert_eq!(&buf[HEADER..], &payload[..]);
        assert_eq!(buf.len(), HEADER + payload.len());

        // And the payload says its own length too, which is why the header above is
        // redundant — see the module docs.
        assert_eq!(
            u32::from_le_bytes(payload[..HEADER].try_into().unwrap()) as usize,
            payload.len(),
        );
    }

    /// Several frames back to back come out in order, which is what the length
    /// prefix is for: nothing has to be scanned to find the boundaries.
    #[tokio::test]
    async fn frames_stream_back_in_order() {
        let sent = messages();
        let mut buf = Vec::new();
        for message in &sent {
            write(&mut buf, message).await.unwrap();
        }

        let mut reader = buf.as_slice();
        let mut read_back = Vec::new();
        while let Some(message) = read(&mut reader).await.unwrap() {
            read_back.push(message);
        }
        assert_eq!(read_back, sent);
    }

    /// A payload byte is never mistaken for structure — the whole reason not to
    /// delimit. Every byte value goes through an output payload untouched.
    #[tokio::test]
    async fn no_payload_byte_is_special() {
        let message = Message::Response {
            id: 2,
            outcome: Outcome::Result(
                bson::serialize_to_bson(&Progress::Done(ExecResult {
                    stdout: (0..=255u8).collect(),
                    ..ExecResult::default()
                }))
                .unwrap(),
            ),
        };
        let mut buf = Vec::new();
        write(&mut buf, &message).await.unwrap();
        assert_eq!(read(&mut buf.as_slice()).await.unwrap().unwrap(), message);
    }

    #[tokio::test]
    async fn a_clean_close_ends_the_channel_but_a_partial_frame_does_not() {
        assert!(read(&mut [].as_slice()).await.unwrap().is_none());

        let mut buf = Vec::new();
        write(&mut buf, &Message::Notification(Notification::Quit))
            .await
            .unwrap();
        buf.truncate(buf.len() - 1);
        assert_eq!(
            read(&mut buf.as_slice()).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        // A header that stops part way is the same kind of ending: something
        // arrived, so the peer did not close between frames.
        assert_eq!(
            read(&mut [0u8, 0].as_slice()).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn a_length_that_could_only_be_wrong_is_refused_rather_than_allocated() {
        let mut absurd = u32::MAX.to_be_bytes().to_vec();
        absurd.extend_from_slice(b"{}");
        let error = read(&mut absurd.as_slice()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("more than"), "{error}");

        // And nothing serializes to nothing.
        let error = read(&mut [0u8; HEADER].as_slice()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("no bytes"), "{error}");
    }

    /// A well-framed payload that is not a message is a data error, not an
    /// ending: the framing worked and the contents did not.
    #[tokio::test]
    async fn a_framed_non_message_is_refused() {
        let payload =
            bson::serialize_to_vec(&bson::doc! {"jsonrpc": "1.0", "method": "quit"}).unwrap();
        let mut buf = (payload.len() as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(&payload);
        let error = read(&mut buf.as_slice()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("reading a message"), "{error}");
    }

    /// The two directions are independent, so what one descriptor was written with
    /// reads back off another with nothing pairing them.
    #[tokio::test]
    async fn the_two_directions_are_independent() {
        let sent = messages();

        let mut outgoing = Vec::new();
        for message in &sent {
            write(&mut outgoing, message).await.unwrap();
        }

        // A reader that was never written to is a clean close, not an error.
        assert!(read(&mut tokio::io::empty()).await.unwrap().is_none());

        let mut incoming = outgoing.as_slice();
        for message in &sent {
            assert_eq!(read(&mut incoming).await.unwrap().as_ref(), Some(message));
        }
        assert!(read(&mut incoming).await.unwrap().is_none());
    }
}
