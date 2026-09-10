#!/usr/bin/env python3
"""同步本 fork 与上游仓库（模仿 sync-feat-cyx-gbk.ps1）。

默认流程：压缩本地累积提交 -> rebase 到上游 -> force-with-lease 推送到 fork。
用 --merge 改为合并流程（merge 上游后普通 push）。
上游 = origin/main（dmtrKovalenko/fff），fork = doraemoncyx。
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

UPSTREAM = "origin"
UPSTREAM_BRANCH = "main"
FORK = "doraemoncyx"


def force_utf8_output() -> None:
    # 避免 Windows 控制台按 GBK 解码中文输出产生乱码
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            reconfigure(encoding="utf-8", errors="replace")


def git(*args: str) -> str:
    # 原生命令非零退出码不会抛异常，必须显式检查，否则冲突后仍会继续 force push
    proc = subprocess.run(["git", *args], capture_output=True)
    out = proc.stdout.decode("utf-8", errors="replace")
    err = proc.stderr.decode("utf-8", errors="replace")
    if proc.returncode != 0:
        if err.strip():
            print(err.rstrip(), file=sys.stderr)
        raise RuntimeError(f"git {' '.join(args)} failed (exit {proc.returncode})")
    return out.rstrip("\n")


def run(merge: bool) -> int:
    repo = Path(__file__).resolve().parent
    upstream_ref = f"{UPSTREAM}/{UPSTREAM_BRANCH}"

    # 确保本地在上游目标分支
    branch = git("branch", "--show-current")
    if branch != UPSTREAM_BRANCH:
        git("checkout", UPSTREAM_BRANCH)

    # 丢弃本地未提交修改
    git("reset", "--hard")
    print("Reset local changes")

    # 拉取上游最新
    print(f"Fetching {upstream_ref}...")
    git("fetch", UPSTREAM, UPSTREAM_BRANCH)

    if merge:
        print(f"Merging {upstream_ref}...")
        git("merge", upstream_ref, "--no-edit")
        print(f"Pushing to fork {FORK}...")
        git("push", FORK, UPSTREAM_BRANCH)
        return 0

    # 先把本地个人提交压缩成一条，让 rebase 只需重放 1 条
    print("Squashing local commits into one...")
    merge_base = git("merge-base", "HEAD", upstream_ref)
    head = git("rev-parse", "HEAD")
    if merge_base != head:
        msg = git("log", "-1", "--format=%s")
        git("reset", "--soft", merge_base)
        git("commit", "-m", msg)

    print(f"Rebasing onto {upstream_ref}...")
    try:
        git("rebase", upstream_ref)
    except RuntimeError:
        git_dir = Path(git("rev-parse", "--git-dir"))
        if (git_dir / "rebase-merge").exists() or (git_dir / "rebase-apply").exists():
            print(
                "Rebase 因冲突暂停：请手动解决冲突后运行 'git rebase --continue'，"
                "或运行 'git rebase --abort' 放弃本次同步。",
                file=sys.stderr,
            )
            return 1
        raise

    print(f"Force pushing to fork {FORK}...")
    git("push", FORK, UPSTREAM_BRANCH, "--force-with-lease")
    return 0


def main() -> int:
    force_utf8_output()

    parser = argparse.ArgumentParser(description="Sync this fork with upstream.")
    parser.add_argument("--rebase", action="store_true", help="rebase onto upstream (default)")
    parser.add_argument("--merge", action="store_true", help="merge upstream instead of rebasing")
    args = parser.parse_args()

    if args.rebase and args.merge:
        parser.error("--rebase and --merge are mutually exclusive")

    try:
        return run(args.merge)
    except RuntimeError as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
