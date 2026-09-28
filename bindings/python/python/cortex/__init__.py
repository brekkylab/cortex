"""The environment an agent works in: what it can see, and what it can do.

What it sees is a filesystem — a ``Directory`` assembled in memory and grafted onto host
directories, mounted on the host with ``HostMount``. What it does is run commands — a
``ConsoleClient`` built against a console server, which runs them wherever that server runs
things, on an image named by an ``ImageSource`` or declared by a ``Recipe``. An
``ImageClient`` builds, lists and removes those images ahead of any session.

The names and their behaviour are cortex's own; see the Rust crate's documentation for the
long form.
"""

from enum import IntEnum

from . import _cortex
from ._cortex import (
    BuildImageResult,
    ConsoleBroken,
    ConsoleClient,
    ConsoleClientBuilder,
    ConsoleRefused,
    CortexError,
    Directory,
    ExecResult,
    ImageClient,
    ImageEntry,
    ImageSource,
    NetworkAccess,
    ReadResult,
    Recipe,
    Step,
)

# The numbers a `ConsoleRefused.code` may hold, named as cortex names them.
ErrorCode = IntEnum("ErrorCode", _cortex.ERROR_CODES)

__all__ = [
    "BuildImageResult",
    "ConsoleBroken",
    "ConsoleClient",
    "ConsoleClientBuilder",
    "ConsoleRefused",
    "CortexError",
    "Directory",
    "ErrorCode",
    "ExecResult",
    "ImageClient",
    "ImageEntry",
    "ImageSource",
    "NetworkAccess",
    "ReadResult",
    "Recipe",
    "Step",
]

# Present only when the extension was built with the `mount` feature, which is the default.
if hasattr(_cortex, "HostMount"):
    from ._cortex import HostMount, mount_support

    __all__ += ["HostMount", "mount_support"]
