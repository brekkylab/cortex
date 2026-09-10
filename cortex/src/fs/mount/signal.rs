//! Taking this process's mounts down when it is killed, for the signals that can
//! be caught.
//!
//! What is taken down is [`claim::live`](super::claim::live) — the mount points
//! this process owns. That register is not kept here, because the other thing
//! that needs it runs in a *later* process; see [`claim`](super::claim).
//!
//! # Why a guard is not enough
//!
//! `Mount`'s contract is that dropping unmounts, and a signal does not drop
//! anything: the process stops between two instructions and every destructor it
//! owed goes unrun. What is left is a mount the kernel still has, with nothing
//! answering it, and a path that stops anything walking into it — `git status`
//! included. So the guard stays the whole lifecycle for every *ordinary* exit,
//! and this is the one path that has no destructor to run.
//!
//! # Opt-in, and why
//!
//! A signal disposition is process-global. A library that installed one on its
//! own would be overwriting whatever the program embedding it had arranged for
//! `SIGTERM`, from its own shutdown to a runtime's, and would do it as a side
//! effect of mounting. So nothing here happens until
//! [`unmount_on_signal`] is called by name, and what it installs chains to what
//! it replaced rather than swallowing it.
//!
//! # What a handler may do
//!
//! Only what is async-signal-safe, which `unmount` and everything the teardown
//! reaches for — allocation, `fork`, `Mutex` — is not. The handler here therefore does one `write(2)` of one byte to a pipe and
//! returns; a thread reading the other end does the work, on a normal stack with
//! nothing borrowed from the interrupted one. That thread then puts the previous
//! disposition back and re-raises, so the signal ends the process exactly as it
//! would have without any of this.
//!
//! # `SIGKILL` is not here
//!
//! No handler catches it, so a `kill -9` leaves the mount up and no amount of
//! care in this process can change that. That case is recovered by the *next*
//! run, and needs nobody to ask: see
//! [`reclaim_abandoned`](super::reclaim_abandoned), which every `try_new` calls.
//!
//! So what this module buys is not *whether* an abandoned mount is cleared but
//! *when*. Without it the mount point stays blocked until something mounts again;
//! with it, a program that is asked to stop takes its mounts with it.

use std::{
    io,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicI32, Ordering},
    },
};

use super::{claim::live, table::unmount_under};

/// The signals worth catching: the four a person or a supervisor sends to ask a
/// process to stop. Everything else that is catchable means the process is
/// already broken, and a mount is the smaller problem.
const CAUGHT: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// The write end of the self-pipe, for the handler to poke.
///
/// An atomic and not a `Mutex`, because a handler may not lock: this is read with
/// a single relaxed load, and `-1` means no handler is installed and the byte has
/// nowhere to go.
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// Take this process's mounts down when it is asked to stop.
///
/// Installs a handler for `SIGINT`, `SIGTERM`, `SIGHUP` and `SIGQUIT` that
/// unmounts everything this process has mounted, then lets the signal do what it
/// was going to do. Call it once, from a program that has decided it wants this
/// — see this module's docs for why it is not the default.
///
/// Calling it more than once is harmless and does nothing after the first.
///
/// **What it cannot do**: `SIGKILL` is uncatchable, a handler the program
/// installs afterwards replaces this one, and a mount whose server is already
/// wedged may refuse to come down — the unmount is bounded rather than
/// guaranteed, and what would not go is reported on stderr.
///
/// `Err` is the pipe, the thread, or one of the four handlers failing to be
/// installed. The handlers are installed in order, so a failure partway leaves
/// the earlier ones in place and working; they are not rolled back, because
/// putting a disposition back is the one thing that could overwrite a handler the
/// program installed in the meantime.
pub fn unmount_on_signal() -> io::Result<()> {
    static INSTALLED: OnceLock<io::Result<()>> = OnceLock::new();
    // Cloned rather than returned by reference: `io::Error` is not `Clone`, so a
    // second caller is told the kind and the message of the first failure.
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
        // Not inherited by anything this process spawns — a mount serves
        // commands, and a child holding the write end open would keep the
        // watcher's read alive past our own exit.
        //
        // SAFETY: `fd` was just returned by `pipe`.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }

    std::thread::Builder::new()
        .name("cortex-unmount-on-signal".into())
        .spawn(move || watch(read))?;

    // Published before the first handler is installed, so a signal that arrives
    // during the loop below already has somewhere to put its byte.
    WAKE.store(write, Ordering::Relaxed);

    for signal in CAUGHT {
        // SAFETY: `action` is fully initialized below, and `sigaction` writes
        // the previous disposition into `was` or returns -1.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = poke as *const () as usize;
        // Restart interrupted syscalls: this handler is a byte on a pipe and
        // says nothing about the work the interrupted thread was doing, so
        // failing that work with `EINTR` would be damage of our own making.
        action.sa_flags = libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };

        let mut was: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, &action, &mut was) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Recorded one at a time rather than all at the end, so that a failure
        // partway leaves the handlers already installed with something to
        // restore. Saving the batch afterwards would strand them: installed,
        // and with no record of what they replaced.
        if let Ok(mut saved) = PREVIOUS.lock() {
            saved.push((signal, Disposition(was)));
        }
    }
    Ok(())
}

/// What a signal's disposition was before [`unmount_on_signal`] replaced it,
/// kept so the re-raise ends the process the way the program intended.
struct Disposition(libc::sigaction);

/// # Safety
/// A `sigaction` is a plain value — a handler address, a mask and flags — with
/// nothing thread-local in it. It is only ever moved from the installing thread
/// to the watcher below, and read there.
unsafe impl Send for Disposition {}

static PREVIOUS: Mutex<Vec<(libc::c_int, Disposition)>> = Mutex::new(Vec::new());

/// The handler. One `write` and nothing else — see this module's docs.
extern "C" fn poke(signal: libc::c_int) {
    let fd = WAKE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = signal as u8;
    // A single byte into an empty pipe cannot block, and a full pipe means the
    // watcher is already awake and on its way — so the result is deliberately
    // unexamined. `write` is async-signal-safe; nothing else here would be.
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
        // `EINTR` is the only failure worth retrying. Anything else means the
        // pipe is gone and no signal will ever arrive, so there is nothing left
        // for this thread to do; parking is cheaper than spinning on it.
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        loop {
            std::thread::park();
        }
    };

    // A copy, so no lock is held across the unmounts — one of which spawns a
    // child and waits on it, while a guard being dropped on another thread wants
    // that same lock to give its mount point up.
    for mountpoint in live() {
        if !unmount_under(&mountpoint) {
            // Not silent: a mount that would not come down is one somebody has
            // to clear by hand, and this is the last moment anything in this
            // process can say so.
            eprintln!(
                "cortex: {} did not come down on signal {signal} — unmount it by hand",
                mountpoint.display()
            );
        }
    }

    // Back to whatever the program had arranged, then let the signal land. The
    // process ends here, with the status it would have had.
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

    // Reached only if the restored disposition ignores the signal, in which case
    // the process is meant to carry on — but this thread has spent its pipe, so
    // it has nothing left to watch.
    loop {
        std::thread::park();
    }
}
