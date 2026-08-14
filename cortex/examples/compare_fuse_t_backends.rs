//! What each of FUSE-T's transports actually asks a store for.
//!
//! FUSE-T's helper carries three backends — an NFSv4 server, an SMB server, and an FSKit
//! module — and cortex picks one with a mount option ([`FuseTBackend`]). The vtable above them
//! is the same either way, but *what the kernel sends through it* is not: the request size, the
//! order operations arrive in, and whether a truncating open arrives as a truncation at all are
//! the client's, not ours.
//!
//! So this mounts one [`InMemFs`] behind a recorder, runs the same fixed script of ordinary
//! file operations against the mountpoint, and prints what the store saw. Run it once per
//! backend and diff.
//!
//! ```sh
//! mkdir -p /tmp/cortex-cmp
//! for b in nfs fskit; do
//!   PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
//!     cargo run --features fuse-t --example compare_fuse_t_backends -- /tmp/cortex-cmp $b \
//!     > /tmp/$b.log
//! done
//! diff /tmp/nfs.log /tmp/fskit.log
//! ```
//!
//! # What this measured (macOS 26.0, FUSE-T 1.2.7, 2026-08-14)
//!
//! Same store, same script, both backends:
//!
//! | | nfs | fskit |
//! |---|---|---|
//! | 256 KiB whole-file read | **8 × 32 KiB** | **1 × 256 KiB** |
//! | `O_TRUNC` open | `truncate size=0` | `truncate size=0` |
//! | `flush` | after the read, twice per close | **never** |
//! | `stat`s per mutation | 2–4, including a re-`stat` of the parent | 1–2 |
//! | unlink of an open file | **the client** renames it to `.nfs<hex>` | `unlink`, which `Posix` turns into a rename |
//!
//! Three of those are worth carrying:
//!
//! **The request size means different things on the two.** 32 KiB is macOS's default NFS
//! `rsize`: through that transport the caller's own size never reaches the store at all —
//! measured earlier at 32 KiB uniformly whether a caller read 4 KiB at a time or the whole file
//! at once, and whether it read sequentially or scattered. Through FSKit it reaches it exactly:
//! one `read(2)` of the whole file arrived as one 256 KiB request.
//!
//! So a store inferring access patterns must do it from **offset continuity**, which carries on
//! both, and must not start trusting size now that one path conveys it. (Continuity is also
//! what a Linux guest over virtio-fs signals most sharply: sequential reads arrive 100%
//! contiguous at 128 KiB after a 16K → 32K → 64K ramp, scattered ones at the caller's 4 KiB and
//! 0% contiguous, Linux switching its own read-ahead off once access stops looking sequential.)
//!
//! **FSKit never asked for a flush.** A store that treated `flush` as its durability point
//! would simply never be asked on this transport — which is why [`FileSystem`]'s contract is
//! that a write is durable when it returns, and `flush` is an `fsync` on a name rather than a
//! promise the store may wait for.
//!
//! **`O_TRUNC` decomposes identically.** Both send it as a separate truncation, so
//! [`Posix`](cortex::fs::Posix) handing it to [`FileSystem::truncate`] is right for both, and
//! the store never sees an open at all.
//!
//! **Unlink-while-open is already solved on one of them, and by the same trick.** The macOS NFS
//! client renames the file to `.nfs<hex>` itself, writes through that name, and removes it when
//! the last descriptor closes — a store on that transport never sees the `unlink` at all. FSKit
//! sends the `unlink`, so what answers there is [`Posix::unlink_child`](cortex::fs::Posix)
//! doing the identical thing under its own prefix. Two transports, one behaviour, and the
//! measurement is how you can tell which half is doing it.
//!
//! [`FileSystem`]: cortex::fs::FileSystem

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use cortex::BoxFuture;
use cortex::fs::{Dirent, FileSystem, FuseTBackend, FuseTMount, InMemFs, Mount, Stat, WorkFs};

/// Set once the mount is up, so the traffic the mount itself makes while coming up does not
/// land in the sample.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// One line per call the store saw, in order.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn note(line: String) {
    if RECORDING.load(Ordering::Relaxed) {
        LOG.lock().unwrap().push(line);
    }
}

/// Forwards every call to the store inside and writes down what it was asked.
///
/// A `FileSystem` in its own right, which is the point: this is what any store would have
/// seen, so what it records is the kernel's behaviour and not this example's.
struct Recorder<T>(T);

impl<T: FileSystem> FileSystem for Recorder<T> {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<Stat>> {
        note(format!("stat     {}", path.display()));
        self.0.stat(path)
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<Vec<Dirent>>> {
        note(format!("list     {}", path.display()));
        self.0.list(path)
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, std::io::Result<usize>> {
        note(format!(
            "read     {} off={offset} len={}",
            path.display(),
            buf.len()
        ));
        self.0.read_at(path, buf, offset)
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<Stat>> {
        note(format!("create   {}", path.display()));
        self.0.create(path)
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<Stat>> {
        note(format!("mkdir    {}", path.display()));
        self.0.mkdir(path)
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        note(format!("unlink   {}", path.display()));
        self.0.unlink(path)
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        note(format!("rmdir    {}", path.display()));
        self.0.rmdir(path)
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, std::io::Result<usize>> {
        note(format!(
            "write    {} off={offset} len={}",
            path.display(),
            buf.len()
        ));
        self.0.write_at(path, buf, offset)
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, std::io::Result<()>> {
        note(format!("truncate {} size={size}", path.display()));
        self.0.truncate(path, size)
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        note(format!("rename   {} -> {}", from.display(), to.display()));
        self.0.rename(from, to)
    }

    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        note(format!("flush    {}", path.display()));
        self.0.flush(path)
    }
}

/// 256 KiB, so a whole-file read crosses several requests at any plausible size.
const FILE_SIZE: usize = 256 << 10;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(mountpoint), backend) = (args.next(), args.next()) else {
        eprintln!("usage: compare_fuse_t_backends <mountpoint> [nfs|fskit|smb]");
        std::process::exit(2);
    };
    let mountpoint = PathBuf::from(mountpoint);
    fs::create_dir_all(&mountpoint).expect("mount point");

    let backend = match backend.as_deref() {
        None | Some("nfs") => FuseTBackend::Nfs,
        Some("fskit") => FuseTBackend::FsKit,
        Some("smb") => FuseTBackend::Smb,
        Some(other) => {
            eprintln!("unknown backend {other:?}");
            std::process::exit(2);
        }
    };

    // A composed tree rather than one store, because that is what a real mount is: a `WorkFs`
    // routing to whichever store claimed the path. `nested/` is a directory no store knows
    // about — it exists only because a mount lies below it — so a listing that shows it is the
    // mount table answering.
    let root = InMemFs::new();
    let nested = InMemFs::new();
    // Set up before mounting, on a throwaway runtime: a store's own plane is async, and nothing
    // is serving yet.
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        root.create(Path::new("/hello.txt")).await.unwrap();
        root.write_at(Path::new("/hello.txt"), &vec![b'x'; FILE_SIZE], 0)
            .await
            .unwrap();
        root.mkdir(Path::new("/sub")).await.unwrap();
        nested.create(Path::new("/inner.txt")).await.unwrap();
        nested
            .write_at(Path::new("/inner.txt"), b"from the nested store\n", 0)
            .await
            .unwrap();
    });
    let store = WorkFs::new()
        .try_with_mount("", root)
        .and_then(|ws| ws.try_with_mount("nested/deep", nested))
        .expect("neither mount path escapes the root");

    let mount = FuseTMount::try_new_with(Recorder(store), &mountpoint, backend).expect("mount");
    println!("== backend {backend:?} at {}", mount.mountpoint().display());
    RECORDING.store(true, Ordering::Relaxed);

    let at = |name: &str| mountpoint.join(name);
    phase("list the root", || {
        let mut names: Vec<_> = fs::read_dir(&mountpoint)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        println!("  names: {names:?}");
    });

    phase("walk into the mount table", || {
        // `nested` is synthesized and `nested/deep` is the mount point; the file under it comes
        // from the second store.
        let mut names: Vec<_> = fs::read_dir(mountpoint.join("nested"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        println!("  names: {names:?}");
        print!(
            "  nested/deep/inner.txt: {}",
            fs::read_to_string(at("nested/deep/inner.txt")).unwrap()
        );
    });

    phase("read the whole file", || {
        let bytes = fs::read(at("hello.txt")).unwrap();
        println!("  read {} bytes", bytes.len());
    });

    phase("open with O_TRUNC and write", || {
        let mut f = fs::File::create(at("hello.txt")).unwrap();
        f.write_all(b"short").unwrap();
        drop(f);
        println!(
            "  size now {}",
            fs::metadata(at("hello.txt")).unwrap().len()
        );
    });

    phase("create a new file", || {
        fs::write(at("fresh.txt"), b"hi").unwrap();
    });

    phase("rename it", || {
        fs::rename(at("fresh.txt"), at("moved.txt")).unwrap();
    });

    phase("unlink a file that is still open", || {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

        // What every tempfile does: make it, unlink it at once, go on using the descriptor.
        // The store should see the name moved aside rather than removed, and removed for real
        // only after the last handle closes.
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(at("temp.txt"))
            .unwrap();
        fs::remove_file(at("temp.txt")).unwrap();
        assert!(!at("temp.txt").exists(), "the name is gone");

        f.write_all(b"still here").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut back = String::new();
        f.read_to_string(&mut back).unwrap();
        println!("  read back through the unlinked handle: {back:?}");
        assert!(
            !fs::read_dir(&mountpoint).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("cortex-unlinked")),
            "what is held aside must not show in a listing"
        );
        drop(f);
    });

    phase("remove a file and a directory", || {
        fs::remove_file(at("moved.txt")).unwrap();
        fs::remove_dir(at("sub")).unwrap();
    });

    // Dropped explicitly, so the unmount happens before the summary rather than at exit.
    drop(mount);
    println!("== unmounted");
}

/// Run one probe and print every call it produced, then the read sizes it used.
fn phase(what: &str, probe: impl FnOnce()) {
    LOG.lock().unwrap().clear();
    println!("\n-- {what}");
    probe();
    let seen = std::mem::take(&mut *LOG.lock().unwrap());
    let mut sizes: Vec<usize> = seen
        .iter()
        .filter_map(|line| line.split("len=").nth(1)?.parse().ok())
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    for line in &seen {
        println!("  {line}");
    }
    if !sizes.is_empty() {
        println!("  SIZES {sizes:?}");
    }
}
