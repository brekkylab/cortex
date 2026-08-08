//! The answering end, over a framed channel: bring a message in, put a response out.
//!
//! That is the whole of this file. Nothing here decides what a request *means* — what a
//! session allows, where a command runs, what counts as booted. A
//! [`Server`] moves frames, and whoever answers them does so somewhere else.
//!
//! ```no_run
//! use cortex::console::stdio::StdioServer;
//! use cortex::console::{Message, Outcome, Server};
//!
//! # fn answer(call: cortex::console::Call) -> Outcome { unimplemented!() }
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! // Takes stdin and stdout for the protocol; everything else goes to stderr.
//! let mut server = StdioServer::stdio()?;
//!
//! while let Some(message) = server.recv().await? {
//!     if let Message::Request { id, call } = message {
//!         server.respond(id, answer(call)).await?;
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_core::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

use crate::console::stdio::{read, write};
use crate::console::{Message, Outcome, RequestId, Server};

/// Whether this process has already taken its standard descriptors.
///
/// There is one stdin and one stdout, so a second [`StdioServer::stdio`] would be a
/// second owner of both and the two would interleave frames. Refusing there is what
/// lets the first one assume it is alone.
///
/// A process-wide claim, so it is a process-wide flag — and it belongs here rather than
/// with the framing, because this is the end that makes it. A requesting end reads and
/// writes a *child's* pipes and never touches its own.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// A [`Server`] over one readable and one writable descriptor.
///
/// The two directions are separate fields and not a pair, because that is what they
/// are: a frame going out has nothing to do with the one coming in beyond the framing
/// they share.
///
/// Trait objects rather than type parameters. Which descriptors these are is not
/// something this end answers differently, so a pair of parameters would only put the
/// answer in every signature that mentions one.
///
/// No state beyond the two descriptors. There is no session here to keep.
pub struct StdioServer {
    /// Where requests come from.
    ///
    /// Buffered here, once. Nothing hands this to a command — it is the protocol's for
    /// the life of the session — so reading ahead cannot take a byte that was somebody
    /// else's.
    incoming: BufReader<Box<dyn AsyncRead + Send + Unpin>>,

    /// Where responses go.
    outgoing: Box<dyn AsyncWrite + Send + Unpin>,
}

impl StdioServer {
    /// Take the two descriptors, whatever they are — this process's stdin and stdout, the
    /// halves of a socket, a `Cursor` and a `Vec` for a test.
    pub fn new(
        incoming: impl AsyncRead + Send + Unpin + 'static,
        outgoing: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        StdioServer {
            incoming: BufReader::new(Box::new(incoming)),
            outgoing: Box::new(outgoing),
        }
    }

    /// Take stdin and stdout for the protocol, for the life of the process.
    ///
    /// From here on **stdout carries frames and nothing else**. That is a rule rather
    /// than something checked — see [`read()`] and [`write()`] for why it cannot be — so
    /// diagnostics go to stderr.
    ///
    /// Fails if called twice: there is one stdin and one stdout, so a second of these
    /// would be a second owner of both and the two would interleave frames.
    pub fn stdio() -> anyhow::Result<Self> {
        if TAKEN.swap(true, Ordering::SeqCst) {
            anyhow::bail!("stdin and stdout are already the protocol's — there is one of each");
        }
        Ok(StdioServer::new(tokio::io::stdin(), tokio::io::stdout()))
    }
}

impl Server for StdioServer {
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<Message>>> {
        Box::pin(read(&mut self.incoming))
    }

    fn respond(&mut self, id: RequestId, outcome: Outcome) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { write(&mut self.outgoing, &Message::Response { id, outcome }).await })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll};

    use super::*;
    use crate::console::{Call, Error, Exec, ExecCmd, Init, Notification};

    /// Everything this end wrote, readable after it has been dropped or not — a `Vec`
    /// cannot be, once the server owns it.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl AsyncWrite for Sent {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn framed(messages: &[Message]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for message in messages {
            write(&mut bytes, message).await.unwrap();
        }
        bytes
    }

    fn request(id: RequestId, call: Call) -> Message {
        Message::Request { id, call }
    }

    /// Everything arrives, in order, and nothing is read into: a notification and a
    /// response are handed over exactly as they came, for a caller to make of what it
    /// will.
    #[tokio::test]
    async fn every_message_arrives_as_it_was_sent() {
        let sent = vec![
            request(0, Call::Init(Init::default())),
            Message::Notification(Notification::Start),
            request(
                1,
                Call::Exec(Exec {
                    cmd: ExecCmd::New(vec!["echo".into(), "hi".into()]),
                    ..Exec::default()
                }),
            ),
            Message::Notification(Notification::Stop),
            // Not a request, and this end has no opinion about that.
            Message::Response {
                id: 99,
                outcome: Outcome::Result(bson::Bson::Null),
            },
            Message::Notification(Notification::Quit),
        ];

        let mut server = StdioServer::new(Cursor::new(framed(&sent).await), Sent::default());
        for message in &sent {
            assert_eq!(server.recv().await.unwrap().as_ref(), Some(message));
        }
        // And the end of the channel is a clean end, not an error.
        assert!(server.recv().await.unwrap().is_none());
    }

    /// A response goes out framed, carrying the id it was given and nothing else.
    #[tokio::test]
    async fn a_response_carries_the_id_it_was_given() {
        let sent = Sent::default();
        let mut server = StdioServer::new(tokio::io::empty(), sent.clone());

        server
            .respond(7, Outcome::Result(bson::Bson::Null))
            .await
            .unwrap();
        server
            .respond(9, Outcome::Error(Error::new(Error::TIMED_OUT, "too slow")))
            .await
            .unwrap();

        let bytes = sent.0.lock().unwrap().clone();
        let mut reader = bytes.as_slice();
        let mut written = Vec::new();
        while let Some(message) = read(&mut reader).await.unwrap() {
            written.push(message);
        }

        assert_eq!(
            written,
            [
                Message::Response {
                    id: 7,
                    outcome: Outcome::Result(bson::Bson::Null),
                },
                Message::Response {
                    id: 9,
                    outcome: Outcome::Error(Error::new(Error::TIMED_OUT, "too slow")),
                },
            ]
        );
    }

    /// A frame that is not a message cannot be resynchronised past, so it is an error
    /// rather than an ending.
    #[tokio::test]
    async fn a_malformed_frame_is_an_error_and_not_an_end() {
        let payload = br#"{"jsonrpc":"2.0","method":"nonsense"}"#;
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(payload);

        let mut server = StdioServer::new(Cursor::new(bytes), Sent::default());
        let error = server.recv().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
