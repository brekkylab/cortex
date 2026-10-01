//! The answering end over a framed channel: bring a message in, put a response out.

use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};

use futures_core::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

use crate::protocol::{
    Message, RequestId, Response, Server,
    stdio::{read, write},
};

/// Whether this process has already taken its stdin and stdout, so a second
/// [`StdioServer::stdio`] cannot become a second owner and interleave frames.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// A [`Server`] over one readable and one writable descriptor.
///
/// Trait objects rather than type parameters: behavior does not depend on the
/// descriptor type. Holds no session state.
///
/// ```no_run
/// use virtx::protocol::stdio::StdioServer;
/// use virtx::protocol::{Message, Response, Server};
///
/// # fn answer(call: virtx::protocol::Call) -> Response { unimplemented!() }
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// // Takes stdin and stdout for the protocol; everything else goes to stderr.
/// let mut server = StdioServer::stdio()?;
///
/// while let Some(message) = server.recv().await? {
///     if let Message::Request { id, call } = message {
///         server.respond(id, answer(call)).await?;
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct StdioServer {
    /// Where requests come from.
    incoming: BufReader<Box<dyn AsyncRead + Send + Unpin>>,

    /// Where responses go.
    outgoing: Box<dyn AsyncWrite + Send + Unpin>,
}

impl StdioServer {
    /// Over any two descriptors: stdin/stdout, socket halves, or a `Cursor` and `Vec` in tests.
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
    /// From here on **stdin and stdout carry frames only**; diagnostics go to stderr. A
    /// rule, not enforced: a `println!` elsewhere reaches the same descriptor without
    /// passing through this end and corrupts the stream, as with an MCP stdio server. Only
    /// this end's own frames are kept from interleaving, by the writer's single owner.
    ///
    /// Fails if called twice: the claim is process-wide.
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

    fn respond(&mut self, id: RequestId, result: Response) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { write(&mut self.outgoing, &Message::Response { id, result }).await })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
    };

    use super::*;
    use crate::protocol::{Call, Error, ExecCall, InitCall, InitResp, Notification};

    /// Everything this end wrote, readable while the server still owns the writer.
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

    /// Every message arrives in order, uninterpreted, including notifications and
    /// responses.
    #[tokio::test]
    async fn every_message_arrives_as_it_was_sent() {
        let sent = vec![
            request(0, Call::Init(InitCall::default())),
            Message::Notification(Notification::Start),
            request(
                1,
                Call::Exec(ExecCall {
                    cmd: vec!["echo".into(), "hi".into()],
                    ..ExecCall::default()
                }),
            ),
            Message::Notification(Notification::Stop),
            // Not a request; passed through regardless.
            Message::Response {
                id: 99,
                result: Response::Init(InitResp::default()),
            },
            Message::Notification(Notification::Quit),
        ];

        let mut server = StdioServer::new(Cursor::new(framed(&sent).await), Sent::default());
        for message in &sent {
            assert_eq!(server.recv().await.unwrap().as_ref(), Some(message));
        }
        // The end of input is a clean close.
        assert!(server.recv().await.unwrap().is_none());
    }

    /// A response goes out framed with the id it was given.
    #[tokio::test]
    async fn a_response_carries_the_id_it_was_given() {
        let sent = Sent::default();
        let mut server = StdioServer::new(tokio::io::empty(), sent.clone());

        server
            .respond(7, Response::Init(InitResp::default()))
            .await
            .unwrap();
        server
            .respond(9, Response::Error(Error::new(Error::TIMED_OUT, "too slow")))
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
                    result: Response::Init(InitResp::default()),
                },
                Message::Response {
                    id: 9,
                    result: Response::Error(Error::new(Error::TIMED_OUT, "too slow")),
                },
            ]
        );
    }

    /// A malformed frame is an error, not a clean end.
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
