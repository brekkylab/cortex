//! One console: boot alpine with `jq` installed, run one command in it, and end.
//!
//! ```sh
//! cargo run --example hello
//! ```
//!
//! Needs `virtx-uvm` in `cache_root()/bin` (`$VIRTX_HOME/bin` if set); `ensure_virtx` fetches it.

use virtx::{console::ConsoleClient, image::Recipe};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut console = ConsoleClient::builder()
        .image(Recipe::new("alpine:latest").step("apk add --no-cache jq"))
        .build()
        .await?;

    let result = console
        .exec(
            ["sh", "-c", r#"echo '{"hello": "virtx"}' | jq -r .hello"#],
            None,
        )
        .await?;

    print!("{}", String::from_utf8_lossy(&result.stdout));
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    println!("exit code: {}", result.code);

    // Dropping the console says `quit`, and the server tears the session down.
    Ok(())
}
