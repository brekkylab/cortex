//! `infer` — run a model, by asking the host to.
//!
//! ```sh
//! infer          # is there a host to ask, and does it answer?
//! ```
//!
//! A session is a few vCPUs with no accelerator and the weights are the host's, so the work
//! this program names is not work it does: it dials out and hands the question over.
//!
//! # What it does not do yet
//!
//! **Take a tensor, or answer with one.** That is what this op is for and it is not settled —
//! neither what a tensor looks like on this wire nor, therefore, what this program's arguments
//! are. What exists today is the round trip with an empty argument, which is worth having on
//! its own: it is the only way to find out, from inside a guest, whether the channel is there
//! at all.
//!
//! So the output is stages rather than a result, and each one is a different thing to fix:
//!
//! 1. **a vsock socket** — needs a driver in the guest kernel. libkrunfw either has one or it
//!    does not, and nothing on the host side can be asked.
//! 2. **a connection** — needs the boot to have attached the port and the server to have bound
//!    the socket behind it. Reaching this far is the whole channel proven, because the connect
//!    only completes once the host socket has accepted.
//! 3. **a frame** — the two ends agreeing on the bytes.
//!
//! # The wire, and why it is written out here
//!
//! The description this is written against is `contract::HOSTCALL_ENV` in
//! `cortex-uvm-v2-common`, and this program deliberately does not take that crate — see its
//! manifest. So the bytes are repeated here, and [`frames_are_what_the_contract_says`] pins
//! them; the host end does the same against the same paragraphs.
//!
//! ```text
//! ask     [u32 BE len][len bytes:  op u8     | argument bytes ]
//! answer  [u32 BE len][len bytes:  status u8 | result bytes   ]
//! ```
//!
//! `len` is never zero. A close before a header's first byte is the conversation ending; one
//! part way through a frame is truncation. An error's bytes are a UTF-8 sentence, which is why
//! a refusal is something to print rather than a connection that dropped.
//!
//! [`frames_are_what_the_contract_says`]: tests::frames_are_what_the_contract_says

// On a host build nothing below `run` is reachable — `run` is the sentence saying so — but it
// is still compiled and still tested, and that is the point: the layout pin runs on the
// machine somebody edits this on rather than only inside a guest.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io;
use std::process::ExitCode;

/// Where this session's host is, as `vsock:<port>`. Spelled out rather than imported — see the
/// module docs.
const HOSTCALL_ENV: &str = "CORTEX_UVM_HOSTCALL";

/// The well-known context id of the host, as every vsock guest addresses it.
const HOST_CID: libc::c_uint = 2;

/// Run a model and hand back what it produced.
const OP_NN_INFERENCE: u8 = 1;

/// The answer is what was asked for; anything else is a UTF-8 sentence saying why there is
/// none.
const STATUS_OK: u8 = 0;

/// The most one frame may carry.
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// The length prefix: a big-endian `u32`.
const HEADER: usize = 4;

/// The shell's code for "found it, could not run it", which is what this is when it cannot
/// reach a host.
const NOT_EXECUTABLE: u8 = 126;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

#[cfg(target_os = "linux")]
fn run() -> anyhow::Result<()> {
    let spec = std::env::var(HOSTCALL_ENV).map_err(|_| {
        anyhow::anyhow!("{HOSTCALL_ENV} is unset — this session was given no host to ask")
    })?;
    println!("{HOSTCALL_ENV}={spec}");

    let mut host = Host::dial(port(&spec)?)?;
    println!("connected: the guest has a vsock driver and the port reaches the server");

    // Empty, because what an inference takes is not settled — see the module docs.
    match host.call(OP_NN_INFERENCE, &[]) {
        Ok(result) => println!("answered: {} result bytes", result.len()),
        Err(e) => println!("no answer: {e:#}"),
    }
    Ok(())
}

/// This program asks over AF_VSOCK, which exists inside the guest and nowhere else. It is
/// built for the host too — it is a workspace member, and a bare `cargo build` builds every
/// member — so there has to be something here to build, and the honest something is the
/// sentence.
#[cfg(not(target_os = "linux"))]
fn run() -> anyhow::Result<()> {
    anyhow::bail!(
        "this asks a host over AF_VSOCK, which exists inside a guest and not on {}. Build it \
         for the guest with `scripts/build-abin.sh` and run it in a session.",
        std::env::consts::OS
    )
}

/// The port in `vsock:<port>`.
///
/// An argument parser would be a dependency for one value that is either spelled right or not.
fn port(spec: &str) -> anyhow::Result<u32> {
    let port = spec.strip_prefix("vsock:").ok_or_else(|| {
        anyhow::anyhow!(
            "{HOSTCALL_ENV} is `{spec}`, which names no transport this build has — only \
             `vsock:<port>`"
        )
    })?;
    port.parse()
        .map_err(|_| anyhow::anyhow!("{HOSTCALL_ENV} is `{spec}`, and `{port}` is not a port"))
}

/// The host, as this process reaches it.
///
/// A descriptor and its two traits rather than one of the standard socket types: an AF_VSOCK
/// socket is neither a `UnixStream` nor a `TcpStream`, and building one of those out of this
/// descriptor would be a type that answers `peer_addr` with a lie.
#[cfg(target_os = "linux")]
struct Host {
    socket: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl Host {
    fn dial(port: u32) -> anyhow::Result<Host> {
        use std::os::fd::FromRawFd as _;

        // SAFETY: a plain `socket(2)`. A negative return is the only failure and is checked
        // before the descriptor is claimed below.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
        anyhow::ensure!(
            fd >= 0,
            "opening a vsock socket: {} — this guest's kernel may have no vsock driver",
            io::Error::last_os_error()
        );
        // SAFETY: `fd` is a fresh descriptor nothing else holds, handed over exactly once, so
        // what is built from it is its only owner and closes it once.
        let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };

        // SAFETY: all-zero is a valid `sockaddr_vm`; the three fields set are the whole of
        // what a connect on this family reads.
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = HOST_CID;
        addr.svm_port = port as libc::c_uint;

        // SAFETY: `addr` is a `sockaddr_vm` and the length passed is that type's own; the
        // descriptor is the one opened above and still owned here.
        let connected = unsafe {
            libc::connect(
                std::os::fd::AsRawFd::as_raw_fd(&socket),
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        anyhow::ensure!(
            connected == 0,
            "connecting to the host on vsock port {port}: {} — nothing is listening there, \
             which is a server started without a socket for this",
            io::Error::last_os_error()
        );
        Ok(Host { socket })
    }

    /// Ask one question and wait for its answer.
    ///
    /// There is one outstanding at a time and this connection is this process's, so what comes
    /// back answers what went out — which is the whole reason nothing here carries an id.
    fn call(&mut self, op: u8, argument: &[u8]) -> anyhow::Result<Vec<u8>> {
        write_frame(self, op, argument)?;
        let answer = read_frame(self)?
            .ok_or_else(|| anyhow::anyhow!("the host closed the connection without answering"))?;
        // Never empty: `read_frame` refuses a frame that carries no status.
        let (status, payload) = (answer[0], &answer[1..]);
        anyhow::ensure!(
            status == STATUS_OK,
            "the host refused: {}",
            String::from_utf8_lossy(payload)
        );
        Ok(payload.to_vec())
    }
}

#[cfg(target_os = "linux")]
impl io::Read for Host {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is a slice this call may write `buf.len()` bytes into, and the
        // descriptor is the connected socket this type owns.
        let read = unsafe {
            libc::read(
                std::os::fd::AsRawFd::as_raw_fd(&self.socket),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        match read {
            -1 => Err(io::Error::last_os_error()),
            n => Ok(n as usize),
        }
    }
}

#[cfg(target_os = "linux")]
impl io::Write for Host {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is a slice this call only reads `buf.len()` bytes of, and the
        // descriptor is the connected socket this type owns.
        let written = unsafe {
            libc::write(
                std::os::fd::AsRawFd::as_raw_fd(&self.socket),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
            )
        };
        match written {
            -1 => Err(io::Error::last_os_error()),
            n => Ok(n as usize),
        }
    }

    /// Nothing is held back, so there is nothing to push: every write above is the syscall.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One ask: the op byte, and whatever the op takes after it.
fn write_frame(w: &mut impl io::Write, op: u8, argument: &[u8]) -> anyhow::Result<()> {
    let len = 1 + argument.len();
    anyhow::ensure!(
        len <= MAX_FRAME,
        "an ask of {len} bytes is more than the {MAX_FRAME} this wire agrees to send"
    );
    w.write_all(&(len as u32).to_be_bytes())?;
    w.write_all(&[op])?;
    w.write_all(argument)?;
    w.flush()?;
    Ok(())
}

/// One answer's body. `Ok(None)` is the host having closed between frames.
fn read_frame(r: &mut impl io::Read) -> anyhow::Result<Option<Vec<u8>>> {
    let mut header = [0u8; HEADER];
    if !fill(r, &mut header)? {
        return Ok(None);
    }

    let len = u32::from_be_bytes(header) as usize;
    anyhow::ensure!(
        len <= MAX_FRAME,
        "a frame of {len} bytes is more than the {MAX_FRAME} this wire agrees to accept"
    );
    // Every frame carries at least its status, so this is a peer that has lost its place.
    anyhow::ensure!(len > 0, "a frame of no bytes cannot carry a status");

    let mut body = vec![0u8; len];
    anyhow::ensure!(fill(r, &mut body)?, "a frame ended mid-way");
    Ok(Some(body))
}

/// Fill `buf`. `Ok(false)` is a close *before any byte arrived*; stopping part way through is
/// corruption and not an ending.
fn fill(r: &mut impl io::Read, buf: &mut [u8]) -> anyhow::Result<bool> {
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

    /// The bytes, against the worked example in `contract::HOSTCALL_ENV`'s comment.
    ///
    /// This is what a round-trip test would be if both ends were one type — they are not, and
    /// deliberately cannot be, so what is pinned here is this end against the **description**.
    /// The host end pins itself against the same paragraphs. A change to either that is not a
    /// change to those paragraphs is what this is here to fail on.
    #[test]
    fn frames_are_what_the_contract_says() {
        let mut wire = Vec::new();
        write_frame(&mut wire, OP_NN_INFERENCE, &[]).unwrap();
        assert_eq!(wire, [0x00, 0x00, 0x00, 0x01, 0x01]);

        let ok = read_frame(&mut [0x00, 0x00, 0x00, 0x01, 0x00].as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(ok, vec![STATUS_OK]);

        let refused = read_frame(
            &mut [
                0x00, 0x00, 0x00, 0x09, 0x01, b'n', b'o', b' ', b'm', b'o', b'd', b'e', b'l',
            ]
            .as_slice(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(refused[0], 1);
        assert_eq!(&refused[1..], b"no model");
    }

    /// The host closing between frames is an ending. A frame that stops part way, or one that
    /// claims to carry nothing, is not.
    #[test]
    fn a_close_between_frames_is_an_ending_and_nothing_else_is() {
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
        assert!(read_frame(&mut [0x00, 0x00, 0x00, 0x02, 0x00].as_slice()).is_err());
        assert!(read_frame(&mut [0x00, 0x00].as_slice()).is_err());
        assert!(read_frame(&mut [0x00, 0x00, 0x00, 0x00].as_slice()).is_err());
        assert!(read_frame(&mut u32::MAX.to_be_bytes().as_slice()).is_err());
    }

    /// The spelling the boot writes, read back the way a caller reads it.
    #[test]
    fn a_transport_this_build_does_not_have_is_refused() {
        assert_eq!(port("vsock:1024").unwrap(), 1024);
        for spelling in [
            "",
            "1024",
            "unix:/tmp/hostcall.sock",
            "vsock:",
            "vsock:http",
        ] {
            assert!(port(spelling).is_err(), "{spelling} was accepted");
        }
    }
}
