# Profiling a running binary

`/stats` says *what* the process is doing; a debugger says *which thread* is
burning the CPU. Start with `/stats`, and reach for the debugger only when the
answer is "something is slow" rather than "the writer is behind".

## 1. Ask the process

```powershell
curl http://127.0.0.1:8737/stats
```

`snapshot.commits` / `rows` / `queued_bytes` / `busy` are the drain thread's own
counters. `queued_bytes` falling with `commits` rising is healthy; `queued_bytes`
pinned and `busy: true` means the writer, not a grep, owns the CPU.
`indexes[].indexing: true` means the bigram build is still running for that Root.

## 2. Sample the hot thread

```powershell
pwsh tools/profile.ps1 -Id <pid> -Samples 15 -IntervalMs 1000
```

It ranks threads by CPU delta, attaches `cdb.exe` non-invasively (no admin), and
prints the top frame per hot thread per round. `-SymPath <dir>` points at the
directory holding the `.pdb`; it defaults to the exe's own directory.

The script prints `Thread Name` even with no symbols — often that is the answer,
since the drain threads are named (`fff-snapshot-writer`, `fff-bg-N`,
`notify-rs debouncer loop`).

## 3. Get function names

`[profile.release]` sets `strip = "symbols"` with no `debug`, so the release pdb
resolves frames to a *neighbouring* public symbol:

```
fff_mcp_http!core::fmt::num::impl$34::fmt+0x2b6837   ← offset over 0x10000
```

An offset that large means "no symbol here", not a slow `fmt`. Build the profile
that keeps debug info instead:

```powershell
$env:ZIG = "$pwd\tools\zig-0.17.1\zig.exe"
$env:PATH = "$pwd\tools\zig-0.17.1;$env:PATH"
cargo build --profile prof -p fff-mcp-http
pwsh tools/profile.ps1 -Id <pid> -SymPath target\prof
```

To see the new binary's cost instead of the running release, start it from
`target\prof\` with the same `--config`.

## 4. Whole-system traces

`wpr.exe` / `xperf.exe` come with the Windows Performance Toolkit and give a
real call tree, but need an elevated shell:

```powershell
xperf -on PROC_THREAD+LOADER+PROFILE -stackwalk Profile
# ... reproduce ...
xperf -d trace.etl
```

## Pitfalls

- Writing sampling output into a directory that is itself an open Root makes the
  watcher re-index it: a `notify-rs debouncer loop` thread then burns a full core
  and looks like the culprit. Keep output outside every indexed root;
  `tools/profile.ps1` defaults to `$env:TEMP\fff-profile`.
- `crates/fff-core/benches/*.rs` cover the hot paths in isolation. Use a bench
  when the question is "is this function fast" and the server when it is "what
  is this process doing".
