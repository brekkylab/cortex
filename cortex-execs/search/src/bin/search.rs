//! `search` against real services, with no console and no mount in front of them.
//!
//! The same [`Executable`] a console reaches, so what it answers here is what it answers there;
//! the difference is only who registers the backends.
//!
//! ```sh
//! set -a; . ./.env; set +a
//! search 'pricing' --in chat/slack
//! ```
//!
//! What is registered depends on what the environment carries. `SLACK_USER_TOKEN` mounts a
//! Slack workspace's index at `chat/slack`; a bot token cannot search, and the command says so
//! rather than answering with nothing. With nothing set, `search` reports that no store has an
//! index — which is the honest answer and not an empty result.

use std::io::Write;
use std::process::ExitCode;

use cortex::exec::{ExecCall, Executable};
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
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", cortex_exec_search::usage());
        return ExitCode::SUCCESS;
    }

    let call = ExecCall {
        name: "search".into(),
        args,
        cwd: None,
        env: Default::default(),
    };
    let out = registered().exec(&call, None).await;

    // Written rather than printed: a record is bytes copied out of a file's own format, and
    // nothing here has a reason to decode and re-encode them.
    let _ = std::io::stdout().write_all(&out.stdout);
    let _ = std::io::stderr().write_all(&out.stderr);
    // Clamped rather than cast: an exit code is one byte here, and 0/1/2 is the whole
    // vocabulary this command has.
    ExitCode::from(u8::try_from(out.exit_code).unwrap_or(2))
}

/// Whatever this environment can search.
///
/// A backend is registered only when its credential is present, so the report a fan-out prints
/// names the stores that actually exist rather than a roster of what might have.
fn registered() -> Search {
    let mut search = Search::new();

    #[cfg(feature = "slack")]
    if let Some(token) = std::env::var("SLACK_USER_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        use cortex::fs::{SlackConfig, SlackSource};
        use cortex_exec_search::backend::messenger::Messenger;

        let config = SlackConfig {
            user_token: Some(token),
            bot_token: None,
            base_url: std::env::var("SLACK_BASE_URL")
                .ok()
                .filter(|u| !u.is_empty()),
        };
        match SlackSource::new(&config) {
            Ok(source) => search = search.with("chat/slack", Messenger::new(source)),
            Err(e) => eprintln!("search: chat/slack: {e}"),
        }
    }

    search
}
