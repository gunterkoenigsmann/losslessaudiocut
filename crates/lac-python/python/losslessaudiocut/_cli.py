"""The ``lac`` command of the package: the same command line tool as the
native ``lac`` program, running inside this Python process."""

from __future__ import annotations

import sys

from . import _native


def main() -> int:
    """Runs ``lac`` with this process's arguments and exits with its status."""
    # Python may have buffered output of its own; the native code writes to
    # the file descriptors directly.
    sys.stdout.flush()
    sys.stderr.flush()
    # argv[0] as "lac", whatever the launcher script is called, so that
    # usage messages and completions name the command users type.
    sys.exit(_native.run_cli(["lac", *sys.argv[1:]]))
