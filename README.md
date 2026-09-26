# Cortex

Cortex lets you run tasks in disposable Linux VMs from your own code.

It's useful for jobs with heavy dependencies that you'd rather not install on your own machine.
Whatever they install or change is gone when the VM shuts down.

No Docker or heavy daemon required.
Create, use, and dispose of VMs directly from your code.

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
        .image(Recipe::new("alpine:latest").step("apk add --no-cache jq"))
        .build()
        .await?;

    let result = console
        .exec(["sh", "-c", r#"echo '{"hello": "cortex"}' | jq -r .hello"#], None)
        .await?;

    print!("\n{}", String::from_utf8_lossy(&result.stdout));

    Ok(())
}
```

Output:
```text
step 1/1: cd '/' && apk add --no-cache jq
(1/2) Installing oniguruma (6.9.10-r0)
(2/2) Installing jq (1.8.2-r0)
Executing busybox-1.37.0-r31.trigger
OK: 9426 KiB in 18 packages

cortex
```

## Features

### Coverage

| | Supported |
|---|---|
| **Languages** | 🐍 Python · <img src="https://cdn.jsdelivr.net/gh/devicons/devicon/icons/nodejs/nodejs-original.svg" height="14" alt=""> Node · 🦀 Rust |
| **Hosts** | 🐧 Linux · 🍎 macOS (Apple silicon) · <img src="https://cdn.jsdelivr.net/gh/devicons/devicon/icons/windows11/windows11-original.svg" height="14" alt=""> Windows 11 or later |
| **Guest** | 🐧 Linux, always |

### GPU support

The VM gets a GPU through Vulkan on every host, so it can run heavy work like deep learning.

Turn it on with the builder's `gpu` option:

```python
import asyncio

from cortex import ConsoleClient, Recipe


async def main() -> None:
    console = await (
        ConsoleClient.builder()
        .image(
            Recipe("debian:bookworm-slim").step(
                "apt-get update && apt-get install -y mesa-vulkan-drivers vulkan-tools"
            )
        )
        .gpu(True)
        .gpu_memory_mib(8192)
        .build()
    )

    async with console:
        result = await console.exec(["vulkaninfo", "--summary"])

    print(result.stdout.decode(), end="")


asyncio.run(main())
```

If the host can't give a GPU, `build()` fails instead of quietly running on the CPU.

### Filesystem

Mount a host directory into the VM by passing its path.

```python
import asyncio

from cortex import ConsoleClient, Recipe


async def main() -> None:
    console = await (
        ConsoleClient.builder()
        .image(Recipe("alpine:latest"))
        .mount(".", "/project")
        .mount_readonly("/etc", "/host-etc")
        .build()
    )

    async with console:
        result = await console.exec(["sh", "-c", "ls /project && touch /project/hello.txt"])

    print(result.stdout.decode(), end="")


asyncio.run(main())
```

Writes to `/project` land in the host's current directory, while `/host-etc` is read-only: commands in the VM can read it but not write to it.

Going further, you can build a virtual directory in code and mount it into the VM through FUSE.
It mixes files held only in memory with host directories, all under one mount point.

```python
import asyncio
import tempfile

from cortex import ConsoleClient, Directory, HostMount, Recipe


async def main() -> None:
    directory = (
        Directory()
        .with_file("notes/today.md", "ship the release")
        .with_mount("project", ".")
    )
    mount = HostMount(directory, tempfile.mkdtemp())

    console = await (
        ConsoleClient.builder()
        .image(Recipe("alpine:latest"))
        .mount(mount, "/data")
        .build()
    )

    async with console:
        result = await console.exec(["sh", "-c", "cat /data/notes/today.md && ls /data/project"])

    print(result.stdout.decode(), end="")


asyncio.run(main())
```

`notes/today.md` lives only in memory, and `project` is the host's current directory.

Moreover, external stores like S3, Google Drive and Notion, and commands inside the VM can read them as ordinary files.

```rust
use cortex::{
    console::ConsoleClient,
    fs::{S3Config, S3Fs},
    image::Recipe,
};

#[cfg(target_os = "linux")]
use cortex::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use cortex::fs::FuseTMount as HostMount;
#[cfg(windows)]
use cortex::fs::DokanMount as HostMount;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bucket = S3Fs::new(&S3Config {
        bucket: "my-bucket".into(),
        region: "us-east-1".into(),
        access_key_id: std::env::var("AWS_ACCESS_KEY_ID")?,
        secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY")?,
        endpoint: None,
        key_prefix: None,
    })?;

    let mountpoint = std::env::temp_dir().join("cortex-s3");
    std::fs::create_dir_all(&mountpoint)?;
    let mount = HostMount::try_new(bucket, &mountpoint)?;

    let mut console = ConsoleClient::builder()
        .image(Recipe::new("alpine:latest"))
        .mount_readonly(mount, "/s3")
        .build()
        .await?;

    let result = console.exec(["ls", "-R", "/s3"], None).await?;
    print!("{}", String::from_utf8_lossy(&result.stdout));

    Ok(())
}
```

These stores are Rust only for now, each behind its own feature: `s3`, `gdrive` and `notion`.
S3 is read-only, so it's mounted with `mount_readonly`.

This feature needs an extra package installed on macOS and Windows.
The `mount` feature, on by default, mounts a cortex filesystem on the host through the host's FUSE provider:

| Host  | Provider | Needed to build | Needed to run |
|-------|----------|-----------------|---------------|
| Linux | `/dev/fuse` in the kernel | — | — |
| macOS | [FUSE-T](https://www.fuse-t.org) | ✓ | ✓ |
| Windows | [Dokany](https://github.com/dokan-dev/dokany) | — | ✓ |

If you don't mount on the host, build with `default-features = false` and skip all of this.

For macOS

```sh
brew install --cask fuse-t
```

And for windows

```powershell
winget install --id dokan-dev.Dokany
```

## Cache

Cortex keeps all persistent state in a single cache directory that you can safely delete at any time:

| Host | Cache directory |
|------|-----------------|
| Linux | `$XDG_CACHE_HOME/cortex`, or `~/.cache/cortex` |
| macOS | `~/Library/Caches/cortex` |
| Windows | `%LOCALAPPDATA%\cortex` |

Set `CORTEX_HOME` to put it somewhere else.
