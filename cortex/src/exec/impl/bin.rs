//! A program on this host, offered as a delegated name.

use std::{collections::BTreeMap, io, os::unix::process::ExitStatusExt as _, process::Stdio};

use tokio::io::AsyncReadExt as _;

use crate::{
    BoxFuture,
    exec::{ExecCall, ExecResult, Executable},
    fs::Mount,
};

/// How many bytes of each stream a call keeps.
///
/// [`ExecResult`] is two `Vec<u8>`s and there is no chunking under it, so whatever a program
/// writes is held whole in this process before any of it moves. A cap is therefore the
/// difference between a chatty program and an allocation the size of its output.
///
/// Fixed rather than a knob, because it is not a decision a registration has to make. What
/// crosses here goes on to a console channel and into whatever is reading the output, and this
/// much of it is already far past anything a delegated call should be answering with. A program
/// with more than this to say should write it into the tree it was handed and answer with the
/// path — the tree is mounted, which is the whole reason a delegated name gets one.
const OUTPUT_LIMIT: usize = 8 << 20;

/// A program that could not be found, which is what a shell reports 127 for.
const NOT_FOUND: i32 = 127;

/// A program that was found and could not be started — 126, the same code the shim uses for
/// the same condition one layer up.
const NOT_EXECUTABLE: i32 = 126;

/// A call that could not be made at all: no tree to run in, or nowhere in it to stand.
const REFUSED: i32 = 1;

/// A call that did not become a program, as the answer a caller gets: the reason on stderr,
/// nothing on stdout, `code` to exit with.
///
/// The one place a reason is formatted, which is what keeps every one of them one
/// newline-terminated line however many refusals there come to be — a reason that ran into the
/// next thing on the terminal would read as part of it. Nothing else in here builds a failing
/// [`ExecResult`].
///
/// Three codes reach this, and they say different things. [`NOT_FOUND`] and [`NOT_EXECUTABLE`]
/// are the shell's, so a caller that knows the convention reads them without being told.
/// [`REFUSED`] is this end refusing the call, which is not a fact about the program at all.
fn refused(code: i32, reason: impl Into<String>) -> ExecResult {
    ExecResult::failed(code, format!("{}\n", reason.into().trim_end()))
}

/// The environment a child is given: what the caller had, minus what it may not hand over, over
/// this host's own answers for the few names it has to supply, with `cwd` as `PWD`.
///
/// Built from empty rather than from this process's environment. A host that passed its own down
/// would hand a spawned program whatever secrets the process was started with, and there is
/// nothing in it a caller's own variable could not name explicitly.
///
/// # What a caller may not hand over
///
/// The list is below, at the point that reads it, and a trailing `*` there is a prefix. Two
/// reasons put a name on it, and they are the only two:
///
/// * **This host has to answer it instead.** A process *has* a working directory and mostly does
///   not work without a `HOME` or a `TMPDIR`, and the caller's answers describe another machine —
///   a guest's `/root` is not here, and its `$TMPDIR` is a directory this host never had.
///
///   `PATH` is the one with teeth. A console server puts a directory of shim symlinks on the
///   `PATH` of everything an execution spawns, so a child given the caller's `PATH` could call a
///   delegated name — and delegated calls are served one at a time, so that does not contend, it
///   deadlocks.
///
/// * **It decides what code the program runs.** The dynamic loaders: `LD_PRELOAD` and
///   `DYLD_INSERT_LIBRARIES` are injection into a process this side spawned with nothing else
///   added, and the rest of each family redirects the library search, which is the same thing
///   with one more step. Prefixes, because both families keep growing and a new member should be
///   denied before anybody here has heard of it. That setuid binaries ignore `LD_*`, and that SIP
///   strips `DYLD_*` from platform binaries, defends neither case: what is spawned is an ordinary
///   unsigned program.
///
/// Everything else is forwarded, and [`BinExecutable`] says what that leaves open. The last word
/// is nonetheless a registration's, and it is not spoken here: a `cmd` beginning
/// `["env", "-u", "http_proxy", "VAR=ours", …]` sets and unsets whatever it likes, because by then
/// the program being spawned is `env` and this is the environment `env` was given.
///
/// # `cwd`
///
/// `None` when the directory the child was given has no `String` form, and then there is no
/// `PWD`. A lossy spelling would name a directory the process is not in, which is the one thing
/// this variable exists to get right.
fn environment(caller: &BTreeMap<String, String>, cwd: Option<&str>) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    // This host's answers first, so these are filled even though the loop below can never reach
    // them. Absent here is absent for the child: a variable nobody set is not one to invent.
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name).and_then(|v| v.into_string().ok()) {
            env.insert(name.to_string(), value);
        }
    }
    // The one host-owned name whose value is neither the caller's nor this host's: it is where the
    // child was actually put.
    if let Some(cwd) = cwd {
        env.insert("PWD".to_string(), cwd.to_string());
    }

    // Then the caller's own, minus the list and minus anything that is not an identifier at all.
    // That second rule is the one here closed under whatever arrives next: an environment is a
    // list of `NAME=VALUE` strings with no escaping, so a name holding `=`, a NUL or a `%` is one
    // nothing downstream agrees about — `BASH_FUNC_x%%`, a shell function smuggled through the
    // environment, is exactly that shape — and refusing the shape refuses the family without
    // needing an entry for it.
    //
    // The four names above appear here again, and that is not a duplicate to be factored out:
    // "cannot come from the caller" and "this host must supply it" are different questions, and a
    // name added here later — `SSH_AUTH_SOCK`, say — is not thereby a name this host should answer
    // with its own value. Denying one is an entry here; supplying one is a line up there.
    for (name, value) in caller {
        let mut chars = name.chars();
        let plain = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        let denied = ["PATH", "HOME", "TMPDIR", "PWD", "LD_*", "DYLD_*"]
            .iter()
            .any(|rule| match rule.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => *rule == name.as_str(),
            });
        if plain && !denied {
            env.insert(name.clone(), value.clone());
        }
    }

    env.into_iter().collect()
}

/// A real program, run on this host when a delegated name is called.
///
/// The name is registered like any other, and a caller cannot tell the difference: arguments
/// arrive as `argv`, the program's own stdout and stderr go back as this call's, and its exit
/// status is the call's. What makes it different from an [`Executable`] written in Rust is that
/// a second process ends up holding the call's `cwd` and environment, and both of those are
/// facts about a machine that is not the one the calling command ran on. Reconciling that is
/// most of what this type does.
///
/// # Why this is not a sandbox, and what that means for registering one
///
/// A path argument reaches the program verbatim, and the program's own kernel resolves it. So
/// the containment [`resolve_under`](crate::exec::resolve_under) provides — a workspace-relative
/// name that cannot leave the root — **does not extend past this point**. `../../etc/passwd` from
/// the child's working directory is the host's file, and so is `/etc/passwd`; nothing here can
/// tell which of a program's arguments were meant as paths, so nothing here can translate them.
///
/// That is worth stating plainly rather than mitigating badly: **registering one of these grants
/// whoever can call the name the ability to run a program on this host, with this process's
/// filesystem access.** For the host-local backend that is roughly what the caller already had.
/// For a backend whose executions run in a micro-VM it is not — a delegated name is the one
/// thing in that arrangement which runs outside the guest, and this is the kind of delegated
/// name where that matters. A deployment that needs the child confined confines it with
/// something built for it (a `sandbox-exec` profile, a Landlock ruleset, a namespace) and not
/// with argument rewriting.
///
/// What *is* honest about it is the reason to reach for this at all: the program exists on this
/// host and not in the execution's world. A licensed binary, a model runner, a tool holding
/// credentials the guest must never see. A program the execution could have run itself is one
/// it should run itself.
///
/// # The environment is filtered, not applied
///
/// [`ExecCall::env`] is a report — what the invoking command had, on the executor's machine —
/// and applying it wholesale would put another machine's `PATH`, `HOME` and dynamic loader
/// settings on a host process. So the default is to forward, minus two things: the names this
/// host has to answer itself and the dynamic loaders — [`environment`] is the whole of it.
/// Forwarding is the default because it is what makes a delegated name behave like a program on
/// `PATH` rather than almost like one — `FOO=bar` reaches the program, and so does whatever
/// vocabulary that particular tool has, which no table here could have known to list.
///
/// **What that leaves undecided is most of the space.** A variable that redirects a program
/// rather than injecting into it is forwarded as it stands: `GIT_SSH_COMMAND`, `NODE_OPTIONS`,
/// `PYTHONPATH`, `http_proxy`, `SSL_CERT_FILE`, `KUBECONFIG`. Each is a way for a caller to
/// change what the program runs, reads or trusts, and several are equivalent to `LD_PRELOAD`
/// when the program is an interpreter. They are not denied here because whether denying them is
/// right depends on what was registered — a proxy is an attack on one deployment and the whole
/// point on another — and a table guessing per deployment would be wrong more often than the
/// gap is. A registration that cares says so in its own `cmd`: a prefix of
/// `["env", "-u", "http_proxy", …]` unsets what arrived, and `["env", "VAR=ours", …]` replaces
/// it.
///
/// Which means the environment is *not* the boundary to lean on. The one below is.
///
/// # No input, whole output
///
/// stdin is `/dev/null`. The protocol carries no input to a delegated call, and that is load-
/// bearing rather than unfinished: delegated calls are served one at a time, which is safe
/// *because* none of them is waiting on another's output. A program that reads stdin sees EOF,
/// and an interactive one cannot be delegated at all.
///
/// Output is collected whole and capped at [`OUTPUT_LIMIT`]. Truncation is
/// reported on stderr, because an [`Executable`] has no other channel for it — the `truncated`
/// flag on the wire belongs to an execution's own output and is not reachable from here.
///
/// # What it needs of the runtime
///
/// A Tokio runtime with the I/O driver enabled. A child's pipes are registered with it and its
/// exit is delivered through `SIGCHLD`, so on a runtime built without I/O — `new_current_thread`
/// with nothing enabled — a spawned program never appears to finish.
///
/// # Who ends a call
///
/// Not this. An [`Executable`] that never answers is not a thing only a spawned program can be —
/// one that waits on a model or a service is the same shape — so the deadline belongs to the
/// layer that invokes them.
///
/// What this type owes that layer is to be **safe to give up on**, and it is. Dropping the future
/// kills the child's whole process group and the reads along with it, so a deadline applied from
/// outside is enough on its own: nothing is left running and no descriptor is left held.
/// [`ExecResult::timed_out`] is never set here, there being nothing here that could know.
///
/// # Lifetime of the child
///
/// The child is made a process-group leader and the group is what gets killed, so a program that
/// forked does not leave its children behind. That is also why the group is new: killing the
/// group this process is in would kill this process. The cost is that the child no longer shares
/// this process's group, so nothing signals it on our behalf — if this process is killed
/// outright, the guard that kills the group does not run.
pub struct BinExecutable {
    /// The program, then the arguments that precede the call's own. Never empty.
    cmd: Vec<String>,
}

impl BinExecutable {
    /// The program `cmd` names, with the rest of `cmd` as arguments before every call's own.
    ///
    /// A prefix rather than only a program, because that is how a delegated name is usually a
    /// *mode* of one: `["python", "-m", "mytool"]`, `["rg", "--json"]`. A call's own arguments
    /// are appended, so a caller cannot displace the prefix.
    ///
    /// It is also the whole of this type's configuration: what a knob here would set, a program
    /// on the host sets better.
    ///
    /// ```ignore
    /// // Give the program a variable the filter denies, and take away one it forwards.
    /// BinExecutable::new([
    ///     "env", "-u", "http_proxy", "LD_LIBRARY_PATH=/opt/tool/lib", "/opt/tool/bin/mytool",
    /// ]);
    /// // A name that means one mode of a multi-call binary — one registration per name.
    /// BinExecutable::new(["busybox", "ls"]);
    ///
    /// BinExecutable::new([
    ///     "mem"
    /// ]);
    /// ```
    ///
    /// The chain resolves the way it reads: `env` is what this spawns and what the environment
    /// below is built for, and `env` is what finds and execs the program after it — exiting 127
    /// itself when there is none, which is the code this type would have answered anyway.
    ///
    /// A first word with no `/` in it is looked up on a `PATH`, by `execvp` as for any other
    /// program — and the `PATH` it looks on is **this host's**, never the caller's, because that
    /// is the one the environment below is built with. A registration that wants no search spells
    /// a path.
    ///
    /// # Panics
    ///
    /// If `cmd` is empty. There is no program in that, and no call at which the mistake would
    /// become anything other than what it already is — a wiring constant that names nothing.
    pub fn new(cmd: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let cmd: Vec<String> = cmd.into_iter().map(Into::into).collect();
        assert!(
            !cmd.is_empty(),
            "a BinExecutable needs a program to run; `cmd` was empty"
        );
        BinExecutable { cmd }
    }
}

/// A real program, answering on stdout and stderr as itself.
impl Executable for BinExecutable {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            // A refusal is already the answer, so the block below is `Result<ExecResult,
            // ExecResult>`: the eight ways a call can fail stay `?`s and early returns, and what
            // a reason *looks like* is `refused`'s business rather than each of theirs.
            let attempted = async move {
            // A program runs *somewhere*, and unlike an executable that only reads files there is no
            // version of this with no directory: `current_dir` is not optional to a process. Both
            // refusals below are therefore refusals of the call, not of one argument in it.
            let Some(mount) = mount else {
                return Err(refused(
                    REFUSED,
                    format!(
                        "{}: nothing is mounted, so there is no directory to run in",
                        call.name
                    ),
                ));
            };
            if call.cwd.is_none() {
                return Err(refused(
                    REFUSED,
                    format!(
                        "{}: the calling command's directory is not a place in this tree, so there \
                         is nowhere to run",
                        call.name
                    ),
                ));
            }
            let workspace_cwd = call
                .resolve(".")
                .map_err(|e| refused(REFUSED, format!("{}: {e}", call.name)))?;
            let cwd = mount.host_path(&workspace_cwd);
            // Asked before spawning, because `current_dir` on a directory that is not there fails
            // the *spawn* with `ENOENT` — indistinguishable, in the answer, from the program being
            // missing.
            if !cwd.is_dir() {
                return Err(refused(
                    REFUSED,
                    format!(
                        "{}: {} is not a directory in this tree",
                        call.name,
                        workspace_cwd.display()
                    ),
                ));
            }

            let mut cmd = tokio::process::Command::new(&self.cmd[0]);
            cmd.args(&self.cmd[1..])
                .args(&call.args)
                .current_dir(&cwd)
                // Cleared and rebuilt, never inherited: see the type's docs and `super::env`.
                .env_clear()
                .envs(environment(&call.env, cwd.to_str()))
                // No input to give — the protocol carries none — and a program left with this
                // process's stdin would read whatever the console channel is on.
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // A backstop for the future being dropped rather than the normal path: it kills the
                // leader only, which is why the group guard below exists as well.
                .kill_on_drop(true)
                // Its own group, so the group can be killed without killing this process.
                .process_group(0);

            // The one place a program's absence is reported, and the kind is what says which
            // absence it was: `execvp` looked, and it knows things a check here would not — which
            // of the mode bits apply to this process, what the ACL says, whether the filesystem
            // is mounted `noexec`. So the answer is its answer, in the codes a shell would use
            // for it.
            let mut child = cmd.spawn().map_err(|e| {
                let (code, said) = match e.kind() {
                    io::ErrorKind::NotFound => (NOT_FOUND, "not found".to_string()),
                    io::ErrorKind::PermissionDenied => (NOT_EXECUTABLE, "not executable".to_string()),
                    _ => (NOT_EXECUTABLE, e.to_string()),
                };
                refused(code, format!("{}: {}: {said}", call.name, self.cmd[0]))
            })?;
            // Armed until the child has been reaped. A group id is only ours while a member of it
            // exists, so disarming after the wait is what keeps this from ever signalling a group
            // that has since been given to somebody else.
            let mut group = GroupKill(child.id());

            // Both pipes drained while the exit is awaited, because a program that fills one while
            // this end reads the other would block in `write` and never reach its own exit — and
            // concurrently with each other, for the same reason.
            //
            // One future rather than spawned tasks, so that dropping this call drops the reads and
            // the wait together and leaves nothing running that holds a descriptor. See the type's
            // docs on who ends a call that does not end.
            //
            // Bound to a `let` so the borrows of `child` and the two pipes end with the statement:
            // the failure arm below needs the child back.
            let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
            let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
            let joined = tokio::try_join!(
                drain(&mut stdout_pipe, OUTPUT_LIMIT),
                drain(&mut stderr_pipe, OUTPUT_LIMIT),
                child.wait(),
            );

            let ((stdout, stdout_truncated), (mut stderr, stderr_truncated), status) = match joined {
                Ok(joined) => joined,
                // Returned without touching the child: `group` is still armed and drops first,
                // so the group goes down while the child is unreaped and the id is still this
                // call's, and `kill_on_drop` reaps the leader after it. The same two steps a
                // caller giving up on the call takes, in the same order.
                Err(e) => return Err(refused(REFUSED, format!("{}: running it: {e}", call.name))),
            };
            group.disarm();

            // Ours, not the program's, and on stderr in both cases: stdout is on its way to a
            // caller's pipe and has to be exactly what was written.
            for (stream, truncated) in [("stdout", stdout_truncated), ("stderr", stderr_truncated)] {
                if truncated {
                    stderr.extend_from_slice(
                        format!(
                            "{}: {stream} over {} bytes was truncated\n",
                            call.name, OUTPUT_LIMIT
                        )
                        .as_bytes(),
                    );
                }
            }

            Ok(ExecResult {
                stdout,
                stderr,
                // A child killed by a signal has no exit code of its own, and `0` would say it
                // succeeded. `128 + signo` is the shell's spelling for the same fact, so a caller
                // reading 137 knows what happened.
                exit_code: status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                timed_out: false,
            })
            }
            .await;

            // Either way it is what the caller asked for.
            match attempted {
                Ok(result) | Err(result) => result,
            }
        })
    }
}

/// Read `src` to its end, keeping at most `cap` bytes in `into`. `true` if anything was dropped.
///
/// Draining past the cap rather than stopping at it, because a pipe nobody reads blocks the
/// program writing to it: stopping would turn a program that says too much into one that never
/// finishes, rather than one that stops where the cap is and never reaches its own exit.
async fn drain(
    src: &mut (impl tokio::io::AsyncRead + Unpin),
    cap: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = src.read(&mut buf).await?;
        if read == 0 {
            return Ok((kept, truncated));
        }
        let room = cap.saturating_sub(kept.len());
        let take = room.min(read);
        kept.extend_from_slice(&buf[..take]);
        truncated |= take < read;
    }
}

/// Kills a child's process group unless the child was reaped first.
///
/// The group and not the process, because a program that forked would otherwise leave its
/// children behind — which also means the reads above would never see the end of the pipes those
/// children hold. `kill_on_drop` covers the leader and only the leader.
///
/// This is what makes the whole call safe to give up on, and covers every path that is not a
/// return as well: the future dropped because a caller stopped waiting, or a panic between the
/// spawn and the wait.
///
/// Declared after the child, so it drops before it — while the child is still unreaped and the
/// group id is therefore still this call's rather than one the kernel has since handed on.
struct GroupKill(Option<u32>);

impl GroupKill {
    /// The child has been reaped, so the group may now be empty and its id may since have been
    /// given to somebody else's. Not ours to signal any more.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        let Ok(pid) = i32::try_from(pid) else { return };
        // SAFETY: `kill` with a negative pid signals a process group and touches nothing else.
        // The group is one this process created with `process_group(0)`, and its leader has not
        // been reaped — see the note on the field — so the id still names that group and cannot
        // yet have been reused.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        time::Duration,
    };

    use super::*;

    /// A directory standing in for a mounted tree. What is under test is what a spawned program
    /// is given for a path a mount reported, and a plain directory reports one the same way.
    struct Mounted(PathBuf);

    impl Mount for Mounted {
        fn mountpoint(&self) -> &Path {
            &self.0
        }
    }

    fn mounted() -> (tempfile::TempDir, Mounted) {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, because `$TMPDIR` on macOS is under a `/var` that is a symlink and a child's
        // `getcwd` answers the resolved name. The test compares the two textually.
        let path = dir.path().canonicalize().unwrap();
        (dir, Mounted(path))
    }

    fn call(args: &[&str], env: &[(&str, &str)]) -> ExecCall {
        ExecCall {
            name: "tool".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: Some(String::new()),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// A shell, so a test can name what to run without shipping a fixture. `/bin/sh` is the one
    /// program both platforms this builds for have at a fixed path.
    fn sh(script: &str) -> BinExecutable {
        BinExecutable::new(["/bin/sh", "-c", script])
    }

    async fn run(exec: &BinExecutable, mount: &Mounted, call: &ExecCall) -> ExecResult {
        exec.exec(call, Some(mount)).await
    }

    /// stdout as text, having asserted the program succeeded.
    async fn out(exec: &BinExecutable, mount: &Mounted, call: &ExecCall) -> String {
        let result = run(exec, mount, call).await;
        assert_eq!(
            result.exit_code,
            0,
            "failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).expect("text")
    }

    /// The child's environment, as it saw it.
    async fn child_env(
        exec: &BinExecutable,
        mount: &Mounted,
        call: &ExecCall,
    ) -> BTreeMap<String, String> {
        // `env` rather than a shell builtin: what is under test is the environment the *process*
        // was given, and a builtin would report the shell's own view of it.
        let printed = out(exec, mount, call).await;
        printed
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn env_exec() -> BinExecutable {
        sh("exec env")
    }

    /// A shell that takes arguments. `sh -c SCRIPT` reads the word after the script as `$0`, so
    /// a prefix that wants a call's arguments as `$1` onwards has to spell that word itself.
    fn sh_with_args(script: &str) -> BinExecutable {
        BinExecutable::new(["/bin/sh", "-c", script, "tool"])
    }

    #[tokio::test]
    async fn the_program_runs_and_its_output_comes_back() {
        let (_dir, mount) = mounted();
        let said = out(
            &sh_with_args("printf 'hi %s' \"$1\""),
            &mount,
            &call(&["there"], &[]),
        )
        .await;
        assert_eq!(said, "hi there");
    }

    /// The prefix is the registration's and a call's arguments come after it, so a caller cannot
    /// displace what the name was registered to mean.
    #[tokio::test]
    async fn a_calls_arguments_follow_the_registered_prefix() {
        let (_dir, mount) = mounted();
        let said = out(
            &sh_with_args("echo \"$#\" \"$1\""),
            &mount,
            &call(&["a", "b"], &[]),
        )
        .await;
        assert_eq!(said.trim(), "2 a");
    }

    /// The chain the mount exists for: where the calling command stood is where the program
    /// stands, on this host.
    #[tokio::test]
    async fn the_program_stands_where_the_calling_command_stood() {
        let (_dir, mount) = mounted();
        std::fs::create_dir(mount.0.join("work")).unwrap();

        let mut c = call(&[], &[]);
        c.cwd = Some("work".into());
        let said = out(&sh("pwd -P"), &mount, &c).await;
        assert_eq!(
            Path::new(said.trim()),
            mount.0.join("work"),
            "the child's own directory is the one the mount reports"
        );
    }

    /// The deadlock case, end to end: a caller's `PATH` names a console server's shim directory,
    /// and a child given it could re-enter a console that serves delegated calls in turn.
    #[tokio::test]
    async fn the_childs_path_is_this_hosts_and_not_the_callers() {
        let (_dir, mount) = mounted();
        let env = child_env(&env_exec(), &mount, &call(&[], &[("PATH", "/guest/bin")])).await;
        assert_eq!(env.get("PATH").cloned(), std::env::var("PATH").ok());
    }

    /// Through a real spawn rather than only through the filter: what matters is that the
    /// *process* never had them.
    #[tokio::test]
    async fn the_loaders_do_not_reach_the_program() {
        let (_dir, mount) = mounted();
        let hostile = [
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("LD_LIBRARY_PATH", "/tmp"),
            ("LD_AUDIT", "/tmp/evil.so"),
            ("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib"),
            ("DYLD_LIBRARY_PATH", "/tmp"),
        ];
        let env = child_env(&env_exec(), &mount, &call(&[], &hostile)).await;
        for (name, _) in hostile {
            assert!(!env.contains_key(name), "{name} reached the program");
        }
    }

    /// The reason a caller's environment is carried at all: a variable nothing here has heard
    /// of belongs to the tool, and reaches it.
    #[tokio::test]
    async fn a_variable_the_filter_does_not_name_reaches_the_program() {
        let (_dir, mount) = mounted();
        let env = child_env(
            &env_exec(),
            &mount,
            &call(&[], &[("FOO", "bar"), ("MYTOOL_TOKEN", "t")]),
        )
        .await;
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.get("MYTOOL_TOKEN").map(String::as_str), Some("t"));
    }

    /// **What this filter deliberately does not decide**, asserted so that deciding it later is
    /// a change to this test rather than a surprise.
    ///
    /// Every one of these lets a caller change what the program runs, reads or trusts, and the
    /// interpreter options are `LD_PRELOAD` again whenever the program is an interpreter. They
    /// are forwarded because whether that is wrong depends on what was registered, and a
    /// registration that cares sets its own value with `env`.
    #[tokio::test]
    async fn what_the_filter_leaves_open_is_forwarded() {
        let (_dir, mount) = mounted();
        let open = [
            ("PYTHONPATH", "/tmp"),
            ("NODE_OPTIONS", "--require /tmp/x.js"),
            ("GIT_SSH_COMMAND", "/tmp/ssh"),
            ("http_proxy", "http://nope:8080"),
            ("SSL_CERT_FILE", "/tmp/ca.pem"),
        ];
        let env = child_env(&env_exec(), &mount, &call(&[], &open)).await;
        for (name, value) in open {
            assert_eq!(env.get(name).map(String::as_str), Some(value));
        }
    }

    /// A registration's last word over the environment, and it needs no knob here: by the time
    /// the assignments are read the program being spawned is `env`, and what this type built is
    /// the environment `env` was handed.
    ///
    /// Both directions in one call — a denied variable given a value, and a forwarded one taken
    /// away. `-u` is why this is not a poorer substitute for a setter but a better one: a setter
    /// could only have made `http_proxy` empty, where this leaves the program without it.
    #[tokio::test]
    async fn a_registration_has_the_last_word_through_its_own_cmd() {
        let (_dir, mount) = mounted();
        let exec = BinExecutable::new([
            "/usr/bin/env",
            "-u",
            "http_proxy",
            "LD_LIBRARY_PATH=/opt/tool/lib",
            "/usr/bin/env",
        ]);
        let sent = [
            ("LD_LIBRARY_PATH", "/tmp/theirs"),
            ("http_proxy", "http://nope"),
            ("FOO", "bar"),
        ];

        let env = child_env(&exec, &mount, &call(&[], &sent)).await;
        assert_eq!(
            env.get("LD_LIBRARY_PATH").map(String::as_str),
            Some("/opt/tool/lib")
        );
        assert!(!env.contains_key("http_proxy"));
        // Everything else still crosses, which is the point of the prefix being a prefix.
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
    }

    /// `HOME` and `TMPDIR` are filled rather than dropped: a program with neither mostly does
    /// not work, and the executor's answers name directories that are not on this machine.
    #[tokio::test]
    async fn the_host_answers_for_the_names_that_describe_the_machine() {
        let (_dir, mount) = mounted();
        let sent = [("HOME", "/root"), ("TMPDIR", "/guest/tmp")];

        let env = child_env(&env_exec(), &mount, &call(&[], &sent)).await;
        assert_eq!(env.get("HOME").cloned(), std::env::var("HOME").ok());
        assert_ne!(env.get("TMPDIR").map(String::as_str), Some("/guest/tmp"));
    }

    /// `PWD` is where the child actually is, because `pwd` and anything else that trusts it
    /// over `getcwd` would otherwise name a directory the process is not in.
    #[tokio::test]
    async fn pwd_is_the_directory_the_child_was_given() {
        let (_dir, mount) = mounted();
        std::fs::create_dir(mount.0.join("work")).unwrap();
        let mut c = call(&[], &[("PWD", "/guest/work")]);
        c.cwd = Some("work".into());

        let env = child_env(&env_exec(), &mount, &c).await;
        assert_eq!(
            env.get("PWD").map(|p| Path::new(p).to_path_buf()),
            Some(mount.0.join("work"))
        );
    }

    /// The shape rule, which is what makes a shell function smuggled through the environment a
    /// non-question rather than a row in the table.
    #[test]
    fn a_name_that_is_not_an_identifier_is_not_forwarded() {
        let sent: BTreeMap<String, String> = [
            ("BASH_FUNC_x%%", "() { :; }"),
            ("a=b", "c"),
            ("2FOO", "x"),
            ("", "x"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let built = environment(&sent, Some("/mnt"));
        for (name, _) in &built {
            assert!(!sent.contains_key(name), "{name} was forwarded");
        }
    }

    /// Built from empty, so a child's environment holds nothing but what a caller sent and the
    /// few denied names this host has to supply — never anything this process was started with.
    #[test]
    fn nothing_of_this_process_leaks_into_a_childs_environment() {
        // Safety: single-threaded test, and the name is one nothing else reads.
        unsafe { std::env::set_var("CORTEX_BIN_EXEC_SECRET", "s3cret") };
        let built = environment(&BTreeMap::new(), Some("/mnt"));
        unsafe { std::env::remove_var("CORTEX_BIN_EXEC_SECRET") };

        for (name, _) in &built {
            assert!(
                ["PATH", "HOME", "TMPDIR", "PWD"].contains(&name.as_str()),
                "{name} came from nowhere"
            );
        }
    }

    /// A killed program has no exit code, and `0` would say it succeeded.
    #[tokio::test]
    async fn a_signal_is_reported_as_the_shell_reports_one() {
        let (_dir, mount) = mounted();
        let result = run(&sh("kill -TERM $$"), &mount, &call(&[], &[])).await;
        assert_eq!(result.exit_code, 128 + libc::SIGTERM);
        assert!(!result.timed_out);
    }

    /// Both streams go back, and separately.
    #[tokio::test]
    async fn stderr_comes_back_as_stderr() {
        let (_dir, mount) = mounted();
        let result = run(&sh("echo out; echo err >&2"), &mount, &call(&[], &[])).await;
        assert_eq!(result.stdout, b"out\n");
        assert_eq!(result.stderr, b"err\n");
    }

    /// The protocol carries no input to a delegated call, so a program that reads sees the end
    /// of it — not this process's stdin, which is whatever the console channel is on.
    #[tokio::test]
    async fn a_program_that_reads_gets_nothing_and_finishes() {
        let (_dir, mount) = mounted();
        let result = run(&sh("cat"), &mount, &call(&[], &[])).await;
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.is_empty());
    }

    /// Kept bytes are exact, the note about the rest is on stderr — stdout is on its way to a
    /// caller's pipe — and the program still finishes.
    ///
    /// That last part is what the cap costs to get right. A drain that stopped reading at the cap
    /// would leave the program blocked in `write` on a pipe nobody empties, and the call would
    /// never answer. Well over a pipe buffer for that reason, and under an outer deadline so that
    /// getting it wrong fails the test instead of hanging it.
    #[tokio::test]
    async fn output_over_the_limit_is_cut_and_says_so() {
        let (_dir, mount) = mounted();
        let exec = sh(&format!(
            "yes aaaaaaaaaaaaaaaa | head -c {}",
            OUTPUT_LIMIT + 1024
        ));
        let result =
            tokio::time::timeout(Duration::from_secs(30), run(&exec, &mount, &call(&[], &[])))
                .await
                .expect("the drain kept reading past the cap");

        assert_eq!(result.exit_code, 0);
        assert!(!result.timed_out);
        assert_eq!(result.stdout.len(), OUTPUT_LIMIT);
        assert!(result.stdout.iter().all(|b| *b == b'a' || *b == b'\n'));
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("truncated"),
            "{:?}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    /// What a deadline applied from outside has to be able to rely on: giving up on the call
    /// takes the program **and everything it forked**, so nothing is left running on this host
    /// and no descriptor is left held.
    ///
    /// The group and not the leader, which is the whole reason the child gets a group of its own.
    #[tokio::test]
    async fn giving_up_on_a_call_takes_the_whole_process_group() {
        let (_dir, mount) = mounted();
        let marker = mount.0.join("still-running");
        // A grandchild that outlives its parent and keeps touching a file, under a parent that
        // never exits. Killing only the leader would leave it running.
        let script = format!(
            "( while :; do touch {}; sleep 0.05; done ) & sleep 30",
            marker.display()
        );
        let exec = BinExecutable::new(["/bin/sh", "-c", &script]);

        // Exactly what a deadline at the invoking layer does: drop the future.
        let c = call(&[], &[]);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), exec.exec(&c, Some(&mount)))
                .await
                .is_err()
        );
        assert!(marker.exists(), "the grandchild ran at all");

        std::fs::remove_file(&marker).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !marker.exists(),
            "the grandchild outlived the call that was given up on"
        );
    }

    /// 127 and 126 are the shell's codes for these, so a caller that knows the convention reads
    /// the answer without being told.
    #[tokio::test]
    async fn a_program_that_is_not_there_says_so() {
        let (_dir, mount) = mounted();
        let result = run(
            &BinExecutable::new(["/nonexistent/tool"]),
            &mount,
            &call(&[], &[]),
        )
        .await;
        assert_eq!(result.exit_code, NOT_FOUND);
        assert!(String::from_utf8_lossy(&result.stderr).contains("not found"));

        let plain = run(
            &BinExecutable::new(["cortex-no-such-program"]),
            &mount,
            &call(&[], &[]),
        )
        .await;
        assert_eq!(plain.exit_code, NOT_FOUND);
    }

    #[tokio::test]
    async fn a_file_that_is_not_executable_is_a_different_answer() {
        let (_dir, mount) = mounted();
        let path = mount.0.join("not-a-program");
        std::fs::write(&path, "data").unwrap();

        let result = run(
            &BinExecutable::new([path.to_str().unwrap()]),
            &mount,
            &call(&[], &[]),
        )
        .await;
        assert_eq!(result.exit_code, NOT_EXECUTABLE);
        assert!(String::from_utf8_lossy(&result.stderr).contains("not executable"));
    }

    /// A program runs *somewhere*, so unlike an executable that only reads files there is no
    /// version of this call with no directory.
    #[tokio::test]
    async fn with_nothing_mounted_the_call_is_refused() {
        let result = sh("pwd").exec(&call(&[], &[]), None).await;
        assert_eq!(result.exit_code, REFUSED);
        assert!(String::from_utf8_lossy(&result.stderr).contains("nothing is mounted"));
        assert!(result.stdout.is_empty());
    }

    #[tokio::test]
    async fn with_nowhere_to_stand_the_call_is_refused() {
        let (_dir, mount) = mounted();
        let mut c = call(&[], &[]);
        c.cwd = None;
        let result = run(&sh("pwd"), &mount, &c).await;
        assert_eq!(result.exit_code, REFUSED);
        assert!(String::from_utf8_lossy(&result.stderr).contains("nowhere to run"));
    }

    /// A directory the tree does not have is not the program being missing, and the answer says
    /// which it was.
    #[tokio::test]
    async fn a_directory_that_is_not_there_is_refused_before_the_spawn() {
        let (_dir, mount) = mounted();
        let mut c = call(&[], &[]);
        c.cwd = Some("absent".into());
        let result = run(&sh("pwd"), &mount, &c).await;
        assert_eq!(result.exit_code, REFUSED);
        assert!(String::from_utf8_lossy(&result.stderr).contains("not a directory"));
    }

    /// Every reason ends its line, whatever produced it.
    #[tokio::test]
    async fn a_reason_is_one_terminated_line() {
        let (_dir, mount) = mounted();
        let mut nowhere = call(&[], &[]);
        nowhere.cwd = None;

        for result in [
            sh("pwd").exec(&call(&[], &[]), None).await,
            run(&sh("pwd"), &mount, &nowhere).await,
            run(
                &BinExecutable::new(["/nonexistent/tool"]),
                &mount,
                &call(&[], &[]),
            )
            .await,
        ] {
            let stderr = String::from_utf8(result.stderr).unwrap();
            assert!(
                stderr.ends_with('\n') && !stderr.ends_with("\n\n"),
                "{stderr:?}"
            );
        }
    }

    /// `argv[0]` is the program, which is what a program run from a shell sees — and a
    /// registration that wants a name to mean a *mode* of one varies the prefix instead of
    /// `argv[0]`, one registration per name.
    #[tokio::test]
    async fn argv_zero_is_the_program() {
        let (_dir, mount) = mounted();
        let said = out(&sh("echo \"$0\""), &mount, &call(&[], &[])).await;
        assert_eq!(said.trim(), "/bin/sh");
    }

    #[test]
    #[should_panic(expected = "needs a program to run")]
    fn a_registration_with_no_program_is_a_wiring_mistake() {
        BinExecutable::new(Vec::<String>::new());
    }
}
