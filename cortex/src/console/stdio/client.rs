//! The asking end, over a framed channel: one call out, one response back.
//!
//! A client only asks. Nothing arrives on this channel but the answers to what it
//! asked, so this is a send, a read and a match — no threads, no locks, no pending
//! table. A delegated executable is *not* an exception: a server that needs one run
//! says so in the response to the `exec` it was already going to answer
//! ([`Progress::Delegated`](crate::console::Progress::Delegated)), so what arrives here
//! is still only ever a response.
//!
//! The protocol's methods come from [`Client`]. What is here is only what the wire adds:
//! an id per call, and waiting for the response that brings it back.
//!
//! # The pipes, and the process they belong to
//!
//! A client over stdio is a client over some program's pipes, so [`new`](StdioClient::new)
//! takes the program and starts it. What a caller decides is the command — what to run,
//! with what arguments and environment, where its stderr goes; the two descriptors the
//! protocol runs on are this end's, because a caller who got those wrong would have a
//! client with nothing to say.
//!
//! The pipes and the process are therefore one fact, and this end holds both.
//! [`quit`](Client::quit) is the ending: the writer is dropped — which is how a
//! server learns the session is over — and then the process is waited for. Dropping a
//! client that was never quit kills it instead, so a server does not outlive the channel
//! to it either way.
//!
//! Which is why the process is not a [`Console`]'s. What a session *is* — the methods, the
//! delegated names, walking the delegation chain — is the same wherever a server runs;
//! something to `wait` for exists only because this transport is a pipe to a child. A
//! transport into a micro-VM guest is a channel with no process behind it, and a `Console`
//! over one should not carry a field for a thing that does not exist.
//!
//! [`Console`] is this plus the names a server may call back into, and is what a caller
//! normally wants.
//!
//! [`Console`]: crate::console::Console

use std::io::{self, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};

use crate::console::stdio::{read, write};
use crate::console::{Call, Client, Failure, Message, Notification, Outcome, RequestId};

/// A [`Client`] over a server process's pipes, and the process itself.
///
/// The two directions are separate fields and not a pair, because that is what they
/// are: what goes out has nothing to do with what comes back beyond the framing they
/// share, and holding them apart is what lets [`call`](Client::call) write and then
/// read without giving up one borrow for the other.
///
/// Trait objects rather than the child's own descriptor types. Reading frames and writing
/// them is all this end does with either, so naming the types would buy nothing and cost
/// two things: a writer that can be let go of when the session ends without the field
/// becoming an `Option`, and a test that can drive this over a canned stream instead of a
/// process.
pub struct StdioClient {
    /// Where responses come from.
    ///
    /// Buffered here, once. Nothing hands this to a command — it is the protocol's for
    /// the life of the session — so reading ahead cannot take a byte that was somebody
    /// else's.
    incoming: BufReader<Box<dyn Read + Send>>,

    /// Where calls go — the server's stdin, until [`quit`](Client::quit) closes it.
    outgoing: Box<dyn Write + Send>,

    /// The process those pipes belong to.
    ///
    /// `None` once it has been collected, which is all this `Option` says and the only
    /// state either end of the session needs: a client with no process left is a client
    /// whose session is over, so nothing has to be tracked twice.
    server: Option<Child>,

    /// From zero, by one. Nothing on the other side reads a meaning into the number,
    /// and it only has to be unique among this client's own — see [`RequestId`].
    next_id: RequestId,
}

impl StdioClient {
    /// Start `server` and drive the session over its own pipes.
    ///
    /// Everything about the command is the caller's — what to run, its arguments, its
    /// environment, where its stderr goes — except the two descriptors the protocol needs,
    /// which are set here. A caller who got those wrong would have a client with nothing
    /// to say, so they are not something to get wrong.
    ///
    /// The pipes are the protocol's from here on, and so is the process: nothing else may
    /// reach either, because a reader that could would take a byte that was a response's.
    pub fn new(mut server: Command) -> io::Result<StdioClient> {
        let mut server = server
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        // Both are `Some`: they were asked for immediately above.
        let outgoing = server.stdin.take().expect("a piped stdin");
        let incoming = server.stdout.take().expect("a piped stdout");

        Ok(StdioClient {
            incoming: BufReader::new(Box::new(incoming)),
            outgoing: Box::new(outgoing),
            server: Some(server),
            next_id: 0,
        })
    }

    /// A client over two descriptors and no process, for the tests below.
    ///
    /// Which is where a canned stream of what a server *would* have said comes from — a
    /// server that has lost track of its own ids is not something a real program does on
    /// request. Not public: what a caller has is a program to run, and a client that could
    /// be built without one would have a process to collect on some paths and not others.
    #[cfg(test)]
    fn over(
        incoming: impl Read + Send + 'static,
        outgoing: impl Write + Send + 'static,
    ) -> StdioClient {
        StdioClient {
            incoming: BufReader::new(Box::new(incoming)),
            outgoing: Box::new(outgoing),
            server: None,
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

impl Client for StdioClient {
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

    /// Say the session is over, close the pipe, and wait for the server to go.
    ///
    /// In that order, and each step because of the next one. A server ends on `quit`; one
    /// that somehow missed it still sees its stdin end, and a `wait` that kept the pipe
    /// open would be a wait that does not return. The writer is swapped for a sink rather
    /// than removed — anything asked after a session is over goes nowhere, which is what a
    /// closed pipe would have made of it anyway.
    ///
    /// What comes back is the process's ending and not the notification's: a server that
    /// exited cleanly did not need to hear `quit` to know, and a send that failed is a
    /// server that was already gone. A bad exit is a [`Broken`](Failure::Broken) carrying
    /// the status — nothing can be done about it by then, but a server that died is not the
    /// ending a caller asked for. With no process to collect, the notification is the whole
    /// of the ending and its result is what this is.
    ///
    /// Calling this twice is not an error; the second time there is nothing left to collect.
    fn quit(&mut self) -> Result<(), Failure> {
        let said = self.notify(Notification::Quit);
        self.outgoing = Box::new(io::sink());

        let Some(mut server) = self.server.take() else {
            return said;
        };

        let status = server
            .wait()
            .map_err(broke("waiting for the console server"))?;

        if !status.success() {
            return Err(Failure::broken(format!(
                "the console server ended with {status}"
            )));
        }
        Ok(())
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // A session that ended by hand has nothing here. One that did not gets no grace:
        // there is no telling what a server left mid-session is waiting for, and a `wait`
        // in a `drop` that guessed wrong would hang whoever is dropping it.
        if let Some(server) = self.server.as_mut() {
            let _ = server.kill();
            let _ = server.wait();
        }
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
    use crate::console::{Error, Exec, ExecResult, Method, Progress, Start};

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
    fn driving(incoming: &[Message]) -> (StdioClient, Sent) {
        let mut bytes = Vec::new();
        for message in incoming {
            write(&mut bytes, message).unwrap();
        }
        let sent = Sent::default();
        (StdioClient::over(Cursor::new(bytes), sent.clone()), sent)
    }

    /// An execution that finished without delegating anything, which is the only shape
    /// this end can see without something out here resolving a name.
    fn ran(id: RequestId, stdout: &[u8]) -> Message {
        Message::Response {
            id,
            outcome: Outcome::Result(
                bson::serialize_to_bson(&Progress::Done(ExecResult {
                    code: 0,
                    stdout: stdout.to_vec(),
                    ..ExecResult::default()
                }))
                .unwrap(),
            ),
        }
    }

    /// The [`ExecResult`] of a finished execution, or a panic if it was not one.
    fn done(progress: Progress) -> ExecResult {
        match progress {
            Progress::Done(result) => result,
            Progress::Delegated(exec) => panic!("still delegating {:?}", exec.cmd),
        }
    }

    fn null(id: RequestId) -> Message {
        Message::Response {
            id,
            outcome: Outcome::Result(bson::Bson::Null),
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
        assert_eq!(done(result).stdout, b"hi\n");
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
        let result = done(client.exec(Exec::default()).unwrap());
        assert_eq!(result.stdout, b"mine\n");

        let (mut client, _) = driving(&[Message::Request {
            id: 1,
            call: Call::Stop,
        }]);
        let failure = client.exec(Exec::default()).unwrap_err();
        assert!(failure.to_string().contains("cannot answer"), "{failure}");
    }

    /// The process is the client's, so how it ended is the client's to report — and
    /// `quit` is where a caller hears it.
    ///
    /// Neither of these programs speaks the protocol, which is the point: what is being
    /// tested is the ending, and an ending is the one thing this end does not need an
    /// answer for.
    #[test]
    fn quitting_collects_the_server_and_says_how_it_went() {
        let mut ends_badly = Command::new("sh");
        ends_badly.args(["-c", "exit 3"]);
        let mut client = StdioClient::new(ends_badly).unwrap();

        let failure = client.quit().unwrap_err();
        assert_eq!(failure.code(), None, "a dead server is not a refusal");
        assert!(failure.to_string().contains("exit status: 3"), "{failure}");

        // Collected once. A second `quit` has nothing left to wait for and says nothing
        // about a status it already reported.
        client.quit().unwrap();

        let mut ends_well = Command::new("sh");
        ends_well.args(["-c", "exit 0"]);
        StdioClient::new(ends_well).unwrap().quit().unwrap();
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
