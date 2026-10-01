# virtx for Python

Python bindings for virtx: the same `ConsoleClient`, `ImageClient`, `Recipe`,
`Directory` and host mount as the Rust crate, with every call that waits as an awaitable.

```python
from virtx import ConsoleClient, Directory, HostMount, Recipe

mount = HostMount(Directory().with_file("SKILL.md", "..."), "/tmp/skill")

async with await (
    ConsoleClient.builder()
    .cmd(["virtx-uvm"])
    .image(Recipe("python:3.12-slim-trixie").step("pip install duckdb"))
    .mount_readonly(mount, "/skills/example")
    .mount("./artifacts", "/artifacts")
    .network(False)
    .vcpus(2)
    .memory_mib(2048)
    .build()
) as console:
    result = await console.exec(["sh", "-c", "ls /skills/example"], timeout_ms=10_000)
    print(result.code, result.stdout.decode())
```

Without `.cmd(..)` a console runs `virtx-uvm` from the stdio server directory
(`$VIRTX_STDIO_SERVER_PATH`, or `~/.cache/virtx/bin`).

An image can also be built ahead of the session that runs on it, and then named by its ref or
its digest:

```python
from virtx import ImageClient, ImageSource, Recipe

async with await ImageClient.try_new() as images:
    built = await images.build(Recipe("alpine:3.20").step("apk add jq"), "myimg:latest")
    print(built.reference, built.digest)

    for image in await images.list():
        print(image.digest, image.refs)

console = await ConsoleClient.builder().image(ImageSource.digest(built.digest)).build()
```

## Building

The extension is built with [maturin](https://www.maturin.rs). The `mount` feature is on by
default and needs the host's FUSE provider, as the crate does — see the top-level README.

```sh
cd bindings/python
uv sync                        # makes .venv and installs the dev group
uv run maturin develop         # builds the extension into it
uv run pytest
```

`uv run maturin build --release` makes a wheel. The crate's other features pass through:
`maturin develop --features s3,notion`.

The console tests at the end of `tests/test_virtx.py` need a console server, named by
`$VIRTX_CONSOLE`; without one they are skipped.
