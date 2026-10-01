"""The environment an agent works in: what it can see, and what it can do.

What it sees is a filesystem: a ``Directory`` assembled in memory and grafted onto host
directories, mounted on the host with ``HostMount``. What it does is run commands through a
``ConsoleClient``, wherever its console server runs them, on an image named by an
``ImageSource`` or declared by a ``Recipe``. ``ImageClient`` builds, lists and removes those
images ahead of any session.

Names and behaviour are virtx's own; see the Rust crate's documentation for details.
"""

from enum import IntEnum

from . import _virtx
from ._virtx import (
    BuildImageResult,
    ConsoleBroken,
    ConsoleClient,
    ConsoleClientBuilder,
    ConsoleRefused,
    VirtxError,
    Directory,
    ExecResult,
    ImageClient,
    ImageEntry,
    ImageSource,
    ReadResult,
    Recipe,
    Step,
)

# The numbers a `ConsoleRefused.code` may hold, named as virtx names them.
ErrorCode = IntEnum("ErrorCode", _virtx.ERROR_CODES)

__all__ = [
    "BuildImageResult",
    "ConsoleBroken",
    "ConsoleClient",
    "ConsoleClientBuilder",
    "ConsoleRefused",
    "VirtxError",
    "Directory",
    "ErrorCode",
    "ExecResult",
    "ImageClient",
    "ImageEntry",
    "ImageSource",
    "ReadResult",
    "Recipe",
    "Step",
]

# Present only when the extension was built with the `ensure` feature, which is the default.
if hasattr(_virtx, "ensure_virtx"):
    from ._virtx import ensure_virtx

    __all__.append("ensure_virtx")

# Present only when the extension was built with the `mount` feature, which is the default.
if hasattr(_virtx, "HostMount"):
    from ._virtx import HostMount, mount_support

    __all__ += ["HostMount", "mount_support"]
