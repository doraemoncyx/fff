#!/usr/bin/env python3
"""Build fff-mcp (release) with zlob feature; extra args are passed to cargo."""

from __future__ import annotations

import glob
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

LLVM_PATHS = [
    r"C:\Program Files\LLVM\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin",
    r"C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin",
]

# zlob 1.8.0 declares .minimum_zig_version = "0.17.1"
MIN_ZIG = (0, 17, 1)
ZIG_VERSION = "0.17.1"
ZIG_INDEX = "https://ziglang.org/download/index.json"
# tried before a direct connection
PROXIES = ["socks5h://127.0.0.1:2080"]
CURL = shutil.which("curl")
ROOT = Path(__file__).resolve().parent
TOOLS = ROOT / "tools"
ZIG_GLOBS = [
    str(TOOLS / "zig-*" / "zig.exe"),
    r"D:\program\zig*\zig.exe",
    r"C:\cache\tools\zig-*\zig.exe",
]
ARCHES = {"amd64": "x86_64", "arm64": "aarch64", "x86": "x86"}


def setup_libclang() -> None:
    for p in LLVM_PATHS:
        if (Path(p) / "libclang.dll").exists():
            os.environ["LIBCLANG_PATH"] = p
            return
    print(
        "warning: libclang.dll not found; zlob build may fail. Set LIBCLANG_PATH manually.",
        file=sys.stderr,
    )


def parse_zig_version(path: str) -> tuple[int, ...] | None:
    try:
        out = subprocess.run([path, "version"], capture_output=True, text=True, timeout=60)
    except OSError:
        return None
    if out.returncode != 0:
        return None
    m = re.match(r"(\d+)\.(\d+)\.(\d+)", out.stdout.strip())
    return tuple(int(g) for g in m.groups()) if m else None


def http_open(url: str, proxy: str | None, timeout: float):
    handler = urllib.request.ProxyHandler({"http": proxy, "https": proxy}) if proxy else None
    opener = urllib.request.build_opener(handler) if handler else urllib.request.build_opener()
    return opener.open(url, timeout=timeout)


def curl(url: str, dest: Path | None = None) -> bytes:
    last = None
    for proxy in [*PROXIES, None]:
        cmd = [CURL, "-fsSL", "--connect-timeout", "10", "--retry", "2"]
        if proxy:
            cmd += ["--proxy", proxy]
        if dest:
            cmd += ["-o", str(dest)]
        cmd.append(url)
        res = subprocess.run(cmd, capture_output=True)
        if res.returncode == 0:
            if dest:
                print(f"  via {proxy or 'direct'}")
            return res.stdout
        last = (res.stderr or b"").decode(errors="replace").strip()
        print(f"  {proxy or 'direct'} failed: {last}", file=sys.stderr)
    raise RuntimeError(f"cannot fetch {url}: {last}")


def fetch_text(url: str) -> str:
    if CURL:
        return curl(url).decode()
    last = None
    for proxy in [None]:
        try:
            with http_open(url, proxy, 60) as resp:
                return resp.read().decode()
        except Exception as exc:  # noqa: BLE001
            last = exc
    raise RuntimeError(f"cannot fetch {url}: {last}")


def fetch_file(url: str, dest: Path) -> None:
    if CURL:
        curl(url, dest)
        return
    try:
        with http_open(url, None, 300) as resp, dest.open("wb") as out:
            shutil.copyfileobj(resp, out, 1 << 20)
    except Exception as exc:  # noqa: BLE001
        raise RuntimeError(f"cannot download {url}: {exc}") from exc


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def zig_platform_key() -> str:
    machine = ARCHES.get(platform.machine().lower(), platform.machine().lower())
    if sys.platform == "win32":
        return f"{machine}-windows"
    if sys.platform == "darwin":
        return f"{machine}-macos"
    return f"{machine}-linux"


def extract_archive(archive: Path, dest: Path) -> Path:
    dest.mkdir(parents=True, exist_ok=True)
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(dest)
    else:
        with tarfile.open(archive) as tf:
            tf.extractall(dest)
    entries = [p for p in dest.iterdir() if p.is_dir()]
    if len(entries) != 1:
        raise RuntimeError(f"unexpected archive layout in {archive.name}")
    return entries[0]


def download_zig() -> Path | None:
    key = zig_platform_key()
    print(f"zig >= {'.'.join(map(str, MIN_ZIG))} not found, downloading {ZIG_VERSION} ({key})")
    try:
        entry = json.loads(fetch_text(ZIG_INDEX))[ZIG_VERSION][key]
        url, shasum = entry["tarball"], entry["shasum"]
    except Exception as exc:  # noqa: BLE001
        print(f"warning: cannot resolve zig {ZIG_VERSION}: {exc}", file=sys.stderr)
        return None
    TOOLS.mkdir(parents=True, exist_ok=True)
    archive = TOOLS / Path(urllib.request.url2pathname(url)).name
    staging = Path(tempfile.mkdtemp(prefix="zig-extract-", dir=TOOLS))
    try:
        print(f"  {url} ({int(entry['size']) / 1e6:.0f} MB)")
        fetch_file(url, archive)
        digest = sha256_of(archive)
        if digest != shasum:
            raise RuntimeError(f"sha256 mismatch: {digest} != {shasum}")
        target = TOOLS / f"zig-{ZIG_VERSION}"
        if target.exists():
            shutil.rmtree(target)
        shutil.move(str(extract_archive(archive, staging)), target)
        print(f"  installed zig {ZIG_VERSION} -> {target}")
        return target
    except Exception as exc:  # noqa: BLE001
        print(f"warning: zig download failed: {exc}", file=sys.stderr)
        return None
    finally:
        archive.unlink(missing_ok=True)
        shutil.rmtree(staging, ignore_errors=True)


def setup_zig() -> None:
    if os.environ.get("ZIG"):
        return
    candidates = [p for pattern in ZIG_GLOBS for p in glob.glob(pattern)]
    path_zig = shutil.which("zig")
    if path_zig:
        candidates.append(path_zig)
    for candidate in candidates:
        version = parse_zig_version(candidate)
        if version and version >= MIN_ZIG:
            os.environ["ZIG"] = candidate
            os.environ["PATH"] = str(Path(candidate).parent) + os.pathsep + os.environ["PATH"]
            print(f"using zig {'.'.join(map(str, version))}: {candidate}")
            return
    installed = download_zig()
    zig = installed / ("zig.exe" if sys.platform == "win32" else "zig")
    if not zig.exists():
        print("warning: no usable zig; set ZIG to a zig.exe path.", file=sys.stderr)
        return
    os.environ["ZIG"] = str(zig)
    os.environ["PATH"] = str(zig.parent) + os.pathsep + os.environ["PATH"]
    print(f"using zig {ZIG_VERSION}: {zig}")


def main() -> int:
    setup_libclang()
    setup_zig()
    return subprocess.run(
        ["cargo", "build", "--release", "-p", "fff-mcp", "--features", "zlob", *sys.argv[1:]]
    ).returncode


if __name__ == "__main__":
    sys.exit(main())
