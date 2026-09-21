//! Answering what a process inside the guest asks this host to do for it.
//!
//! The other direction, and the other socket. [`uvm`](super::uvm) is this end asking the guest
//! to run something; this is something the guest is already running asking back — because a
//! model is here and a guest is a few vCPUs with no accelerator in front of them.
//!
//! **What crosses is [`HOSTCALL_ENV`]'s comment and not a type**, for the reason it gives: the
//! asking end is an `/abin` executable that does not take this crate. So the frames below are
//! written out by hand, against that description, and [`frames_are_what_the_contract_says`]
//! pins them to it. The other end does the same against the same paragraphs.
//!
//! Two of that contract's obligations are this end's, and both are here rather than implied:
//! an op this build does not know is answered with [`HOSTCALL_STATUS_ERROR`] rather than by
//! closing, and a frame over [`HOSTCALL_MAX_FRAME`] is refused rather than allocated.
//!
//! # It is a task of its own, and that is not a preference
//!
//! The server's own loop is in [`session`](super::super::session), and when one of these
//! questions arrives that loop is **inside** [`Uvm::call`](super::Uvm::call), waiting for the
//! answer to the `exec` that started the process now asking. A question answered there would
//! be a question answered after the thing that depends on it, which is a deadlock rather than
//! a slow reply. So the listener is spawned at boot and lives beside the session instead,
//! sharing nothing with it but the machine both are talking to.
//!
//! # And a blocking task per connection
//!
//! An inference is work and not waiting, which is what [`spawn_blocking`] is for: it runs off
//! the runtime's own threads, where it cannot keep the session's frames from being written
//! while it is busy.
//!
//! A connection is one caller, for as long as that caller has questions, and the caller is a
//! command: `index ingest` opens one, asks for as many vectors as it has chunks, and closes it
//! by exiting. So there is nothing to pair here — no ids, no table — and a connection that
//! ends is a caller that finished rather than anything to report.
//!
//! [`frames_are_what_the_contract_says`]: tests::frames_are_what_the_contract_says
//! [`spawn_blocking`]: tokio::task::spawn_blocking

use crate::contract::{
    HOSTCALL_MAX_FRAME, HOSTCALL_OP_NN_INFERENCE, HOSTCALL_STATUS_ERROR, HOSTCALL_STATUS_OK,
};

/// The length prefix: a big-endian `u32`.
const HEADER: usize = 4;

/// Answer on `listener` until the task is dropped.
///
/// The handle is the machine's: [`Uvm`](super::Uvm) aborts it on the way out, which is what
/// makes this end go away with the guest that could reach it rather than outliving it.
pub fn serve(listener: tokio::net::UnixListener) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let connection = match listener.accept().await {
                Ok((connection, _)) => connection,
                // Nothing here can mend a listener, and retrying one that is broken is a loop
                // that spins. What it costs to stop is that a session's delegation stops
                // working, which the next caller reports as a connection refused.
                Err(e) => {
                    eprintln!(
                        "{}: the host-call listener stopped: {e}",
                        env!("CARGO_BIN_NAME")
                    );
                    return;
                }
            };

            // Back to a blocking socket: what is about to run on it is work rather than
            // waiting, and the other end of this wire is a program with no runtime at all.
            let connection = match connection
                .into_std()
                .and_then(|connection| connection.set_nonblocking(false).map(|()| connection))
            {
                Ok(connection) => connection,
                Err(e) => {
                    eprintln!("{}: taking a host call: {e}", env!("CARGO_BIN_NAME"));
                    continue;
                }
            };

            tokio::task::spawn_blocking(move || {
                if let Err(e) = converse(connection) {
                    // Said and not answered: whatever went wrong here went wrong with the
                    // wire, so there is nowhere left to put a sentence. A caller sees its
                    // connection close, which is what it reports.
                    eprintln!("{}: a host call: {e:#}", env!("CARGO_BIN_NAME"));
                }
            });
        }
    })
}

/// One caller, for as long as it has questions.
fn converse(mut connection: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
    // `None` is the caller having closed between frames, which is how one that is done says
    // so — it exits, and the connection goes with it.
    while let Some(ask) = read_frame(&mut connection)? {
        // Never empty: `read_frame` refuses a frame that carries no op.
        let (op, argument) = (ask[0], &ask[1..]);
        let answered = match op {
            HOSTCALL_OP_NN_INFERENCE => nn_inference(argument),
            // Answered rather than closed, which is the contract's own rule: a caller from a
            // build that knows an op this one does not gets a sentence it can print.
            unknown => Err(format!(
                "op {unknown} is not one this host answers — it knows {HOSTCALL_OP_NN_INFERENCE} \
                 (nn_inference)"
            )),
        };
        write_frame(&mut connection, &answered)?;
    }
    Ok(())
}

/// Run a model on this host and say what it produced.
///
/// `Err` is a sentence for the caller to print, carried back as
/// [`HOSTCALL_STATUS_ERROR`] — a command that cannot have its inference is one that prints why
/// and exits, not a session that broke.
fn nn_inference(argument: &[u8]) -> Result<Vec<u8>, String> {
    todo!(
        "run a model on this host over {} argument bytes",
        argument.len()
    )
}

/// One answer, as the status byte and what follows it.
fn write_frame(
    w: &mut impl std::io::Write,
    answered: &Result<Vec<u8>, String>,
) -> anyhow::Result<()> {
    let mut body = Vec::new();
    match answered {
        Ok(result) => {
            body.push(HOSTCALL_STATUS_OK);
            body.extend_from_slice(result);
        }
        Err(said) => {
            body.push(HOSTCALL_STATUS_ERROR);
            body.extend_from_slice(said.as_bytes());
        }
    }
    anyhow::ensure!(
        body.len() <= HOSTCALL_MAX_FRAME,
        "an answer of {} bytes is more than the {HOSTCALL_MAX_FRAME} this wire agrees to send",
        body.len()
    );
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// One ask's body. `Ok(None)` is the caller having closed between frames.
fn read_frame(r: &mut impl std::io::Read) -> anyhow::Result<Option<Vec<u8>>> {
    let mut header = [0u8; HEADER];
    if !fill(r, &mut header)? {
        return Ok(None);
    }

    let len = u32::from_be_bytes(header) as usize;
    anyhow::ensure!(
        len <= HOSTCALL_MAX_FRAME,
        "a frame of {len} bytes is more than the {HOSTCALL_MAX_FRAME} this wire agrees to accept"
    );
    // Every frame carries at least its op, so this is a caller that has lost its place —
    // better said here than as an empty slice indexed a line later.
    anyhow::ensure!(len > 0, "a frame of no bytes cannot carry an op");

    let mut body = vec![0u8; len];
    anyhow::ensure!(fill(r, &mut body)?, "a frame ended mid-way");
    Ok(Some(body))
}

/// Fill `buf`. `Ok(false)` is a close *before any byte arrived*; stopping part way through is
/// corruption and not an ending.
fn fill(r: &mut impl std::io::Read, buf: &mut [u8]) -> anyhow::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => anyhow::bail!("a frame ended mid-way"),
            n => filled += n,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes, against the worked example in [`HOSTCALL_ENV`]'s comment.
    ///
    /// This is what a round-trip test would be if both ends were one type — they are not, and
    /// cannot be, so what is pinned here is this end against the **description**. The other end
    /// pins itself against the same paragraphs. A change to either that is not a change to
    /// those paragraphs is what this is here to fail on.
    ///
    /// [`HOSTCALL_ENV`]: crate::contract::HOSTCALL_ENV
    #[test]
    fn frames_are_what_the_contract_says() {
        // ask nn_inference, no arguments
        let asked = read_frame(&mut [0x00, 0x00, 0x00, 0x01, 0x01].as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(asked, vec![HOSTCALL_OP_NN_INFERENCE]);

        // ok, no result
        let mut wire = Vec::new();
        write_frame(&mut wire, &Ok(Vec::new())).unwrap();
        assert_eq!(wire, [0x00, 0x00, 0x00, 0x01, 0x00]);

        // an error is a status byte and a UTF-8 sentence
        let mut wire = Vec::new();
        write_frame(&mut wire, &Err("no model".to_string())).unwrap();
        assert_eq!(
            wire,
            [
                0x00, 0x00, 0x00, 0x09, 0x01, b'n', b'o', b' ', b'm', b'o', b'd', b'e', b'l'
            ]
        );
    }

    /// A caller that is done exits, and the connection goes with it. That is an ending; a frame
    /// that stops part way, or one that claims to carry nothing, is not.
    #[test]
    fn a_close_between_frames_is_an_ending_and_nothing_else_is() {
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none());

        assert!(read_frame(&mut [0x00, 0x00, 0x00, 0x02, 0x01].as_slice()).is_err());
        assert!(read_frame(&mut [0x00, 0x00].as_slice()).is_err());
        assert!(read_frame(&mut [0x00, 0x00, 0x00, 0x00].as_slice()).is_err());

        let absurd = u32::MAX.to_be_bytes();
        assert!(read_frame(&mut absurd.as_slice()).is_err());
    }
}
