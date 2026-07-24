//! Reference transport for the VM→host Executable forwarding channel.
//!
//! A hand-rolled, dependency-free HTTP/1.1 pair over `std::net`: the host
//! [`server`] and the guest [`client`], speaking a single `POST /exec`. The wire
//! types ([`ExecRequest`]/[`ExecOutput`]) and their framing are the shared
//! contract and live in the `cortex` core; this crate re-exports them so both ends
//! (and callers) reference one definition.
//!
//! A host that already has its own server can skip this crate and drive
//! [`cortex::ExecutableRegistry::run`] from its own handler.

pub mod client;
pub mod server;

pub use cortex::{ExecOutput, ExecRequest};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{HandlerError, handle_connection};
    use std::net::TcpListener;
    use std::thread;

    fn req() -> ExecRequest {
        ExecRequest {
            name: "ping".into(),
            args: vec!["x".into()],
        }
    }

    #[test]
    fn client_server_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream, None, &|r: ExecRequest| {
                Ok(ExecOutput::ok(r.name.into_bytes()))
            })
            .unwrap();
        });
        let out = crate::client::post(addr, None, &req()).unwrap();
        server.join().unwrap();
        assert_eq!(out.stdout, b"ping");
    }

    #[test]
    fn handler_not_found_maps_to_404() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle_connection(stream, None, &|_: ExecRequest| Err(HandlerError::NotFound));
        });
        let err = crate::client::post(addr, None, &req()).unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("404"));
    }

    #[test]
    fn bad_token_maps_to_401() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle_connection(stream, Some("secret"), &|_: ExecRequest| {
                Ok(ExecOutput::default())
            });
        });
        let err = crate::client::post(addr, Some("wrong"), &req()).unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("401"));
    }
}
