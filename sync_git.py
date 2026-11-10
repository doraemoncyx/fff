#!/usr/bin/env python3
"""把 fork 的个人分支同步到 upstream 最新（默认 merge upstream 后普通推送）。

Usage:
  python sync_git.py                        # 检查 → 确认 → 同步（检查默认开启，同步需 --yes 或交互 y）
  python sync_git.py --check                # 只做工作前检查并打印，不改动任何东西
  python sync_git.py --yes                  # 检查通过后不再确认，直接同步
  python sync_git.py --rebase               # 用 rebase 代替 merge（改写提交、需强推）
  python sync_git.py --discard              # 允许丢弃已跟踪文件上的未提交修改
  python sync_git.py --branch main --fork origin --upstream upstream

流程（破坏性动作之前先做完所有检查）：
  1. 工作前检查（默认执行，--check 可只看结果）：识别 remote、确认分支存在、工作区干净、fork 可写
  2. 任何一项 FAIL 都在动手前中止；WARN 提示但不拦（如缺少 upstream remote、--discard 将丢改动）
  3. 检查通过后仍需确认（--yes 或交互 y），非交互环境没有 --yes 一律不动
  4. fetch upstream 与 fork 两个分支：后者给 --force-with-lease 提供新鲜比较基线
  5. 默认模式：merge upstream 到本地分支再普通 push，代价 ~ 分叉规模、不改写提交；
     --rebase 模式则逐条 rebase 到 upstream 后带 lease 强推

本仓库的现状：唯一 remote 是 origin = doraemoncyx/fff（fork），上游 dmtrKovalenko/fff
尚未配置，会在检查通过后按 --upstream-url 自动添加为 upstream。

merge 而非 squash/rebase 的原因：这是持续开发的活 fork，保留每条特性提交才能在
upstream..fork 里按特性查看与挑选；merge 每次只解一次冲突，代价不随提交数增长，
且普通 push 不改写 SHA。

fork 与 upstream 必须先分清：认错一次就会把 force push 打到上游仓库，上游没有写
权限，表现为 git push 退出码 128 加 Permission denied——所以推之前必须先探测可写性。

rebase 因冲突暂停时只提示手动处理并退出，绝不继续推送。
"""

import argparse
import ctypes
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent

# 默认目标：本仓库本地分支叫 main，上游仓库是 dmtrKovalenko/fff
UPSTREAM_REPO = "dmtrKovalenko/fff"
DEFAULT_BRANCH = "main"
DEFAULT_UPSTREAM_URL = f"git@github.com:{UPSTREAM_REPO}.git"

# 可写性探测用的临时 ref 名，只在 --dry-run 里出现，不会真的落到远端
PROBE_REF = "fff-sync-probe"

RESET = "\033[0m"
CYAN = "\033[36m"
YELLOW = "\033[33m"
RED = "\033[31m"


def log(message: str, color: str = "") -> None:
    # 只在交互式终端着色，重定向到文件时保持纯文本
    if color and sys.stdout.isatty():
        print(f"{color}{message}{RESET}", flush=True)
        return
    print(message, flush=True)


def git(args: list[str], capture: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=ROOT,
        capture_output=capture,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def git_checked(args: list[str], capture: bool = False) -> subprocess.CompletedProcess:
    # 关键步骤失败即中止，避免在错误状态下继续 rebase / push
    result = git(args, capture)
    if result.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} 失败（退出码 {result.returncode}）")
    return result


def git_probe(args: list[str]) -> str | None:
    # 用于「失败属正常」的探测，例如 remote/分支是否存在
    result = subprocess.run(
        ["git", *args],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def use_utf8_console() -> int | None:
    # git 会输出 UTF-8 的中文提交信息，默认 GBK 控制台会乱码；
    # 把控制台代码页切到 UTF-8，返回原代码页供还原（非 Windows 或无控制台时为 None）。
    previous = None
    if os.name == "nt":
        try:
            previous = ctypes.windll.kernel32.SetConsoleOutputCP(65001)
        except OSError:
            previous = None
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    return previous


def restore_console(codepage: int | None) -> None:
    if codepage:
        ctypes.windll.kernel32.SetConsoleOutputCP(codepage)


def rebase_in_progress() -> bool:
    git_dir = git_probe(["rev-parse", "--git-dir"])
    if git_dir is None:
        return False
    path = Path(git_dir)
    if not path.is_absolute():
        path = ROOT / path
    return (path / "rebase-merge").exists() or (path / "rebase-apply").exists()


def merge_in_progress() -> bool:
    git_dir = git_probe(["rev-parse", "--git-dir"])
    if git_dir is None:
        return False
    path = Path(git_dir)
    if not path.is_absolute():
        path = ROOT / path
    return (path / "MERGE_HEAD").exists()


def repo_slug(url: str) -> str:
    """把 remote URL 归一成 owner/repo，用来判断某个 remote 是不是上游仓库。
    支持 git@host:owner/repo.git、https://host/owner/repo、ssh://git@host/owner/repo。"""
    path = url.strip().rstrip("/").removesuffix(".git")
    path = path.split("://", 1)[-1].split("@", 1)[-1]
    # scp 语法的冒号在第一段里（git@github.com:owner/repo），换成路径分隔符
    head, sep, tail = path.partition("/")
    return (head.replace(":", "/") + sep + tail).split("/", 1)[1].lower() if sep else ""


def remotes() -> dict[str, str]:
    return {name: git_probe(["remote", "get-url", name]) or "" for name in (git_probe(["remote"]) or "").split()}


def pick(candidates: list[str], preferred: tuple[str, ...]) -> str | None:
    for name in preferred:
        if name in candidates:
            return name
    return candidates[0] if candidates else None


def resolve_remotes(fork: str | None, upstream: str | None) -> tuple[str, str, dict[str, str]]:
    """分清哪个 remote 是自己的 fork、哪个是上游：URL 归一到上游仓库的算 upstream，
    其余都是 fork。同一个仓库配了多个 remote 时按 preferred 的命名顺序取。"""
    urls = remotes()
    resolved_upstream = upstream or pick(
        [name for name, url in urls.items() if repo_slug(url) == UPSTREAM_REPO], ("upstream", "origin")
    )
    if resolved_upstream is None:
        # 仓库里还没有任何指向上游的 remote，稍后按 --upstream-url 添加
        resolved_upstream = "upstream"
    resolved_fork = fork or pick([name for name in urls if name != resolved_upstream], ("fork", "origin"))
    if resolved_fork is None:
        raise SystemExit("找不到 fork remote，请用 --fork 指定")
    return resolved_fork, resolved_upstream, urls


def writable(remote: str) -> tuple[bool, str]:
    # dry-run 推一个一次性 ref：可写时服务端校验通过（实际不落库），只读时返回 Permission denied
    result = git(["push", "--dry-run", remote, f"HEAD:refs/heads/{PROBE_REF}"], capture=True)
    return result.returncode == 0, (result.stderr or "").strip()


def preflight(args: argparse.Namespace) -> tuple[str, str, list[tuple[str, str]]]:
    """动手之前把逻辑全部查一遍：remote 映射对不对、分支在不在、工作区干不干净、
    fork 到底能不能写。全是只读操作，结果交给 report 打印。"""
    fork, upstream, urls = resolve_remotes(args.fork, args.upstream)
    checks: list[tuple[str, str]] = [("ok", f"fork remote = {fork}（{urls[fork]}）")]
    if upstream in urls:
        checks.append(("ok", f"upstream remote = {upstream}（{urls[upstream]}）"))
    else:
        checks.append(("warn", f"没有指向上游 {UPSTREAM_REPO} 的 remote，检查通过后按 --upstream-url 添加 {upstream}"))

    if git_probe(["rev-parse", "--verify", f"refs/heads/{args.branch}"]) is None:
        checks.append(("fail", f"本地没有分支 {args.branch}，请先创建再同步"))
    else:
        checks.append(("ok", f"本地分支 {args.branch} 存在"))

    dirty = (git_probe(["status", "--porcelain", "--untracked-files=no"]) or "").splitlines()
    if not dirty:
        checks.append(("ok", "工作区（已跟踪文件）干净"))
    elif args.discard:
        checks.append(("warn", f"{len(dirty)} 处未提交修改，--discard 将丢弃它们"))
    else:
        checks.append(("warn", f"{len(dirty)} 处未提交修改，将用 --autostash 暂存、同步后还原"))

    ok, detail = writable(fork)
    if ok:
        checks.append(("ok", f"fork {fork} 可写（push --dry-run 探测）"))
    else:
        checks.append(("fail", f"fork {fork} 不可写（{urls[fork]}）：{detail}\n用 --fork 指向自己的 fork"))
    return fork, upstream, checks


def report(checks: list[tuple[str, str]]) -> list[str]:
    # 检查结果全部打出来再决定要不要动手，避免错了才发现是 remote 认错
    marks = {"ok": ("OK  ", CYAN), "warn": ("WARN", YELLOW), "fail": ("FAIL", RED)}
    log("工作前检查：", CYAN)
    failures = []
    for status, label in checks:
        mark, color = marks[status]
        log(f"  [{mark}] {label}", color)
        if status == "fail":
            failures.append(label)
    return failures


def confirm() -> bool:
    # 破坏性动作的确认闸门：非交互环境（管道/CI）必须显式给 --yes，否则一律不执行
    if not sys.stdin.isatty():
        return False
    try:
        reply = input("确认执行同步并推送到 fork？[y/N] ")
    except (EOFError, KeyboardInterrupt):
        # stdin 是 TTY 但已到 EOF（或被 Ctrl-C），当作「否」
        print(flush=True)
        return False
    return reply.strip().lower() in ("y", "yes")


def push(fork: str, branch: str, lease: str | None) -> None:
    refspec = f"{branch}:{branch}"
    if lease is None:
        # fork 上还没有这个分支，普通 push 即可，不需要 force
        log(f"正在推送到 {fork} ...", CYAN)
        git_checked(["push", fork, refspec])
        return
    log(f"正在 force push 到 {fork}（lease {lease[:9]}）...", CYAN)
    git_checked(["push", fork, refspec, f"--force-with-lease={branch}:{lease}"])


def main() -> None:
    parser = argparse.ArgumentParser(description="同步 fork 分支到 upstream 最新")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--rebase", action="store_true", help="逐条 rebase 到 upstream 后强推（改写提交，默认关）")
    mode.add_argument("--merge", action="store_true", help="merge upstream 后普通 push（默认）")
    parser.add_argument("--discard", action="store_true", help="允许丢弃已跟踪文件上的未提交修改")
    parser.add_argument("--check", action="store_true", help="只做工作前检查（remote 映射/分支/工作区/可写性）后退出，不改动任何东西")
    parser.add_argument("--yes", "-y", action="store_true", help="检查通过后不再确认，直接执行同步")
    parser.add_argument("--branch", default=DEFAULT_BRANCH, help=f"要同步的本地/fork 分支（默认 {DEFAULT_BRANCH}）")
    parser.add_argument("--fork", help="fork remote 名（默认自动识别）")
    parser.add_argument("--upstream", help="upstream remote 名（默认自动识别）")
    parser.add_argument(
        "--upstream-branch",
        default=DEFAULT_BRANCH,
        help=f"upstream 上要同步的分支（默认 {DEFAULT_BRANCH}）",
    )
    parser.add_argument("--upstream-url", default=DEFAULT_UPSTREAM_URL, help="缺少 upstream remote 时使用的地址")
    args = parser.parse_args()

    codepage = use_utf8_console()
    try:
        sync(args)
    finally:
        restore_console(codepage)


def sync(args: argparse.Namespace) -> None:
    fork, upstream, checks = preflight(args)
    failures = report(checks)
    if args.check:
        if failures:
            raise SystemExit(1)
        log("检查通过，未做任何改动（--check）", CYAN)
        return
    if failures:
        raise SystemExit("工作前检查未通过，已中止，未做任何改动")
    # 检查默认就做，同步本身要再确认一次；管道/CI 里没有 --yes 就不动
    if not args.yes and not confirm():
        raise SystemExit("已取消，未做任何改动（加 --yes 或交互确认才会执行同步）")

    # 检查通过才开始动手，顺序：改 remote / 丢修改 / 切分支 / fetch / merge(或 rebase) / push
    if upstream not in remotes():
        git_checked(["remote", "add", upstream, args.upstream_url])
        log(f"已添加 {upstream} remote", CYAN)
    if args.discard and git_probe(["status", "--porcelain", "--untracked-files=no"]):
        git_checked(["reset", "--hard"])
        log("已丢弃未提交修改", CYAN)
    if git_probe(["branch", "--show-current"]) != args.branch:
        git_checked(["checkout", args.branch])

    upstream_ref = f"{upstream}/{args.upstream_branch}"
    log(f"正在 fetch {upstream_ref} ...", CYAN)
    git_checked(["fetch", upstream, args.upstream_branch])
    # fork 也要 fetch：--force-with-lease 拿 fork/<branch> 当比较基线，基线过期等于没保护
    git_checked(["fetch", fork, args.branch])
    lease = git_probe(["rev-parse", "--verify", f"{fork}/{args.branch}"])

    ahead = int(git_probe(["rev-list", "--count", f"{upstream_ref}..{args.branch}"]) or "0")
    if ahead == 0:
        git_checked(["reset", "--hard", upstream_ref])
        log("本地没有独有提交，已直接对齐 upstream，无需推送", CYAN)
        return

    if args.rebase:
        # rebase 会改写提交，先留一个本地备份 ref 兜底（只在本仓库，不会被推送）
        backup = f"refs/backup/sync/{time.strftime('%Y%m%d-%H%M%S')}"
        git_checked(["update-ref", backup, "HEAD"])
        log(f"已创建备份 ref {backup}", CYAN)
        log(f"正在 rebase 到 {upstream_ref}（保留每条提交）...", YELLOW)
        result = git(["rebase", "--autostash", upstream_ref])
        if result.returncode != 0:
            # 冲突时 rebase 会暂停在中间，交由用户手动解决，绝不继续推送
            if rebase_in_progress():
                log(
                    "Rebase 因冲突暂停：请手动解决冲突后运行 'git rebase --continue'，"
                    "或运行 'git rebase --abort' 放弃本次同步。",
                    RED,
                )
                raise SystemExit(1)
            raise SystemExit(f"git rebase 失败（退出码 {result.returncode}）")
        # rebase 没报错不代表结果正确，确认 HEAD 真的在 upstream 之上再推
        if git(["merge-base", "--is-ancestor", upstream_ref, "HEAD"]).returncode != 0:
            raise SystemExit(f"rebase 后 HEAD 不包含 {upstream_ref}，已中止推送")
    else:
        log(f"正在 merge {upstream_ref} ...", YELLOW)
        result = git(["merge", "--autostash", upstream_ref, "--no-edit"])
        if result.returncode != 0:
            # 冲突时 merge 会暂停在中间，交由用户手动解决，绝不继续推送
            if merge_in_progress():
                log(
                    "Merge 因冲突暂停：请手动解决冲突后运行 'git commit' 完成合并，"
                    "或运行 'git merge --abort' 放弃本次同步。",
                    RED,
                )
                raise SystemExit(1)
            raise SystemExit(f"git merge 失败（退出码 {result.returncode}）")

    # 本地与 fork 分支已一致时没有可推的内容，避免无谓推送
    if lease is not None and git_probe(["rev-parse", "HEAD"]) == lease:
        log(f"{fork}/{args.branch} 已是最新，无需推送", CYAN)
        return
    # rebase 改写了历史，必须带 lease 强推；merge 只在旧提交之上追加，普通 push 即可
    push(fork, args.branch, lease if args.rebase else None)
    log(f"完成：{fork}/{args.branch} 已同步到 {upstream_ref}", CYAN)


if __name__ == "__main__":
    main()
