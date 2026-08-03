//! Measure the read sizes a kernel actually asks a [`Mountable`] for.
//!
//! A backend cannot negotiate its request size: it advertises a ceiling and the
//! kernel picks. The difference matters to any backend that fetches over a
//! network, because how much to read ahead is a ratio against whatever arrives.
//!
//! Recording happens at [`FileExt::read_at`] rather than inside a binding, for two
//! reasons: `PosixFs::read_handle` passes the kernel's `size` straight through, so
//! nothing is lost, and it is the exact call a remote backend would receive.
//!
//! # What this measured (macOS, FUSE-T, 2026-07)
//!
//! | access | request size | offsets contiguous |
//! |---|---|---|
//! | whole-file read | 32 KiB, every time | 94% |
//! | sequential 4 KiB reads | 32 KiB, every time | 96% |
//! | scattered 4 KiB reads | 32 KiB, every time | 11% |
//!
//! The size is uniform, and equal to macOS's default NFS `rsize` — FUSE-T serves
//! through a `go-nfsv4` helper, so what is measured here is that client, not a FUSE
//! kernel. Two conclusions follow, and the second is the useful one:
//!
//! **The caller's request size never reaches the backend.** Asking in 4 KiB pieces
//! produced the same 32 KiB requests as reading the whole file at once.
//!
//! **On this path the size carries no hint of the access pattern, but the offsets
//! do.** Sequential and scattered access are indistinguishable by size and obvious
//! by continuity. The ~5% of sequential reads that arrive out of order are the NFS
//! client batching; a backend that reads ahead on contiguity degrades gracefully
//! through them, because a reordered read that still lands inside an already
//! fetched window costs nothing.
//!
//! A Linux guest over virtio-fs agrees, more sharply: sequential reads arrive at
//! 128 KiB (after a 16K → 32K → 64K ramp) and 100% contiguous, scattered reads at
//! the 4 KiB the caller asked for and 0% contiguous — Linux switches its own
//! read-ahead off once access stops looking sequential. Reproducing that needs the
//! guest, not this example; see `apply_krun`.
//!
//! So a caching backend should infer the access pattern from **offset continuity
//! per handle**, never from the request size: one path signals nothing by size, and
//! both signal clearly by continuity.
//!
//! Each phase ends by printing a `SUMMARY` line with both columns of that table, so
//! there is nothing to post-process. The `READ <offset> <size>` lines are kept for
//! whatever the summary does not answer.
//!
//! ```sh
//! mkdir -p /tmp/cortex-measure
//! PKG_CONFIG_PATH="$PWD/contrib/pkgconfig:/usr/local/lib/pkgconfig" \
//!     cargo run --features fuse-t --example measure_read_sizes -- \
//!     /tmp/cortex-measure | tee /tmp/reads.log | grep SUMMARY
//! ```

use std::io::{self, Read};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use cortex::{
    Dirent, FileExt, FileHandle, InMemVolume, Mountable, OpenOptions, PosixFs, Result, Stat,
};

#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
use cortex::CortexMount as Mount;
#[cfg(feature = "fuse-t")]
use cortex::FuseTMount as Mount;

/// Set once the mount is up, so the reads the mount itself performs while coming
/// up do not land in the sample.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// Reads seen since the last phase boundary, as `(offset, size)`. Drained by
/// [`report`].
static SAMPLES: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

/// 32 MiB — large enough that a sequential read crosses many requests at any
/// plausible size, small enough to keep three copies in memory.
const FILE_SIZE: usize = 32 << 20;

/// Scattered-read step. Coprime with `FILE_SIZE` so successive offsets neither
/// repeat nor march in order — the access pattern that thrashes a single-window
/// cache.
const SCATTER_STEP: u64 = 7_919_003;

const SCATTER_READS: u64 = 200;

const SMALL_READ: usize = 4096;

/// A [`Mountable`] that forwards everything and reports the reads it is asked for.
struct Recorder<T>(T);

struct RecHandle<H>(H);

impl<H: FileExt> FileExt for RecHandle<H> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if RECORDING.load(Ordering::Relaxed) {
            // `buf.len()` *is* the kernel's `size`: `read_handle` allocates the
            // buffer from it and does not split or coalesce.
            println!("READ {offset} {}", buf.len());
            SAMPLES.lock().expect("samples").push((offset, buf.len()));
        }
        self.0.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.0.write_at(buf, offset)
    }
}

impl<H: FileHandle> FileHandle for RecHandle<H> {
    fn truncate(&self, size: u64) -> Result<()> {
        self.0.truncate(size)
    }

    fn flush(&self) -> Result<()> {
        self.0.flush()
    }

    fn commit(&self) -> Result<()> {
        self.0.commit()
    }
}

impl<T: Mountable> Mountable for Recorder<T> {
    type Handle = RecHandle<T::Handle>;

    fn stat(&self, path: &Path) -> Result<Stat> {
        self.0.stat(path)
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        self.0.list(path)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        self.0.mkdir(path)
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        self.0.unlink(path)
    }

    fn rmdir(&self, path: &Path) -> Result<()> {
        self.0.rmdir(path)
    }

    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        let (handle, stat) = self.0.open(path, options)?;
        Ok((RecHandle(handle), stat))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        self.0.rename(from, to)
    }
}

/// One file per phase. Reading the same file twice would be served from the host's
/// page cache and never reach the mount, so the second phase would sample nothing.
const PHASES: [&str; 3] = ["seq-whole.bin", "seq-small.bin", "scattered.bin"];

/// Drain the phase's samples and print the two numbers this example exists to produce.
///
/// A read is *contiguous* when it starts exactly where the previous one ended. That
/// ratio is the signal `S3Volume` reads its access pattern from, and the sizes beside it
/// are what shows the request size carrying no such signal.
fn report(phase: &str) {
    let samples = std::mem::take(&mut *SAMPLES.lock().expect("samples"));
    let Some(pairs) = samples.len().checked_sub(1).filter(|n| *n > 0) else {
        println!(
            "SUMMARY {phase}: {} reads, too few to compare",
            samples.len()
        );
        return;
    };
    let contiguous = samples
        .windows(2)
        .filter(|pair| pair[1].0 == pair[0].0 + pair[0].1 as u64)
        .count();
    let mut sizes: Vec<usize> = samples.iter().map(|(_, size)| *size).collect();
    sizes.sort_unstable();
    sizes.dedup();
    let sizes: Vec<String> = sizes
        .iter()
        .map(|size| format!("{}K", size / 1024))
        .collect();
    println!(
        "SUMMARY {phase}: {} reads, sizes [{}], {}% contiguous",
        samples.len(),
        sizes.join(" "),
        contiguous * 100 / pairs,
    );
}

fn main() {
    let mountpoint = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: measure_read_sizes <mountpoint>   (must already exist)");
        std::process::exit(2);
    });

    let vol = InMemVolume::new();
    let filler = vec![0xABu8; FILE_SIZE];
    for name in PHASES {
        let (file, _) = vol
            .open(Path::new(name), OpenOptions::create_new())
            .expect("fresh volume");
        file.write_all_at(&filler, 0).expect("fill");
    }
    drop(filler);

    let mount = Mount::spawn(PosixFs::new(Recorder(vol)), &mountpoint).expect("mount");
    let base = Path::new(&mountpoint);
    RECORDING.store(true, Ordering::Relaxed);

    // Phase 1: one whole-file read — what `cat`/`cp` does, and the pattern a
    // read-ahead window is meant for.
    println!("PHASE seq-whole");
    let read = std::fs::read(base.join(PHASES[0])).expect("read whole");
    assert_eq!(read.len(), FILE_SIZE, "whole-file read is short");
    drop(read);
    report("seq-whole");

    // Phase 2: sequential, but asked for in 4 KiB pieces. Shows whether the
    // caller's request size reaches us or the kernel decides on its own.
    println!("PHASE seq-small");
    let mut file = std::fs::File::open(base.join(PHASES[1])).expect("open small");
    let mut buf = vec![0u8; SMALL_READ];
    let mut total = 0usize;
    loop {
        match file.read(&mut buf).expect("read small") {
            0 => break,
            n => total += n,
        }
    }
    assert_eq!(total, FILE_SIZE, "sequential small reads are short");
    report("seq-small");

    // Phase 3: scattered 4 KiB reads through one handle — the case that evicts a
    // single-window cache on every request, with no concurrency.
    println!("PHASE scattered");
    let file = std::fs::File::open(base.join(PHASES[2])).expect("open scattered");
    let limit = (FILE_SIZE - SMALL_READ) as u64;
    let mut offset = 0u64;
    for _ in 0..SCATTER_READS {
        offset = (offset + SCATTER_STEP) % limit;
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut buf, offset).expect("scattered read");
    }
    drop(file);
    report("scattered");

    RECORDING.store(false, Ordering::Relaxed);
    mount.unmount().expect("unmount");
}
