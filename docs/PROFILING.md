# Profiling a running binary

Four sources, cheapest first. Each one answers a different question, and skipping
straight to the debugger is how a 20-second burst gets mistaken for a steady leak.

## 1. Ask the process: `/stats`

```powershell
curl http://127.0.0.1:8737/stats
```

`snapshot.commits` / `rows` / `dropped` / `queued_bytes` / `busy` are the drain
thread's own counters: `queued_bytes` falling with `commits` rising is healthy,
while `queued_bytes` pinned with `busy: true` means the writer owns the CPU.
`indexes[].content_index` separates the stages behind `indexing`: `pending` (the
walk still runs), `building` (the bigram filter is being built), `ready` (the
prefilter is installed) and `disabled` (content indexing is off). `scanning:
false` with no Root `pending` or `building` means no scan or build is the culprit
— the cost is coming from requests or from a thread outside the index.

## 2. Read the per-run log: what each request cost

Every run writes its own file — `%LOCALAPPDATA%\fff\fff_mcp_http+<ts>+<pid>.log`
on Windows, `<stem>+<ts>+<pid>.log` under the state dir elsewhere — keeping the
last 20 (`retain_runs`). `--log-file` / `log_file` moves it; the path is printed
in the log's first line, so a running process can be matched by its pid:

```powershell
Get-ChildItem "$env:LOCALAPPDATA\fff" -Filter "*+24224.log" | Sort-Object LastWriteTime -Descending
```

Request spans carry `time.busy`. Each names its Root, and a grep also carries the
query length and the mode, so a 24 s call is attributable without guessing from
`file_count`. A real window over one 35k-file Root:

```
grep::grep                 235 calls   15.2s total   2.76s max
fff_search::scan            12 calls    4.9s total   2.18s max
walk::zlob                  21 calls    0.7s total   0.37s max
fff_search::log              6 calls 1113.6s total 1078.0s max   ← lifetime span
```

`fff_search::log` and `rmcp::service` (`serve_inner`) are guard spans around an
Index's whole life and a whole MCP session — Session spans in `GLOSSARY.md`:
their busy is wall clock, never CPU, and one session can cover many requests.
Sorting every `time.busy` descending puts them on top and hides the answer.
Judge by the per-request spans only.

## 3. Timeline: which second burned the CPU

```powershell
pwsh tools/profile.ps1 -Id <pid> -Mode timeline -Samples 30
```

Prints one line per interval — CPU seconds, a bar, and the log events that closed
in that same interval — then the per-thread totals and the span table for the
window. This is the attribution step: it needs no debugger, so nothing perturbs
the process.

The shape it exists to show is burst, not steady state:

```
 10s   0.23s
 11s   8.61s  ################################  grep(35695f)x6
 12s   6.42s  ############################
 13s   0.02s
```

Eight cores for two seconds is six concurrent greps over a 35k-file Root. Between
bursts the same process sits at 0.2–0.5%. A 10-sample stack run that lands in the
gap proves nothing, and one that lands in the burst makes every thread look busy.

## 4. Stacks: which thread, which function

```powershell
pwsh tools/profile.ps1 -Id <pid> -Mode stacks -Samples 15
```

Ranks threads by CPU delta, attaches `cdb.exe` non-invasively (no admin), and
prints the first project frame per hot thread per round. Thread names alone often
answer it: `fff-snapshot-writer`, `fff-bg-N`, `notify-rs debouncer loop`,
`fff-search-N` (rayon).

`-SymPath <dir>` points at the directory holding the `.pdb`; by default it looks
next to the exe, then at this repo's `target\release` and `target\prof`, because
a deployed copy under `windows_utils\` carries no pdb of its own.

A frame in `rayon_core::sleep::...wait_until_cold` is the pool spinning down: the
CPU is real but no work is behind it, and spread over 20 `fff-search-N` threads it
adds up to a phantom total. Trust the timeline for magnitude, the stack for names.

## 5. Symbols: which build has them

A plain `cargo build --release` is stripped — `[profile.release]` sets
`strip = "symbols"` with no `debug` — so its pdb resolves every frame to a
neighbouring public symbol:

```
fff_mcp_http!core::fmt::num::impl$34::fmt+0x2b6837   ← offset over 0x10000
```

An offset that large means "no symbol here", not a slow `fmt`.

```powershell
python build_fff_mcp_http.py
```

That script sets `CARGO_PROFILE_RELEASE_DEBUG=2` and
`CARGO_PROFILE_RELEASE_STRIP=none` for the release profile, so `target\release`
gets a ~130 MB pdb and real frames:

```
fff_mcp_http!fff_search::dbs::snapshot_writer::SnapshotWriter::push
fff_mcp_http!fff_search::index::snapshot_write::RowSink::push
```

`cargo build --profile prof -p fff-mcp-http` is the alternative when the binary
should not be the shipped one; it lands in `target\prof`.

## 6. Whole-system traces

`wpr.exe` / `xperf.exe` come with the Windows Performance Toolkit and give a real
call tree, but need an elevated shell:

```powershell
xperf -on PROC_THREAD+LOADER+PROFILE -stackwalk Profile
# ... reproduce ...
xperf -d trace.etl
```

## Pitfalls

- Sampling output written into a directory that is itself an open Root makes the
  watcher re-index it: a `notify-rs debouncer loop` thread then burns a full core
  and looks like the culprit. `tools/profile.ps1` defaults to `$env:TEMP\fff-profile`.
- `cdb` attach is cheap: 20 attaches in 20 seconds cost a quiet process nothing
  measurable. When a sample shows work, it is the process's own.
- `crates/fff-core/benches/*.rs` cover the hot paths in isolation. Use a bench when
  the question is "is this function fast", the server when it is "what is this
  process doing".
