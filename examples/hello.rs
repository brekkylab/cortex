//! One console: boot alpine with `jq` installed, run one command in it, and end.
//!
//! ```sh
//! cargo run --example hello
//! ```
//!
//! Needs the console server under `~/.cache/cortex/bin` (or `CORTEX_STDIO_SERVER_PATH`).

use cortex::{console::ConsoleClient, image::Recipe};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut console = ConsoleClient::builder()
        .image(Recipe::new("alpine:3.24").step("apk add --no-cache jq"))
        .build()
        .await?;

    let result = console
        .exec(
            ["sh", "-c", r#"echo '{"hello": "cortex"}' | jq -r .hello"#],
            None,
        )
        .await?;

    print!("{}", String::from_utf8_lossy(&result.stdout));
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    println!("exit code: {}", result.code);

    // Dropping the console says `quit`, and the server tears the session down.
    Ok(())
}
