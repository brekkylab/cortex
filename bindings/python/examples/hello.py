"""One console: boot alpine with `jq` installed, run one command in it, and end.

    uv run python examples/hello.py
"""

import asyncio
import sys

from virtx import ConsoleClient, Recipe, ensure_virtx


async def main() -> None:
    await ensure_virtx()
    console = await (
        ConsoleClient.builder()
        .image(Recipe("alpine:latest").step("apk add --no-cache jq"))
        .build()
    )

    # Leaving the block says `quit`, and the server tears the session down.
    async with console:
        result = await console.exec(
            ["sh", "-c", """echo '{"hello": "virtx"}' | jq -r .hello"""]
        )

    print(result.stdout.decode(errors="replace"), end="")
    print(result.stderr.decode(errors="replace"), end="", file=sys.stderr)
    print(f"exit code: {result.code}")


if __name__ == "__main__":
    asyncio.run(main())
