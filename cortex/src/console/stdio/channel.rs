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
//! Holding [`StdoutLock`](io::StdoutLock) for the process would turn the mistake
//! into a hang, and a hang can be found — but that guard is not [`Send`], and a
//! writer has to be movable to whatever thread writes it. So an end holds the
//! [`Stdout`](io::Stdout) handle instead, which locks per write. Two frames of ours
//! cannot interleave — whoever owns the writer serializes them — and a `println!`
//! from elsewhere is not caught by anything.
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
//! # Why buffering is fine now
//!
//! The old wire could not buffer its reads of fd 0: the command's stdin followed
//! the command on the same descriptor, so a byte read early was a byte the
//! command never saw. Nothing is handed over here — the descriptors belong to the
//! protocol for the life of the process — so a buffered reader is not merely
//! allowed but wanted. The [`Stdin`](io::Stdin) handle is buffered already, which is
//! why an end that takes it needs no wrapper of its own.

use std::io::{self, Read, Write};

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
pub fn write(w: &mut impl Write, message: &Message) -> io::Result<()> {
    let payload =
        bson::serialize_to_vec(message).map_err(|e| bad(format!("serializing a message: {e}")))?;
    if payload.len() > MAX_PAYLOAD {
        return Err(oversized(payload.len(), "to send"));
    }

    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read one frame. `Ok(None)` is a clean end of channel.
///
/// The length is read first and then exactly that many bytes, so a frame cannot
/// run into the one after it however the payload is spelled.
pub fn read(r: &mut impl Read) -> io::Result<Option<Message>> {
    let mut header = [0u8; HEADER];
    if !fill(r, &mut header)? {
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
    if !fill(r, &mut payload)? {
        return Err(truncated());
    }

    bson::deserialize_from_slice(&payload)
        .map(Some)
        .map_err(|e| bad(format!("reading a message: {e}")))
}

/// Fill `buf` completely. `Ok(false)` means a clean end of stream *before any
/// byte arrived* — the peer closed between frames, which is how a channel ends.
/// Stopping part way through is corruption, not an ending.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
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
    use crate::console::{Call, Exec, ExecResult, Notification, Outcome, Progress, Start};

    fn messages() -> Vec<Message> {
        vec![
            Message::Request {
                id: 0,
                call: Call::Start(Start {
                    delegated: vec!["foo".into()],
                    default_timeout_ms: Some(30_000),
                }),
            },
            Message::Response {
                id: 0,
                outcome: Outcome::Result(bson::Bson::Null),
            },
            Message::Request {
                id: 2,
                call: Call::Exec(Exec {
                    cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
                    timeout_ms: None,
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

    #[test]
    fn a_message_survives_a_frame() {
        for message in messages() {
            let mut buf = Vec::new();
            write(&mut buf, &message).unwrap();
            assert_eq!(read(&mut buf.as_slice()).unwrap().unwrap(), message);
        }
    }

    /// A frame is its length and then exactly that many bytes of payload —
    /// nothing else, and no delimiter.
    #[test]
    fn a_frame_is_a_length_and_a_payload() {
        let message = Message::Notification(Notification::Quit);
        let mut buf = Vec::new();
        write(&mut buf, &message).unwrap();

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
    #[test]
    fn frames_stream_back_in_order() {
        let sent = messages();
        let mut buf = Vec::new();
        for message in &sent {
            write(&mut buf, message).unwrap();
        }

        let mut reader = buf.as_slice();
        let mut read_back = Vec::new();
        while let Some(message) = read(&mut reader).unwrap() {
            read_back.push(message);
        }
        assert_eq!(read_back, sent);
    }

    /// A payload byte is never mistaken for structure — the whole reason not to
    /// delimit. Every byte value goes through an output payload untouched.
    #[test]
    fn no_payload_byte_is_special() {
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
        write(&mut buf, &message).unwrap();
        assert_eq!(read(&mut buf.as_slice()).unwrap().unwrap(), message);
    }

    #[test]
    fn a_clean_close_ends_the_channel_but_a_partial_frame_does_not() {
        assert!(read(&mut [].as_slice()).unwrap().is_none());

        let mut buf = Vec::new();
        write(&mut buf, &Message::Notification(Notification::Quit)).unwrap();
        buf.truncate(buf.len() - 1);
        assert_eq!(
            read(&mut buf.as_slice()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        // A header that stops part way is the same kind of ending: something
        // arrived, so the peer did not close between frames.
        assert_eq!(
            read(&mut [0u8, 0].as_slice()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_length_that_could_only_be_wrong_is_refused_rather_than_allocated() {
        let mut absurd = u32::MAX.to_be_bytes().to_vec();
        absurd.extend_from_slice(b"{}");
        let error = read(&mut absurd.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("more than"), "{error}");

        // And nothing serializes to nothing.
        let error = read(&mut [0u8; HEADER].as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("no bytes"), "{error}");
    }

    /// A well-framed payload that is not a message is a data error, not an
    /// ending: the framing worked and the contents did not.
    #[test]
    fn a_framed_non_message_is_refused() {
        let payload =
            bson::serialize_to_vec(&bson::doc! {"jsonrpc": "1.0", "method": "quit"}).unwrap();
        let mut buf = (payload.len() as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(&payload);
        let error = read(&mut buf.as_slice()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("reading a message"), "{error}");
    }

    /// The two directions are independent, so what one descriptor was written with
    /// reads back off another with nothing pairing them.
    #[test]
    fn the_two_directions_are_independent() {
        let sent = messages();

        let mut outgoing = Vec::new();
        for message in &sent {
            write(&mut outgoing, message).unwrap();
        }

        // A reader that was never written to is a clean close, not an error.
        assert!(read(&mut io::empty()).unwrap().is_none());

        let mut incoming = outgoing.as_slice();
        for message in &sent {
            assert_eq!(read(&mut incoming).unwrap().as_ref(), Some(message));
        }
        assert!(read(&mut incoming).unwrap().is_none());
    }
}
