//! End-to-end over the real binary: the file plane, and that it shares a namespace with
//! the commands.
//!
//! A `read` and a `write` are answered by the server process, not by this one, so the only
//! way to see what they actually do is to put a file somewhere and ask across the channel
//! — and then to run a command that opens the same path by the same name, which is the
//! property the two planes are supposed to have.

use std::process::Stdio;

use cortex::console::{Console, Error, ExecResult, Failure, ReadResult, stdio::StdioClient};
use tempfile::TempDir;
use tokio::process::Command;

/// A console over the real binary, and a directory to put files in.
struct Fixture {
    console: Console,
    dir: TempDir,
}

impl Fixture {
    async fn new() -> Fixture {
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
        server.stderr(Stdio::inherit());
        let client = StdioClient::new(server).expect("starting the console server");

        // Building announces the session; nothing here delegates anything, which is a
        // session too.
        let console = Console::builder()
            .client(client)
            .build()
            .await
            .expect("building the console");

        Fixture {
            console,
            dir: tempfile::tempdir().expect("a temp directory"),
        }
    }

    /// An absolute path under this fixture's directory. Absolute because the server is a
    /// process of its own and its working directory is not this test's to assume.
    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_str().expect("utf-8").into()
    }

    async fn write(&mut self, path: &str, data: &[u8], offset: Option<u64>) -> u64 {
        self.console
            .write(self.path(path), data, offset)
            .await
            .expect("writing")
            .size
    }

    async fn read(&mut self, path: &str, offset: Option<u64>, len: Option<u64>) -> ReadResult {
        self.console
            .read(self.path(path), offset, len)
            .await
            .expect("reading")
    }

    async fn output(&mut self, script: &str) -> ExecResult {
        self.console
            .exec(["sh", "-c", script], None)
            .await
            .expect("running the command")
    }
}

/// The round trip, on bytes that are not text: what a `write` sent is what a `read`
/// brings back, and `size` is the whole of it.
#[tokio::test]
async fn what_a_write_sent_is_what_a_read_brings_back() {
    let mut fx = Fixture::new().await;
    let bytes = [0xff, 0xfe, 0x00, b'\n', 0x00];

    assert_eq!(fx.write("raw", &bytes, None).await, 5);

    let out = fx.read("raw", None, None).await;
    assert_eq!(out.data, bytes);
    assert_eq!(out.size, 5);
}

/// The two planes name the same file. A command writes it, a `read` brings it back, a
/// `write` replaces it, and the next command sees that.
#[tokio::test]
async fn a_file_is_the_same_file_to_a_command() {
    let mut fx = Fixture::new().await;
    let path = fx.path("shared");

    fx.output(&format!("printf 'from the command' > {path}"))
        .await;
    assert_eq!(
        fx.read("shared", None, None).await.data,
        b"from the command"
    );

    fx.write("shared", b"from the client", None).await;
    assert_eq!(
        fx.output(&format!("cat {path}")).await.stdout,
        b"from the client"
    );
}

/// No offset means the file is to *be* `data`, so a shorter write leaves nothing of a
/// longer file behind.
#[tokio::test]
async fn a_whole_file_write_replaces_what_was_there() {
    let mut fx = Fixture::new().await;

    fx.write("f", b"a much longer first version", None).await;
    assert_eq!(fx.write("f", b"short", None).await, 5);

    let out = fx.read("f", None, None).await;
    assert_eq!(out.data, b"short");
    assert_eq!(out.size, 5);
}

/// An offset speaks only for the bytes it covers, so the rest of the file stays — which
/// is what makes `None` and `Some(0)` different requests.
#[tokio::test]
async fn a_positioned_write_leaves_the_rest_alone() {
    let mut fx = Fixture::new().await;

    fx.write("f", b"aaaaaaaaaa", None).await;
    assert_eq!(fx.write("f", b"BBB", Some(3)).await, 10);
    assert_eq!(fx.read("f", None, None).await.data, b"aaaBBBaaaa");

    // The same bytes at the same place, sent the other way, are the whole file instead.
    assert_eq!(fx.write("f", b"BBB", None).await, 3);
    assert_eq!(fx.read("f", None, None).await.data, b"BBB");
}

/// Writing past the end extends the file, and the gap reads as zeroes rather than as
/// whatever was on the disk.
#[tokio::test]
async fn a_write_past_the_end_zero_fills_the_gap() {
    let mut fx = Fixture::new().await;

    fx.write("f", b"ab", None).await;
    assert_eq!(fx.write("f", b"z", Some(5)).await, 6);
    assert_eq!(fx.read("f", None, None).await.data, b"ab\0\0\0z");
}

/// A bounded read hands back its slice, and `size` is the file rather than the slice —
/// which is the only thing that says there is more to ask for.
#[tokio::test]
async fn a_bounded_read_says_how_much_it_left() {
    let mut fx = Fixture::new().await;
    fx.write("f", b"0123456789", None).await;

    let out = fx.read("f", Some(3), Some(4)).await;
    assert_eq!(out.data, b"3456");
    assert_eq!(out.size, 10);

    // Asking for more than is left is not an error; it is just the rest.
    let out = fx.read("f", Some(8), Some(100)).await;
    assert_eq!(out.data, b"89");
    assert_eq!(out.size, 10);
}

/// Starting past the end asks for nothing, which is an empty answer and not a failure.
#[tokio::test]
async fn a_read_past_the_end_is_empty_and_not_an_error() {
    let mut fx = Fixture::new().await;
    fx.write("f", b"0123456789", None).await;

    let out = fx.read("f", Some(50), None).await;
    assert!(out.data.is_empty());
    assert_eq!(out.size, 10);
}

/// A `write` makes the file but not the directories above it, so a path through one that
/// is not there is the same `NOT_FOUND` a missing file is to a `read`.
#[tokio::test]
async fn a_path_that_is_not_there_is_not_found() {
    let mut fx = Fixture::new().await;

    let missing = fx.path("nothing-here");
    let refused = fx
        .console
        .read(missing, None, None)
        .await
        .expect_err("reading a file that is not there");
    assert_eq!(refused.code(), Some(Error::NOT_FOUND));

    let nested = fx.path("no/such/dir/f");
    let refused = fx
        .console
        .write(nested, b"x".as_slice(), None)
        .await
        .expect_err("writing under a directory that is not there");
    assert_eq!(refused.code(), Some(Error::NOT_FOUND));
}

/// A directory has no bytes either way, and says so apart from being absent: the name is
/// taken, and by something a retry will not turn into a file.
#[tokio::test]
async fn a_directory_is_neither_readable_nor_writable() {
    let mut fx = Fixture::new().await;
    let dir: String = fx.dir.path().to_str().expect("utf-8").into();

    let refused = fx
        .console
        .read(&dir, None, None)
        .await
        .expect_err("reading a directory");
    assert_eq!(refused.code(), Some(Error::IS_A_DIRECTORY));

    let refused = fx
        .console
        .write(&dir, b"x".as_slice(), None)
        .await
        .expect_err("writing over a directory");
    assert_eq!(refused.code(), Some(Error::IS_A_DIRECTORY));
}

/// A refusal is about the call and not the session: the channel is still good, and the
/// next thing asked on it is answered.
#[tokio::test]
async fn a_refused_file_call_leaves_the_session_usable() {
    let mut fx = Fixture::new().await;

    let refused: Failure = fx
        .console
        .read(fx.path("gone"), None, None)
        .await
        .expect_err("reading a file that is not there");
    assert!(matches!(refused, Failure::Refused(_)));

    fx.write("after", b"still here", None).await;
    assert_eq!(fx.read("after", None, None).await.data, b"still here");
    assert_eq!(fx.output("echo ok").await.stdout, b"ok\n");
}
