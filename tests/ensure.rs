//! The default server is fetched by building on it, and only by that.
//!
//! Releases come from a bucket stood up on localhost (`$VIRTX_DIST_URL`), into a
//! `$VIRTX_HOME` of the test's own. One test, because both are process-wide: a binary of its
//! own, so nothing else in it reads them.
//!
//! The archive's `virtx-uvm` is a stand-in that is no server, so a console built on it fails
//! after the fetch -- which is how the fetch is seen, without a real release.

#![cfg(feature = "ensure")]

use std::{
    io::Write as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use virtx::{console::ConsoleClient, ensure_virtx, image::ImageClient};

const SERVER: &str = if cfg!(windows) {
    "virtx-uvm.exe"
} else {
    "virtx-uvm"
};

/// A release archive holding only a `virtx-uvm` that exits at once.
fn archive() -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    {
        let mut tar = tar::Builder::new(&mut gz);
        let body = b"#!/bin/sh\nexit 0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, SERVER, &body[..]).unwrap();
        tar.finish().unwrap();
    }
    gz.flush().unwrap();
    gz.finish().unwrap()
}

/// Serve `archive()` as release `good` for this platform, and 404 for anything else,
/// counting what it served. Slow on purpose, so callers at once all find the server missing.
async fn bucket() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let served = Arc::new(AtomicUsize::new(0));
    let good = format!(
        "/virtx-uvm/good/virtx-uvm-{}-{}.tar.gz",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let body = archive();
    let count = served.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (good, body, count) = (good.clone(), body.clone(), count.clone());
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let path = String::from_utf8_lossy(&request)
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                tokio::time::sleep(Duration::from_millis(200)).await;
                let (status, body) = if path == good {
                    count.fetch_add(1, Ordering::SeqCst);
                    ("200 OK", body)
                } else {
                    ("404 Not Found", Vec::new())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    (url, served)
}

fn set(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: this binary's one test sets these before anything reads them concurrently;
    // the bucket's tasks read none.
    unsafe { std::env::set_var(key, value) };
}

#[tokio::test(flavor = "multi_thread")]
async fn building_on_the_default_server_fetches_it() {
    let (url, served) = bucket().await;
    set("VIRTX_DIST_URL", &url);

    // A release that is not there: building on the default server says so, rather than
    // failing to start a program that was never fetched.
    let home = tempfile::tempdir().unwrap();
    set("VIRTX_HOME", home.path());
    set("VIRTX_UVM_VERSION", "missing");
    let err = format!(
        "{:#}",
        ConsoleClient::builder().build().await.err().unwrap()
    );
    assert!(err.contains("no virtx-uvm release is published"), "{err}");
    let err = format!("{:?}", ImageClient::try_new().await.err().unwrap());
    assert!(err.contains("no virtx-uvm release is published"), "{err}");

    // A server named by the caller is the caller's: nothing is fetched for it.
    let err = format!(
        "{:#}",
        ConsoleClient::builder()
            .cmd(&["virtx-no-such-server"])
            .build()
            .await
            .err()
            .unwrap()
    );
    assert!(!err.contains("virtx-uvm release"), "{err}");

    // Every caller at once on a fresh host gets the server, fetched once.
    set("VIRTX_UVM_VERSION", "good");
    let calls: Vec<_> = (0..8).map(|_| tokio::spawn(ensure_virtx())).collect();
    for call in calls {
        let bin = call.await.unwrap().unwrap();
        assert_eq!(bin, home.path().join("bin"));
    }
    assert!(home.path().join("bin").join(SERVER).is_file());
    assert_eq!(served.load(Ordering::SeqCst), 1);

    // Building a console on a fresh host fetches the server before starting it. The stand-in
    // is no server, so the build fails -- after the fetch.
    let home = tempfile::tempdir().unwrap();
    set("VIRTX_HOME", home.path());
    assert!(ConsoleClient::builder().build().await.is_err());
    assert!(home.path().join("bin").join(SERVER).is_file());
    assert_eq!(served.load(Ordering::SeqCst), 2);

    // And an image client the same way.
    let home = tempfile::tempdir().unwrap();
    set("VIRTX_HOME", home.path());
    assert!(ImageClient::try_new().await.is_err());
    assert!(home.path().join("bin").join(SERVER).is_file());
    assert_eq!(served.load(Ordering::SeqCst), 3);
}
