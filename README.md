# virtx

virtx lets you run tasks in disposable Linux VMs from your own code.

It's useful for jobs with heavy dependencies that you'd rather not install on your own machine.
Whatever they install or change is gone when the VM shuts down.

No Docker or heavy daemon required.
Create, use, and dispose of VMs directly from your code.

## Quickstart

### Python

```sh
pip install virtx
```

```python
import asyncio

from virtx import ConsoleClient, Recipe, ensure_virtx


async def main() -> None:
    # Fetches the console server into virtx's cache the first time; a no-op after.
    await ensure_virtx()
    console = await (
        ConsoleClient.builder()
        .image(Recipe("alpine:latest").step("apk add --no-cache jq"))
        .build()
    )

    async with console:
        result = await console.exec(
            ["sh", "-c", """echo '{"hello": "virtx"}' | jq -r .hello"""]
        )

    print("\n" + result.stdout.decode(), end="")


asyncio.run(main())
```

### Node

```sh
npm install @brekkylab/virtx
```

```js
import { ConsoleClient, Recipe, ensureVirtx } from '@brekkylab/virtx'

// Fetches the console server into virtx's cache the first time; a no-op after.
await ensureVirtx()
const console_ = await ConsoleClient.builder()
  .image(new Recipe('alpine:latest').step('apk add --no-cache jq'))
  .build()

try {
  const result = await console_.exec(['sh', '-c', `echo '{"hello": "virtx"}' | jq -r .hello`])
  process.stdout.write('\n' + result.stdout)
} finally {
  await console_.close()
}
```

### Rust

```rust
use virtx::{console::ConsoleClient, image::Recipe};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut console = ConsoleClient::builder()
        .image(Recipe::new("alpine:latest").step("apk add --no-cache jq"))
        .build()
        .await?;

    let result = console
        .exec(["sh", "-c", r#"echo '{"hello": "virtx"}' | jq -r .hello"#], None)
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

virtx
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

Turn it on with the builder's `gpu` option, and install the guest's half of Vulkan in the image:

```python
import asyncio

from virtx import ConsoleClient, Recipe, ensure_virtx


async def main() -> None:
    await ensure_virtx()
    console = await (
        ConsoleClient.builder()
        .image(
            Recipe("alpine:latest").step(
                # The Vulkan loader, the venus driver that reaches the host's GPU,
                # and vulkaninfo to look at it.
                "apk add --no-cache vulkan-loader mesa-vulkan-virtio vulkan-tools"
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

The host's GPU shows up in the guest as `Virtio-GPU Venus (<the host's GPU>)`. What the image needs for that:

| | Alpine | Debian |
|---|---|---|
| Vulkan loader (`libvulkan.so.1`) | `vulkan-loader` | `libvulkan1` |
| venus driver | `mesa-vulkan-virtio` | `mesa-vulkan-drivers`, trixie or later |

- **Install the loader yourself.** Without it, no program in the guest finds a Vulkan device, whatever the host has. Some packages bring it along (Debian's `vulkan-tools` does) and some don't (Alpine's doesn't), so name it rather than count on it.
- **An image with no venus driver still runs, but on the CPU.** Debian bookworm's Mesa has no venus, and Vulkan there falls back to `llvmpipe`, a software renderer. Check the `deviceName` your program picks.

If the host can't give a GPU, `build()` fails instead of quietly running on the CPU. On the host, that takes a Vulkan loader too: `libvulkan1` (Debian, Ubuntu) or `vulkan-loader` (Fedora) on Linux, and on Windows `vulkan-1.dll`, which comes with the GPU's driver.

### Filesystem

Mount a host directory into the VM by passing its path.

```python
import asyncio

from virtx import ConsoleClient, Recipe


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

from virtx import ConsoleClient, Directory, HostMount, Recipe


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
use virtx::{
    console::ConsoleClient,
    fs::{S3Config, S3Fs},
    image::Recipe,
};

#[cfg(target_os = "linux")]
use virtx::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use virtx::fs::FuseTMount as HostMount;
#[cfg(windows)]
use virtx::fs::DokanMount as HostMount;

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

    let mountpoint = std::env::temp_dir().join("virtx-s3");
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
The `mount` feature, on by default, mounts a virtx filesystem on the host through the host's FUSE provider:

| Host  | Provider | Needed to build | Needed to mount |
|-------|----------|-----------------|---------------|
| Linux | `/dev/fuse` in the kernel | — | — |
| macOS | [FUSE-T](https://www.fuse-t.org) | — | ✓ |
| Windows | [Dokany](https://github.com/dokan-dev/dokany) | — | ✓ |

A program built with `mount` runs on a host without the provider; only mounting fails, with an error that says what to install.
`virtx::fs::mount_support()` (`mount_support()` in Python, `mountSupport()` in Node) asks ahead of a mount.

On Windows, a Rust program gets that only if it delay-loads `dokan2.dll`, which it has to ask for in its own `build.rs`. virtx's cannot do it on the program's behalf, since a link argument reaches only the package that prints it. The Node and Python packages already do it. Without it, a program that mounts needs Dokany installed just to start.

```rust
// build.rs of a program that depends on virtx
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo::rustc-link-arg=/DELAYLOAD:dokan2.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
```

If you don't mount on the host, build with `default-features = false` and skip all of this.

For macOS

```sh
brew install --cask fuse-t
```

virtx mounts through FUSE-T 1.x, and is checked against 1.2.7. A FUSE-T of another major version is refused before a mount, with the release to install instead, rather than risk a crash on a layout that changed; `VIRTX_FUSE_T_UNCHECKED=1` mounts with it anyway.

And for windows

```powershell
winget install --id dokan-dev.Dokany
```

## Cache

virtx keeps all persistent state in a single cache directory that you can safely delete at any time:

| Host | Cache directory |
|------|-----------------|
| Linux | `$XDG_CACHE_HOME/virtx`, or `~/.cache/virtx` |
| macOS | `~/Library/Caches/virtx` |
| Windows | `%LOCALAPPDATA%\virtx` |

Set `VIRTX_HOME` to put it somewhere else.
