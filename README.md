# Cortex

Cortex lets you run tasks in VMs on any OS.

It's useful for jobs with heavy dependencies that you'd rather not install on your own machine.
Whatever they install or change is gone when the VM shuts down.

It doesn't need Docker or a heavy daemon.
Everything runs inside your own code.

## Quickstart

### Python

```python
import asyncio

from cortex import ConsoleClient, Recipe


async def main() -> None:
    console = await (
        ConsoleClient.builder()
        .image(Recipe("alpine:latest").step("apk add --no-cache jq"))
        .build()
    )

    async with console:
        result = await console.exec(
            ["sh", "-c", """echo '{"hello": "cortex"}' | jq -r .hello"""]
        )

    print("\n" + result.stdout.decode(), end="")


asyncio.run(main())
```

### Node

```js
import { ConsoleClient, Recipe } from 'cortex-node'

const console_ = await ConsoleClient.builder()
  .image(new Recipe('alpine:latest').step('apk add --no-cache jq'))
  .build()

try {
  const result = await console_.exec(['sh', '-c', `echo '{"hello": "cortex"}' | jq -r .hello`])
  process.stdout.write('\n' + result.stdout)
} finally {
  await console_.close()
}
```

### Rust

```rust
use cortex::{console::ConsoleClient, image::Recipe};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut console = ConsoleClient::builder()
        .image(
            Recipe::new("alpine:latest")
                .step("apk add --no-cache jq")
        )
        .build()
        .await?;

    let result = console
        .exec(["sh", "-c", r#"echo '{"hello": "cortex"}' | jq -r .hello"#], None)
        .await?;

    print!("\n{}", String::from_utf8_lossy(&result.stdout));

    Ok(())
}
```

It'll shows
```text
step 1/1: cd '/' && apk add --no-cache jq
(1/2) Installing oniguruma (6.9.10-r0)
(2/2) Installing jq (1.8.2-r0)
Executing busybox-1.37.0-r31.trigger
OK: 9426 KiB in 18 packages

cortex
```

The only thing cortex keeps is its cache, all under one directory you can delete at any time:

| Host | Cache directory |
|------|-----------------|
| Linux | `$XDG_CACHE_HOME/cortex`, or `~/.cache/cortex` |
| macOS | `~/Library/Caches/cortex` |
| Windows | `%LOCALAPPDATA%\cortex` |

Set `CORTEX_HOME` to put it somewhere else.

## Features

- **Simple to run**: describe an image as a base and a few steps, or hand over a Dockerfile, and one builder call boots a VM from it. Built images are cached by digest, so the build is paid once.
- **Virtual mounts**: mount a directory from the host, files held in memory, or an S3, Google Drive or Notion store, and commands inside read it as ordinary files.
- **GPU support**: ask for a GPU and set its memory. If the backend can't provide one, the session fails to start instead of quietly running on the CPU.
- **Cross-platform**: runs on Linux, macOS and Windows.

## Requirements

The `mount` feature, on by default, mounts a cortex filesystem on the host through the host's FUSE provider:

| Host  | Provider | Needed to build | Needed to run |
|-------|----------|-----------------|---------------|
| Linux | `/dev/fuse` in the kernel | — | — |
| macOS | [FUSE-T](https://www.fuse-t.org) | ✓ | ✓ |
| Windows | [Dokany](https://github.com/dokan-dev/dokany) | — | ✓ |

If you don't mount on the host, build with `default-features = false` and skip all of this.

### macOS

```sh
brew install --cask fuse-t
```

### Windows

```powershell
winget install --id dokan-dev.Dokany
```

The build links the installed Dokany library when `DokanLibrary2_LibraryPath_x64` is set, and otherwise builds one from vendored sources. Prefer the installed one: a self-built library can disagree with the installed driver's version, which fails only at mount time.
