"""The crate's quickstart, in Python.

A file held in memory beside a host directory, mounted on the host, and a console that runs
a command against it.

    CORTEX_CONSOLE=cortex-krun uv run python examples/quickstart.py /path/to/project
"""

import asyncio
import os
import sys
import tempfile

from cortex import ConsoleClient, Directory, HostMount, NetworkAccess, Recipe


async def main(project: str) -> None:
    # What the agent can see. A `Directory` is itself a filesystem, so a binding drives it
    # like any single store.
    context = (
        Directory()
        .with_file("notes/today.md", "ship the release")
        .with_mount("project", project)
    )

    # Where the host can see it. It stays mounted for as long as something holds it — here,
    # the variable and the console below.
    mount = HostMount(context, tempfile.mkdtemp(prefix="cortex-"))

    # What the agent can do: a server that runs its commands, against that tree.
    console = await (
        ConsoleClient.builder()
        .cmd([os.environ.get("CORTEX_CONSOLE", "cortex-krun")])
        .image(Recipe("python:3.12-slim-trixie"))
        .mount(mount, "/work")
        .network(NetworkAccess.none())
        .build()
    )

    async with console:
        result = await console.exec(["sh", "-c", "wc -w /work/notes/today.md"])
        print(result.stdout.decode(errors="replace"), end="")


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else os.getcwd()))
