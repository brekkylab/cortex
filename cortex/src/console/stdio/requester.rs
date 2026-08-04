//! The asking end, over a framed channel: one call out, one response back.
//!
//! A client only asks. Nothing arrives on this channel but the answers to what it
//! asked, so this is a send, a read and a match — no threads, no locks, no pending
//! table. A delegated executable is *not* an exception: the shim that runs one
//! reaches whoever owns the behaviour on a channel of its own, which is what keeps
//! this end a requester.
//!
//! The protocol's methods are [`Requestable`]'s. What is here is only what the wire adds:
//! an id per call, and waiting for the response that brings it back.
//!
//! # Two descriptors, and nothing about where they came from
//!
//! A client is handed the pair it talks over and knows nothing else. Not what program
//! a server is, nor how it was started, nor who collects it — those are decisions this
//! end has no part in, and leaving them out is what lets the same code drive a child's
//! pipes, a virtio port into a guest, or both ends inside one test.
//!
//! It does own the descriptors, though, which is the one thing a caller has to know:
//! **dropping the client closes the writer**, and for a server process that closed
//! stdin is how it learns the session is over. Whoever holds the process should expect
//! it to end shortly after.
//!
//! [`Console`](crate::console::Console) is this plus the process and the names a server
//! may call back into, and is what a caller normally wants.

use std::io::{self, BufReader, Read, Write};

use crate::console::stdio::{read, write};
use crate::console::{Call, Failure, Message, Notification, Outcome, RequestId, Requestable};

/// A [`Requestable`] over one readable and one writable descriptor.
///
/// The two directions are separate fields and not a pair, because that is what they
/// are: what goes out has nothing to do with what comes back beyond the framing they
/// share, and holding them apart is what lets [`call`](Requestable::call) write and then
/// read without giving up one borrow for the other.
///
/// Trait objects rather than type parameters. Which descriptors these are is not
/// something this end reads differently, so a pair of parameters would only put the
/// answer in every signature that mentions a client — including
/// [`Console`](crate::console::Console), which holds one behind a `dyn Requestable` and so
/// could not use the answer anyway.
pub struct StdioRequester {
    /// Where responses come from.
    ///
    /// Buffered here, once. Nothing hands this to a command — it is the protocol's for
    /// the life of the session — so reading ahead cannot take a byte that was somebody
    /// else's.
    incoming: BufReader<Box<dyn Read + Send>>,

    /// Where calls go.
    outgoing: Box<dyn Write + Send>,

    /// From zero, by one. Nothing on the other side reads a meaning into the number,
    /// and it only has to be unique among this client's own — see [`RequestId`].
    next_id: RequestId,
}

impl StdioRequester {
    /// Take the two descriptors, whatever they are — a child's stdout and stdin, a
    /// virtio port, a `Cursor` and a `Vec` for a test.
    ///
    /// Owned rather than borrowed: they are the protocol's until this client is
    /// dropped, and a caller that could still reach them could take a byte that was a
    /// response's.
    pub fn new(
        incoming: impl Read + Send + 'static,
        outgoing: impl Write + Send + 'static,
    ) -> Self {
        StdioRequester {
            incoming: BufReader::new(Box::new(incoming)),
            outgoing: Box::new(outgoing),
            next_id: 0,
        }
    }

    fn send(&mut self, message: &Message) -> Result<(), Failure> {
        write(&mut self.outgoing, message).map_err(broke("sending a message"))
    }

    fn recv(&mut self) -> Result<Option<Message>, Failure> {
        read(&mut self.incoming).map_err(broke("reading a message"))
    }
}

impl Requestable for StdioRequester {
    /// Send one call and read until the response with its id arrives.
    ///
    /// Anything else that arrives is a server that has lost track of itself. A
    /// response to a call nobody made is noted and dropped; a *request* is a server
    /// trying to be a client, which this end has no answer for.
    fn call(&mut self, call: Call) -> Result<Outcome, Failure> {
        // Spent before the send rather than after the answer, so a call that fails
        // part way does not leave its id for the next one to reuse.
        let id = self.next_id;
        self.next_id += 1;

        self.send(&Message::Request { id, call })?;

        loop {
            let Some(message) = self.recv()? else {
                return Err(Failure::broken(format!(
                    "the server closed the channel before answering request {id}"
                )));
            };

            match message {
                Message::Response {
                    id: answered,
                    outcome,
                } if answered == id => return Ok(outcome),

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

    fn notify(&mut self, notification: Notification) -> Result<(), Failure> {
        self.send(&Message::Notification(notification))
    }
}

/// An [`io::Error`] on this channel is about the session and not the call: there is no
/// answer and there will not be one.
fn broke(doing: &'static str) -> impl FnOnce(io::Error) -> Failure {
    move |e| Failure::Broken(anyhow::Error::new(e).context(doing))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::console::{Error, Exec, ExecResult, Method, Start};

    /// Everything the client wrote, readable after it has been dropped or not — a
    /// `Vec` cannot be, once the client owns it.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl Write for Sent {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sent {
        /// The id and method of each message, in the order they went out.
        fn messages(&self) -> Vec<(Option<RequestId>, Option<Method>)> {
            let bytes = self.0.lock().unwrap().clone();
            let mut reader = bytes.as_slice();
            let mut sent = Vec::new();
            while let Some(message) = read(&mut reader).unwrap() {
                sent.push((message.id(), message.method()));
            }
            sent
        }
    }

    /// A client over a canned stream of what a server would have said — which is
    /// enough for the whole protocol, now that everything this end reads is a
    /// response.
    fn driving(incoming: &[Message]) -> (StdioRequester, Sent) {
        let mut bytes = Vec::new();
        for message in incoming {
            write(&mut bytes, message).unwrap();
        }
        let sent = Sent::default();
        (StdioRequester::new(Cursor::new(bytes), sent.clone()), sent)
    }

    fn ran(id: RequestId, stdout: &[u8]) -> Message {
        Message::Response {
            id,
            outcome: Outcome::Result(
                serde_json::to_value(ExecResult {
                    code: 0,
                    stdout: stdout.to_vec(),
                    ..ExecResult::default()
                })
                .unwrap(),
            ),
        }
    }

    fn null(id: RequestId) -> Message {
        Message::Response {
            id,
            outcome: Outcome::Result(serde_json::Value::Null),
        }
    }

    /// The ordinary session, and the ids it allocates: from zero, by one.
    #[test]
    fn a_session_is_start_then_execs_then_stop() {
        let (mut client, sent) = driving(&[null(0), ran(1, b"hi\n"), null(2)]);

        client
            .start(Start {
                delegated: vec!["foo".into()],
                default_timeout_ms: Some(30_000),
            })
            .unwrap();
        let result = client
            .exec(Exec {
                cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
                ..Exec::default()
            })
            .unwrap();
        assert_eq!(result.stdout, b"hi\n");
        client.stop().unwrap();
        client.quit().unwrap();

        assert_eq!(
            sent.messages(),
            [
                (Some(0), Some(Method::Start)),
                (Some(1), Some(Method::Exec)),
                (Some(2), Some(Method::Stop)),
                // A notification: no id, because nothing answers it.
                (None, Some(Method::Quit)),
            ]
        );
    }

    /// The two failures a caller does different things about.
    #[test]
    fn a_refusal_is_an_answer_and_a_closed_channel_is_not() {
        let (mut client, _) = driving(&[Message::Response {
            id: 0,
            outcome: Outcome::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
        }]);
        let failure = client.exec(Exec::default()).unwrap_err();
        assert_eq!(failure.code(), Some(Error::TIMED_OUT));

        // Nothing at all: the server closed without answering.
        let (mut client, _) = driving(&[]);
        let failure = client.exec(Exec::default()).unwrap_err();
        assert_eq!(failure.code(), None);
        assert!(
            failure.to_string().contains("before answering request 0"),
            "{failure}"
        );
    }

    /// A response to a call nobody made is dropped; a request is not something this
    /// end can answer at all.
    #[test]
    fn a_client_answers_nothing() {
        let (mut client, _) = driving(&[ran(99, b"who asked"), ran(0, b"mine\n")]);
        assert_eq!(client.exec(Exec::default()).unwrap().stdout, b"mine\n");

        let (mut client, _) = driving(&[Message::Request {
            id: 1,
            call: Call::Stop,
        }]);
        let failure = client.exec(Exec::default()).unwrap_err();
        assert!(failure.to_string().contains("cannot answer"), "{failure}");
    }

    /// An id is spent whether or not its call worked, so a failed call cannot leave
    /// its number for the next one to reuse.
    #[test]
    fn a_failed_call_still_spends_its_id() {
        // Nothing is ever answered, so both calls fail — after their requests have
        // already gone out, which is the part that matters.
        let (mut client, sent) = driving(&[]);
        assert!(client.exec(Exec::default()).is_err());
        assert!(client.stop().is_err());

        assert_eq!(
            sent.messages(),
            [(Some(0), Some(Method::Exec)), (Some(1), Some(Method::Stop))]
        );
    }
}
