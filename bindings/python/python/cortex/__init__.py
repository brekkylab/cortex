"""The environment an agent works in: what it can see, and what it can do.

What it sees is a filesystem — a ``Directory`` assembled in memory and grafted onto host
directories, mounted on the host with ``HostMount``. What it does is run commands — a
``Console`` built against a console server, which runs them wherever that server runs
things.

The names and their behaviour are cortex's own; see the Rust crate's documentation for the
long form.
"""

from enum import IntEnum

from . import _cortex
from ._cortex import (
    Console,
    ConsoleBroken,
    ConsoleBuilder,
    ConsoleRefused,
    CortexError,
    Directory,
    ExecResult,
    Image,
    NetworkAccess,
    ReadResult,
    Step,
)

# The numbers a `ConsoleRefused.code` may hold, named as cortex names them.
ErrorCode = IntEnum("ErrorCode", _cortex.ERROR_CODES)

__all__ = [
    "Console",
    "ConsoleBroken",
    "ConsoleBuilder",
    "ConsoleRefused",
    "CortexError",
    "Directory",
    "ErrorCode",
    "ExecResult",
    "Image",
    "NetworkAccess",
    "ReadResult",
    "Step",
]

# Present only when the extension was built with the `mount` feature, which is the default.
if hasattr(_cortex, "HostMount"):
    from ._cortex import HostMount

    __all__.append("HostMount")
