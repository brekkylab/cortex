//! `search` against real services, with no console and nothing mounted on the host.
//!
//! The same [`Executable`] a console reaches, over a [`WorkFs`] assembled the way a session
//! assembles one — so what it answers here is what it answers there. The difference is only
//! that nothing puts this workspace in front of a kernel, which costs nothing: `search` names
//! files rather than reading them, so it needs the mount table and not the mount.
//!
//! ```sh
//! set -a; . ./.env; set +a
//! search 'pricing' --in chat/slack
//! ```
//!
//! What is searchable depends on what the environment carries. `SLACK_USER_TOKEN` mounts a
//! Slack workspace at `chat/slack`, and mounting is the whole of the registration. A bot token
//! mounts just as well and simply has no index to offer, which the command reports rather than
//! answering with nothing. With nothing set, `search` reports that no store has an index —
//! which is the honest answer and not an empty result.

use std::io::Write;
use std::process::ExitCode;

use cortex::exec::{ExecCall, Executable};
use cortex::fs::WorkFs;
use cortex_exec_search::Search;

fn main() -> ExitCode {
    // One runtime, `enable_all` because the clients behind the backends wait on a network.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("search: no runtime: {e}");
            return ExitCode::from(2);
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Before any credential, deliberately: `--help` is a question about the command and not
    // about a service, and answering it with "no token" tells the asker nothing they asked.
    // Asked of the parser rather than of the argument list, so the answer here and the answer
    // through a console cannot differ — `--in --help` is a missing query to both.
    if cortex_exec_search::wants_help(&args) {
        print!("{}", cortex_exec_search::usage());
        return ExitCode::SUCCESS;
    }

    let call = ExecCall {
        name: "search".into(),
        args,
        cwd: None,
        env: Default::default(),
    };
    let out = Search::over(&workspace()).exec(&call, None).await;

    // Written rather than printed: a record is bytes copied out of a file's own format, and
    // nothing here has a reason to decode and re-encode them.
    let _ = std::io::stdout().write_all(&out.stdout);
    let _ = std::io::stderr().write_all(&out.stderr);
    // Clamped rather than cast: an exit code is one byte here, and 0/1/2 is the whole
    // vocabulary this command has.
    ExitCode::from(u8::try_from(out.exit_code).unwrap_or(2))
}

/// The workspace this environment can assemble.
///
/// A store is mounted only when its credential is present, so what a fan-out reports on is what
/// actually exists rather than a roster of what might have. Whether any of them can be
/// *searched* is not decided here: that is the store's own
/// [`index`](cortex::fs::FileSystem::index), and reading it is [`Search::over`]'s job.
fn workspace() -> WorkFs {
    let mut work = WorkFs::new();

    #[cfg(feature = "slack")]
    if let Some(token) = std::env::var("SLACK_USER_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        use cortex::fs::{MessengerFs, SlackConfig, SlackSource};

        let config = SlackConfig {
            user_token: Some(token),
            bot_token: None,
            base_url: std::env::var("SLACK_BASE_URL")
                .ok()
                .filter(|u| !u.is_empty()),
        };
        // Neither failure is fatal and both are said: a workspace missing one store is still a
        // workspace, and naming the one that is missing is more use than exiting.
        match SlackSource::new(&config) {
            Ok(source) => {
                if let Err(e) = work.mount("chat/slack", MessengerFs::new(source)) {
                    eprintln!("search: chat/slack: {e}");
                }
            }
            Err(e) => eprintln!("search: chat/slack: {e}"),
        }
    }

    work
}
