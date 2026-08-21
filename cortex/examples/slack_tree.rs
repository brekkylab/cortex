//! Print a live Slack workspace as the tree it is mounted as, and nothing else.
//!
//! ```text
//! set -a; . ./.env; set +a
//! cargo run -p cortex --features slack --example slack_tree
//! ```
//!
//! Nothing here is part of the crate's surface: it walks `MessengerFs` through `FileSystem`
//! exactly the way a mount does, and prints what comes back.

use std::path::Path;

use cortex::fs::{DirentKind, FileSystem, MessengerFs, SlackConfig, SlackSource};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let Ok(token) = std::env::var("SLACK_USER_TOKEN") else {
        eprintln!("SLACK_USER_TOKEN is not set");
        return;
    };
    let source = match SlackSource::new(&SlackConfig {
        user_token: Some(token),
        bot_token: None,
        base_url: std::env::var("SLACK_BASE_URL").ok().filter(|u| !u.is_empty()),
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot build a source: {e}");
            return;
        }
    };

    println!(".");
    walk(&MessengerFs::new(source), "", "").await;
}

/// One directory, then each child under it. Recursion through a boxed future because the depth
/// is the tree's and not a number known here.
fn walk<'a>(
    fs: &'a MessengerFs<SlackSource>,
    at: &'a str,
    pad: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let mut entries = match fs.list(Path::new(if at.is_empty() { "/" } else { at })).await {
            Ok(es) => es,
            Err(e) => {
                println!("{pad}└── <{e}>");
                return;
            }
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        for (i, e) in entries.iter().enumerate() {
            let last = i + 1 == entries.len();
            let (branch, next) = if last {
                ("└── ", format!("{pad}    "))
            } else {
                ("├── ", format!("{pad}│   "))
            };
            let dir = e.kind == DirentKind::Dir;
            println!("{pad}{branch}{}{}", e.name, if dir { "/" } else { "" });
            if dir {
                walk(fs, &format!("{at}/{}", e.name), &next).await;
            }
        }
    })
}
