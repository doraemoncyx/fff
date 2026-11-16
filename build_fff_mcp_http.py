#!/usr/bin/env python3
"""Build fff-mcp-http; extra args are passed to cargo.

FFF_FEATURES selects the walker feature set (default zlob); --debug builds the
dev profile instead of release. The release build keeps debuginfo and symbols
so sampled stacks from a running binary resolve to real frames. zig and
libclang are set up by build.py.
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CRATE = "fff-mcp-http"


def load_build_helpers():
    spec = importlib.util.spec_from_file_location("fff_build", ROOT / "build.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load build.py next to this script")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    helpers = load_build_helpers()
    helpers.setup_libclang()
    helpers.setup_zig()

    args = sys.argv[1:]
    profile = ["--release"]
    if "--debug" in args:
        args.remove("--debug")
        profile = ["--profile", "dev"]
    else:
        # release with debuginfo: keep opt-level 3 / lto from [profile.release],
        # only override strip and debug level. Respect values already set.
        os.environ.setdefault("CARGO_PROFILE_RELEASE_DEBUG", "2")
        os.environ.setdefault("CARGO_PROFILE_RELEASE_STRIP", "none")

    command = [
        "cargo",
        "build",
        "-p",
        CRATE,
        "--no-default-features",
        "--features",
        os.environ.get("FFF_FEATURES", "zlob"),
        *profile,
        *args,
    ]
    print(" ".join(command), flush=True)
    return subprocess.run(command, cwd=ROOT).returncode


if __name__ == "__main__":
    sys.exit(main())
