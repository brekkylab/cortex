import os
import shutil

import pytest

import cortex
from cortex import (
    Console,
    CortexError,
    Directory,
    ErrorCode,
    Image,
    NetworkAccess,
    Step,
)


def test_image_is_a_value():
    base = Image().base("python:3.12-slim")
    extended = base.step("pip install duckdb").step(Step.env("TZ", "UTC"))

    assert "python:3.12-slim" in repr(extended)
    assert "duckdb" in repr(extended)
    assert "duckdb" not in repr(base)


def test_image_from_dockerfile():
    image = Image.from_dockerfile("FROM alpine:3.20\nRUN apk add jq\n")
    assert "alpine:3.20" in repr(image)


def test_network_access():
    network = NetworkAccess.host().with_host_ports([8080])
    assert network.reach == "host"
    assert network.host_ports == [8080]
    assert NetworkAccess.none().host_ports == []


def test_error_codes_are_cortex_s():
    assert ErrorCode.TIMED_OUT == -32000
    assert ErrorCode.INTERNAL_ERROR == -32603


def test_directory_refuses_a_file_under_a_mount(tmp_path):
    directory = Directory().with_mount("project", tmp_path)
    with pytest.raises(OSError):
        directory.add_file("project/notes.md", "under a mount")


@pytest.mark.skipif(not hasattr(cortex, "HostMount"), reason="built without `mount`")
def test_host_mount_serves_the_directory(tmp_path):
    mountpoint = tmp_path / "mnt"
    mountpoint.mkdir()
    directory = Directory().with_file("notes/today.md", b"ship the release")

    mount = cortex.HostMount(directory, mountpoint)
    try:
        assert (mountpoint / "notes" / "today.md").read_bytes() == b"ship the release"
        # The mount owns the tree now.
        with pytest.raises(ValueError):
            directory.add_file("more.md", "")
    finally:
        del mount


async def test_building_without_a_server_fails_and_spends_the_builder():
    builder = Console.builder()
    with pytest.raises(CortexError):
        await builder.build()
    with pytest.raises(ValueError):
        builder.vcpus(2)


async def test_building_against_a_missing_binary_fails():
    with pytest.raises(CortexError):
        await Console.builder().stdio_client(["cortex-no-such-console-server"]).build()


# Against a real console server, named by `$CORTEX_CONSOLE` (`cortex-krun`, say).
SERVER = os.environ.get("CORTEX_CONSOLE")


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $CORTEX_CONSOLE")
async def test_exec_read_write(tmp_path):
    builder = (
        Console.builder()
        .stdio_client([SERVER])
        .image(Image().base("python:3.12-slim-trixie"))
        .mount(tmp_path, "/work")
        .network(NetworkAccess.none())
    )
    async with await builder.build() as console:
        assert console.mounts == ["/work"]

        assert await console.write("/work/hello.txt", "hi") == 2
        result = await console.exec(["cat", "/work/hello.txt"])
        assert result.code == 0
        assert result.stdout == b"hi"

        read = await console.read("/work/hello.txt")
        assert read.data == b"hi"
        assert read.size == 2
