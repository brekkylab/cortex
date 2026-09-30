//! Taking this process's mounts down on catchable signals.

use std::{
    io,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicI32, Ordering},
    },
};

use super::{claim::live, table::unmount_under};

/// The signals a person or supervisor sends to ask a process to stop. Any other catchable
/// signal means the process is already broken, and the mount is the smaller problem.
const CAUGHT: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// The self-pipe's write end, atomic because a handler may not lock. `-1` until installed.
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// Take this process's mounts down when it is asked to stop.
///
/// Handles `SIGINT`, `SIGTERM`, `SIGHUP` and `SIGQUIT` by unmounting everything this
/// process has mounted, then restoring the previous disposition and re-raising, so the signal
/// ends the process as it would have. Idempotent.
///
/// A signal runs no destructors, so a guard's mount outlives the process with nothing
/// answering it, and anything walking the path (`git status` included) hangs until it comes
/// down. Without this, the process's watchdog unmounts it a moment after the process ends, or
/// [`reclaim_abandoned`](super::reclaim_abandoned) in a later run; this does it before.
///
/// Opt-in: a signal disposition is process-global, and installing one while mounting would
/// overwrite whatever the embedding program arranged.
///
/// **Limits:** `SIGKILL` is uncatchable, a handler installed later replaces this one, and a
/// wedged mount may refuse to come down; the unmount is bounded, and survivors are reported
/// on stderr.
///
/// `Err` if the pipe, the thread, or a handler fails to install. A partway failure leaves the
/// earlier handlers working; they are not rolled back, since restoring a disposition could
/// overwrite a handler the program installed meanwhile.
pub fn unmount_on_signal() -> io::Result<()> {
    static INSTALLED: OnceLock<io::Result<()>> = OnceLock::new();
    // `io::Error` is not `Clone`, so later callers get the first failure's kind and message.
    match INSTALLED.get_or_init(install) {
        Ok(()) => Ok(()),
        Err(err) => Err(io::Error::new(err.kind(), err.to_string())),
    }
}

fn install() -> io::Result<()> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe` fills two descriptors or returns -1.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let [read, write] = fds;
    for fd in [read, write] {
        // `CLOEXEC` so programs this process runs do not inherit the pipe.
        //
        // SAFETY: `fd` was just returned by `pipe`.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }

    std::thread::Builder::new()
        .name("cortex-unmount-on-signal".into())
        .spawn(move || watch(read))?;

    // Before any handler, so a signal during the loop has somewhere to write.
    WAKE.store(write, Ordering::Relaxed);

    for signal in CAUGHT {
        // SAFETY: `action` is fully initialized below, and `sigaction` writes
        // the previous disposition into `was` or returns -1.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = poke as *const () as usize;
        // The handler only writes a byte, so the interrupted work should not see `EINTR`.
        action.sa_flags = libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };

        let mut was: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, &action, &mut was) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Saved per signal so a partway failure still leaves the installed ones restorable.
        if let Ok(mut saved) = PREVIOUS.lock() {
            saved.push((signal, Disposition(was)));
        }
    }
    Ok(())
}

/// A signal's disposition before [`unmount_on_signal`], restored before the re-raise.
struct Disposition(libc::sigaction);

/// # Safety
/// A `sigaction` is a plain value (handler address, mask, flags) with nothing thread-local.
unsafe impl Send for Disposition {}

static PREVIOUS: Mutex<Vec<(libc::c_int, Disposition)>> = Mutex::new(Vec::new());

/// The handler: one async-signal-safe `write` that wakes [`watch`], since `unmount` and the
/// teardown (allocation, `fork`, `Mutex`) are not async-signal-safe.
extern "C" fn poke(signal: libc::c_int) {
    let fd = WAKE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = signal as u8;
    // Result ignored: one byte into an empty pipe cannot block, and a full pipe means the
    // watcher is already awake.
    //
    // SAFETY: `fd` is the pipe's write end, published before any handler was
    // installed and never closed.
    unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
}

/// Unmount everything, put the previous disposition back, and re-raise.
fn watch(read: libc::c_int) -> ! {
    let mut byte = 0u8;
    let signal = loop {
        // SAFETY: `read` is the pipe's read end and `byte` is one writable byte.
        let n = unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
        if n == 1 {
            break byte as libc::c_int;
        }
        // Any failure but `EINTR` means the pipe is gone and no signal will arrive; park.
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        loop {
            std::thread::park();
        }
    };

    // A copy, so no lock is held across unmounts; a guard dropping on another thread needs it.
    for mountpoint in live() {
        if !unmount_under(&mountpoint) {
            // Last chance to tell someone to clear it by hand.
            eprintln!(
                "cortex: {} did not come down on signal {signal} — unmount it by hand",
                mountpoint.display()
            );
        }
    }

    // Restore the program's dispositions and re-raise, so the process exits as it would have.
    if let Ok(saved) = PREVIOUS.lock() {
        for (signal, disposition) in saved.iter() {
            // SAFETY: `disposition` is the value `sigaction` itself wrote when
            // this handler was installed.
            unsafe { libc::sigaction(*signal, &disposition.0, std::ptr::null_mut()) };
        }
    }
    // SAFETY: no arguments to get wrong, and the disposition is now the
    // program's own.
    unsafe { libc::raise(signal) };

    // Reached only if the restored disposition does not end the process (ignored, or a
    // handler that returns); the process carries on.
    loop {
        std::thread::park();
    }
}
