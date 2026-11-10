#!/usr/bin/env python3
"""Build fff-mcp (release) with zlob feature; extra args are passed to cargo."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

LLVM_PATHS = [
    r"C:\Program Files\LLVM\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin",
]


def setup_libclang() -> None:
    for p in LLVM_PATHS:
        if (Path(p) / "libclang.dll").exists():
            os.environ["LIBCLANG_PATH"] = p
            return
    print(
        "warning: libclang.dll not found; zlob build may fail. Set LIBCLANG_PATH manually.",
        file=sys.stderr,
    )


def main() -> int:
    setup_libclang()
    return subprocess.run(
        ["cargo", "build", "--release", "-p", "fff-mcp", "--features", "zlob", *sys.argv[1:]]
    ).returncode


if __name__ == "__main__":
    sys.exit(main())
