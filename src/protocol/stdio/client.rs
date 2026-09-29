//! The asking end over a framed channel: one call out, one response back.
//!
//! Only answers arrive on this channel, so a call is a send, a read and a match: no tasks,
//! locks or pending table. The methods come from [`Client`]; this adds an id per call and
//! waiting for the response carrying it.
//!
//! # The pipes and their process
//!
//! [`new`](StdioClient::new) starts the server program; the caller chooses the command,
//! arguments and environment, and this end sets the protocol's descriptors.
//!
//! [`quit`](Client::quit) drops the writer (how the server learns the session is over) and
//! waits for the process. Dropping an unquit client, or the whole process exiting, ends it
//! the same way minus the wait, since nothing may await on drop.
//!
//! **The server is never killed**, so it can release what the session made (a machine, a
//! disk, a built image) when its input ends. The cost: a server stuck in a call outlives
//! the client until that call returns.
//!
//! The process lives here, not on [`ConsoleClient`], because only a pipe-to-child
//! transport has one; a micro-VM guest channel has no process behind it.
//! [`ConsoleClient`] is this plus the session it was built with, and is what a caller
//! normally wants.
//!
//! [`ConsoleClient`]: crate::console::ConsoleClient

use std::{io, process::Stdio, time::Duration};

use futures_core::future::BoxFuture;
use tokio::{
    io::{AsyncRead, AsyncWrite, BufReader},
    process::{Child, Command},
    task::JoinHandle,
};

use crate::protocol::{
    Call, Client, Failure, Message, Notification, RequestId, Response,
    stdio::{read, write},
};

/// A [`Client`] over a server process's pipes, and the process itself.
///
/// The directions are separate fields so [`call`](Client::call) can borrow both at once.
///
/// Trait objects rather than the child's descriptor types, so the writer can be swapped
/// for a sink at `quit` without an `Option`, and tests can drive a canned stream.
pub struct StdioClient {
    /// Where responses come from. Buffering is safe: the protocol owns this for the
    /// session's lifetime, so reading ahead takes no one else's bytes.
    incoming: BufReader<Box<dyn AsyncRead + Send + Unpin>>,

    /// The server's stdin, until [`quit`](Client::quit) closes it.
    outgoing: Box<dyn AsyncWrite + Send + Unpin>,

    /// The process those pipes belong to; `None` once collected, which also means the
    /// session is over.
    server: Option<Child>,

    stderr: Option<JoinHandle<()>>,

    /// Unique only among this client's calls; see [`RequestId`].
    next_id: RequestId,
}

impl StdioClient {
    /// Start `server` and drive the session over its pipes.
    ///
    /// The caller's command settings are kept except stdin, stdout and stderr, which this
    /// sets. Nothing else may touch the pipes, or it would steal response bytes.
    ///
    /// Must be called within a Tokio runtime, which registers the child for reaping.
    pub fn new(mut server: Command) -> io::Result<StdioClient> {
        let mut server = server
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // `Some`: piped just above.
        let outgoing = server.stdin.take().expect("a piped stdin");
        let incoming = server.stdout.take().expect("a piped stdout");
        let mut said = server.stderr.take().expect("a piped stderr");
        let stderr = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut said, &mut tokio::io::stderr()).await;
        });

        Ok(StdioClient {
            incoming: BufReader::new(Box::new(incoming)),
            outgoing: Box::new(outgoing),
            server: Some(server),
            stderr: Some(stderr),
            next_id: 0,
        })
    }

    /// A client over two descriptors and no process, so tests can replay canned server
    /// output (e.g. a server that lost track of its ids).
    #[cfg(test)]
    fn over(
        incoming: impl AsyncRead + Send + Unpin + 'static,
        outgoing: impl AsyncWrite + Send + Unpin + 'static,
    ) -> StdioClient {
        StdioClient {
            incoming: BufReader::new(Box::new(incoming)),
            outgoing: Box::new(outgoing),
            server: None,
            stderr: None,
            next_id: 0,
        }
    }

    async fn send(&mut self, message: &Message) -> Result<(), Failure> {
        write(&mut self.outgoing, message)
            .await
            .map_err(broke("sending a message"))
    }

    async fn recv(&mut self) -> Result<Option<Message>, Failure> {
        read(&mut self.incoming)
            .await
            .map_err(broke("reading a message"))
    }

    /// Put a request out under `id` and read until the response carrying it arrives.
    ///
    /// Separate from [`call`](Client::call) so the id is spent before any `?` here can
    /// return early.
    async fn round_trip(&mut self, id: RequestId, call: Call) -> Result<Response, Failure> {
        self.send(&Message::Request { id, call }).await?;

        loop {
            let Some(message) = self.recv().await? else {
                return Err(Failure::broken(format!(
                    "the server closed the channel before answering request {id}"
                )));
            };

            match message {
                // An error response becomes `Refused`, so callers handle "no answer" as a
                // `Failure` alongside a broken channel.
                Message::Response {
                    id: answered,
                    result,
                } if answered == id => {
                    return match result {
                        Response::Error(error) => Err(Failure::Refused(error)),
                        answer => Ok(answer),
                    };
                }

                Message::Response { id: answered, .. } => eprintln!(
                    "console: a response arrived for request {answered}, which nobody made"
                ),

                other => {
                    return Err(Failure::broken(format!(
                        "the server sent {other:?}, which a client cannot answer"
                    )));
                }
            }
        }
    }
}

impl Client for StdioClient {
    /// Send one call and read until the response with its id arrives.
    ///
    /// A response to an unissued id is logged and dropped; a request from the server
    /// breaks the channel.
    ///
    /// The round trip holds both descriptors in one borrow, so no other frame can come
    /// between the request and its response.
    fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
        Box::pin(async move {
            // Spent before sending, so a failed call's id is never reused.
            let id = self.next_id;
            self.next_id += 1;

            self.round_trip(id, call).await
        })
    }

    fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move { self.send(&Message::Notification(notification)).await })
    }

    /// Say the session is over, close the pipe, and wait for the server to exit.
    ///
    /// The pipe must close before `wait`, so a server that missed `quit` still sees EOF
    /// instead of hanging. The writer becomes a sink, so later sends go nowhere.
    ///
    /// Returns the process's outcome, not the notification's (a failed send just means
    /// the server was already gone): a non-zero exit is [`Broken`](Failure::Broken) with
    /// the status. With no process, the notification's result is returned.
    ///
    /// Idempotent: a second call has nothing to collect.
    fn quit(&mut self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            let said = self.notify(Notification::Quit).await;
            self.outgoing = Box::new(tokio::io::sink());

            let Some(mut server) = self.server.take() else {
                return said;
            };

            let status = server
                .wait()
                .await
                .map_err(broke("waiting for the console server"))?;

            if let Some(stderr) = self.stderr.take() {
                let _ = tokio::time::timeout(Duration::from_secs(1), stderr).await;
            }

            if !status.success() {
                return Err(Failure::broken(format!(
                    "the console server ended with {status}"
                )));
            }
            Ok(())
        })
    }
}

/// An [`io::Error`] here breaks the session, not just the call.
fn broke(doing: &'static str) -> impl FnOnce(io::Error) -> Failure {
    move |e| Failure::Broken(anyhow::Error::new(e).context(doing))
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use super::*;
    use crate::protocol::{Error, ExecCall, ExecResp, InitCall, InitResp, Method, ReadCall};

    /// Everything the client wrote, readable while the client still owns the writer.
    /// Never pends.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl AsyncWrite for Sent {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Sent {
        /// The id and method of each message, in the order they went out.
        async fn messages(&self) -> Vec<(Option<RequestId>, Option<Method>)> {
            let bytes = self.0.lock().unwrap().clone();
            let mut reader = bytes.as_slice();
            let mut sent = Vec::new();
            while let Some(message) = read(&mut reader).await.unwrap() {
                sent.push((message.id(), message.method()));
            }
            sent
        }
    }

    /// A client over a canned stream of server output.
    async fn driving(incoming: &[Message]) -> (StdioClient, Sent) {
        let mut bytes = Vec::new();
        for message in incoming {
            write(&mut bytes, message).await.unwrap();
        }
        let sent = Sent::default();
        (StdioClient::over(Cursor::new(bytes), sent.clone()), sent)
    }

    /// A successful `exec` response.
    fn ran(id: RequestId, stdout: &[u8]) -> Message {
        Message::Response {
            id,
            result: Response::Exec(ExecResp {
                code: 0,
                stdout: stdout.to_vec(),
                ..ExecResp::default()
            }),
        }
    }

    /// A default `init` response.
    fn initialized(id: RequestId) -> Message {
        Message::Response {
            id,
            result: Response::Init(InitResp::default()),
        }
    }

    /// Ids count from zero over calls only; notifications take none.
    #[tokio::test]
    async fn a_session_is_init_then_execs() {
        let (mut client, sent) = driving(&[initialized(0), ran(1, b"hi\n")]).await;

        client.init(InitCall::default()).await.unwrap();
        client.start().await.unwrap();
        // Id 1: `init` took 0, and `start` takes none.
        let result = client
            .exec(ExecCall {
                cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
                ..ExecCall::default()
            })
            .await;
        assert_eq!(result.unwrap().stdout, b"hi\n");
        client.stop().await.unwrap();
        client.quit().await.unwrap();

        assert_eq!(
            sent.messages().await,
            [
                (Some(0), Some(Method::Init)),
                (None, Some(Method::Start)),
                (Some(1), Some(Method::Exec)),
                (None, Some(Method::Stop)),
                (None, Some(Method::Quit)),
            ]
        );
    }

    /// A refusal carries a code; a closed channel does not.
    #[tokio::test]
    async fn a_refusal_is_an_answer_and_a_closed_channel_is_not() {
        let (mut client, _) = driving(&[Message::Response {
            id: 0,
            result: Response::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
        }])
        .await;
        let failure = client.exec(ExecCall::default()).await.unwrap_err();
        assert_eq!(failure.code(), Some(Error::TIMED_OUT));

        // Nothing at all: the server closed without answering.
        let (mut client, _) = driving(&[]).await;
        let failure = client.exec(ExecCall::default()).await.unwrap_err();
        assert_eq!(failure.code(), None);
        assert!(
            failure.to_string().contains("before answering request 0"),
            "{failure}"
        );
    }

    /// A response to an unissued id is dropped; a request from the server is an error.
    #[tokio::test]
    async fn a_client_answers_nothing() {
        let (mut client, _) = driving(&[ran(99, b"who asked"), ran(0, b"mine\n")]).await;
        let result = client.exec(ExecCall::default()).await.unwrap();
        assert_eq!(result.stdout, b"mine\n");

        let (mut client, _) = driving(&[Message::Request {
            id: 1,
            call: Call::Exec(ExecCall::default()),
        }])
        .await;
        let failure = client.exec(ExecCall::default()).await.unwrap_err();
        assert!(failure.to_string().contains("cannot answer"), "{failure}");
    }

    /// `quit` reports how the server process exited. These programs do not speak the
    /// protocol; only the ending is tested.
    #[tokio::test]
    async fn quitting_collects_the_server_and_says_how_it_went() {
        let mut ends_badly = Command::new("sh");
        ends_badly.args(["-c", "exit 3"]);
        let mut client = StdioClient::new(ends_badly).unwrap();

        let failure = client.quit().await.unwrap_err();
        assert_eq!(failure.code(), None, "a dead server is not a refusal");
        assert!(failure.to_string().contains("exit status: 3"), "{failure}");

        // A second `quit` has nothing to collect and does not re-report the status.
        client.quit().await.unwrap();

        let mut ends_well = Command::new("sh");
        ends_well.args(["-c", "exit 0"]);
        StdioClient::new(ends_well).unwrap().quit().await.unwrap();
    }

    /// A failed call still spends its id (visible only on the wire).
    #[tokio::test]
    async fn a_failed_call_still_spends_its_id() {
        // Both fail unanswered, after their requests went out.
        let (mut client, sent) = driving(&[]).await;
        assert!(client.exec(ExecCall::default()).await.is_err());
        assert!(client.read(ReadCall::default()).await.is_err());

        assert_eq!(
            sent.messages().await,
            [(Some(0), Some(Method::Exec)), (Some(1), Some(Method::Read))]
        );
    }
}
