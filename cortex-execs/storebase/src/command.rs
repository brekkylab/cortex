//! The shell every registered command in `cortex-execs` has around it: how a workspace path
//! becomes a file, and how clap's own output becomes an answer.
//!
//! None of this is a command. What a command *is* — which subcommands, which arguments —
//! differs in every one of these crates and belongs to each. What is here is the handful of
//! rules they all follow and were all spelling separately.

use std::path::{Path, PathBuf};

use cortex::exec::{ExecCall, ExecResult};
use cortex::fs::Mount;

/// The host path of a workspace path an argument named, or the refusal to answer with.
///
/// A name that cannot reach a file says so, rather than resolving against something else. With
/// nothing mounted there is no such file and no honest substitute for one — a substituted root
/// would read a file nobody asked about and say nothing about it.
pub fn host_path(
    call: &ExecCall,
    mount: Option<&dyn Mount>,
    arg: &str,
) -> Result<PathBuf, ExecResult> {
    let Some(mount) = mount else {
        return Err(ExecResult::failed(
            1,
            format!(
                "{}: nothing is mounted, so there is no {arg} to reach\n",
                call.name
            ),
        ));
    };
    match call.resolve(arg) {
        Ok(path) => Ok(mount.host_path(&path)),
        Err(e) => Err(ExecResult::failed(1, format!("{arg}: {e}\n"))),
    }
}

/// clap wrote the whole message, including the trailing newline; it goes back as it is.
///
/// **A clap error is not a failure by itself.** `--help` arrives as one, and
/// [`exit_code`](clap::Error::exit_code) is what already separates it (`0`) from a caller who
/// got the usage wrong (`2`) — which is a difference a caller acts on, since the second kind of
/// line can be reissued differently and the first was answered.
///
/// Help on stdout and a usage error on stderr, which is where each belongs for a caller reading
/// one out of a pipe. Nothing exits the process: an [`Executable`](cortex::exec::Executable)
/// answers with an [`ExecResult`], and whoever dispatched the call decides what to do with it.
pub fn rendered_by_clap(err: clap::Error) -> ExecResult {
    let text = err.render().to_string();
    match err.exit_code() {
        0 => ExecResult::ok(text),
        code => ExecResult::failed(code, text),
    }
}

/// A [`Command`](clap::Command) as an [`ExecCall`] needs it, whatever the command is.
///
/// Three things, none of them clap's default:
///
/// * **`no_binary_name`** — [`ExecCall::args`] is everything *after* the name, so the first
///   element is a subcommand and not a program.
/// * **the name comes at run time** — one executable can be registered under several, so the
///   usage line has to say the one it was invoked by rather than a name baked into the source.
///   `bin_name` as well as `name`, because the second is what a *subcommand's* usage is spelled
///   with. This is what clap's `string` feature is on for.
/// * **never colour** — nothing here is writing to a terminal it can see. What it produces is
///   an [`ExecResult`]'s bytes, and an escape chosen against *this* stdout would be an escape
///   in whatever the caller does with them.
pub fn as_registered(command: clap::Command, call: &ExecCall) -> clap::Command {
    command
        .name(call.name.clone())
        .bin_name(call.name.clone())
        .no_binary_name(true)
        .color(clap::ColorChoice::Never)
}

/// The tree, as a program supplies it: a directory on this host.
///
/// A [`Mount`] with nothing mounted, and what the standalone binary of each of these hands its
/// own `Executable`. What the trait offers a consumer is one fact — where the tree is on this
/// host — and when the tree already *is* a directory on this host, that fact is known before
/// anything is asked: there is no binding to make, and so nothing for a drop to take down. The
/// RAII rule holds trivially rather than being broken.
///
/// This is the whole difference between one of these run as a program and the same one invoked
/// by a consumer that has a tree mounted. There it is handed a mount over a tree it could not
/// otherwise reach; here it is handed this. The `Executable` is the same one and cannot tell
/// which it got, which is what makes running it as a program a real test of the other case.
pub struct Here(pub PathBuf);

impl Mount for Here {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}
