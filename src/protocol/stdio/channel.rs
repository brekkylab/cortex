//! Framing: where one message ends and the next begins.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::{MAX_PAYLOAD, Message};

/// The length prefix: a big-endian `u32`, as wire lengths conventionally are.
///
/// Redundant: a BSON document already starts with its own length (little-endian `int32`,
/// including those four bytes), so a reader could use that and drop the one bespoke layer
/// no off-the-shelf peer can guess. Kept because dropping it is a wire change, which
/// means: read `HEADER` bytes as a little-endian `u32`, check it against [`MAX_PAYLOAD`],
/// read that many *minus* the four already in hand, and refuse anything under five (an
/// empty document, `\x05\x00\x00\x00\x00`) instead of zero. What [`read`](fn@read) tells apart
/// stays the same.
pub const HEADER: usize = 4;

/// Write one frame and flush it.
///
/// Header and payload are two writes, to avoid copying a possibly multi-megabyte
/// payload. So a descriptor needs a single writer: share one behind a lock rather than
/// cloning it, or a header can interleave with another frame's payload.
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
/// Wrap `r` in a [`BufReader`](tokio::io::BufReader) once, so a header and payload are
/// not two syscalls.
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
    // No message serializes to nothing: the peer has lost its place.
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

/// Fill `buf` completely. `Ok(false)` is a clean close *before any byte* (between
/// frames); stopping part way is an error.
///
/// Not [`read_exact`](AsyncReadExt::read_exact), which reports both as `UnexpectedEof`.
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
    use crate::protocol::{Call, ExecCall, ExecResp, InitCall, InitResp, Notification, Response};

    fn messages() -> Vec<Message> {
        vec![
            Message::Request {
                id: 0,
                call: Call::Init(InitCall::default()),
            },
            Message::Response {
                id: 0,
                result: Response::Init(InitResp::default()),
            },
            Message::Request {
                id: 2,
                call: Call::Exec(ExecCall {
                    cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
                    ..ExecCall::default()
                }),
            },
            Message::Response {
                id: 2,
                result: Response::Exec(ExecResp {
                    // Not UTF-8, and includes a byte newline framing would escape.
                    stdout: vec![0xff, 0x00, b'\n'],
                    ..ExecResp::default()
                }),
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

    /// A frame is its length and exactly that many payload bytes, with no delimiter.
    #[tokio::test]
    async fn a_frame_is_a_length_and_a_payload() {
        let message = Message::Notification(Notification::Quit);
        let mut buf = Vec::new();
        write(&mut buf, &message).await.unwrap();

        let payload = bson::serialize_to_vec(&message).unwrap();
        assert_eq!(&buf[..HEADER], &(payload.len() as u32).to_be_bytes());
        assert_eq!(&buf[HEADER..], &payload[..]);
        assert_eq!(buf.len(), HEADER + payload.len());

        // The payload also carries its own length, making the header redundant.
        assert_eq!(
            u32::from_le_bytes(payload[..HEADER].try_into().unwrap()) as usize,
            payload.len(),
        );
    }

    /// Back-to-back frames read back in order.
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

    /// Every byte value passes through a payload untouched.
    #[tokio::test]
    async fn no_payload_byte_is_special() {
        let message = Message::Response {
            id: 2,
            result: Response::Exec(ExecResp {
                stdout: (0..=255u8).collect(),
                ..ExecResp::default()
            }),
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

        // A partial header is also truncation, not a clean close.
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

        // A zero-length frame is refused too.
        let error = read(&mut [0u8; HEADER].as_slice()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("no bytes"), "{error}");
    }

    /// A well-framed payload that is not a message is a data error, not an ending.
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

    /// Frames written to one descriptor read back from another, with nothing pairing them.
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
