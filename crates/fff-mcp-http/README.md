# fff-mcp-http

A long-lived file finder that takes a query **and** a target directory (tardir)
per request. Every tardir becomes a **Root** with its own index; indexes for
parent and child directories are kept side by side, merged per request, and
evicted on a memory budget instead of being destroyed on every directory
switch.

```
fff-mcp-http --port 8737 --max-memory 3G
curl 'http://127.0.0.1:8737/search?querystr=*.png&tardir=F:%5Cwork%5Cgame%5Cmhimage'
```

Start it once, then query any directory on any drive without restarting or
re-indexing what is already known.

## Endpoints

| route | purpose |
| --- | --- |
| `GET /search?querystr=…&tardir=…` | same JSON as the MCP tool, for scripts and curl |
| `GET /grep?query=…&tardir=…` | content search, returns `{"text", "scanning", "scanned"}` |
| `GET /multi_grep?patterns=a&patterns=b&tardir=…` | OR content search over several literals |
| `POST /mcp` | Streamable HTTP MCP, stateless, JSON responses (no SSE) |
| `GET /stats` | global memory, budget, and one entry per Index in LRU order |
| `GET /healthz` | liveness probe; never authenticated |

`/search` and the `find_files` MCP tool accept identical parameters, so a
client switching between the stdio `fff-mcp` server and this one only changes
its URL. The same holds for `grep` and `multi_grep`.

```json
{
  "results": [{ "path": "F:\\wg\\mhimage\\data\\a.png", "rel": "data/a.png", "score": 1234, "is_dir": false }],
  "total": 137,
  "scanned": 204311,
  "scanning": false,
  "served_by": ["F:\\wg\\mhimage", "F:\\wg\\mhimage\\data"],
  "merged": true,
  "cursor": "b3f1a0c9d27e4455"
}
```

`total` is the match count, not the page size. `scanning: true` means the
index was still being built and the results are partial — retry the same
request to get the complete set. `served_by` names the Roots that were merged;
`cursor` only appears when another page exists.

## Parameters

Required: `querystr` (aliases `query`, `pattern`; a JSON array is joined with
spaces) and `tardir` (absolute path; the directory is indexed on demand and an
error is returned if it does not exist).

Optional, same semantics as `fff-mcp`: `path`, `paths`, `globs` (`!` prefix
excludes), `hidden` (default `true`), `mode` (`fuzzy` default, `glob`,
`regex`), `filters` (regex AND), `maxResults` (default 20, clamped to
1..=10000), `cursor`. `path` and `paths` are relative to `tardir`; absolute
paths must stay inside it and `..` is rejected.

## Content search

The `grep` and `multi_grep` MCP tools and the `GET /grep` / `GET /multi_grep`
routes search file contents. Both tools require `tardir`, exactly like
`find_files`.

`grep` takes `query` (aliases `pattern` and `querystr`, so the curl name is
the same as `/search`; a JSON array is joined with spaces) plus
`path`, `paths`, `globs`, `hidden`, `mode` (`auto` default, `regex`,
`literal`), `case` (`smart`, `sensitive`, `insensitive`), `maxResults`
(default 100, or 30 with `definitions_only`, clamped to 200), `cursor`,
`output_mode` (`content` default, `files_with_matches`, `count`), `context`,
`before_context`, `after_context`, `max_matches_per_file` and
`definitions_only`.

`multi_grep` takes `patterns` (a string or an array; OR logic), `constraints`
(a `rg`-shaped path/module/glob string), `maxResults` (default 20), `cursor`,
`output_mode`, `context` and `definitions_only`.

Results are text, not JSON, and byte-for-byte the same shape as the stdio
`fff-mcp` server: a file header line, one-based ` line: text` matches, `-` for
context rows, the `0 matches.` / definitions-fallback sentences, and an opaque
`cursor: <id>` footer while another page exists. The routes wrap that text in
`{"text": "...", "scanning": false, "scanned": 204311}`.

Content search runs against **one** Index — the deepest one whose Root covers
the tardir — because paging needs one coherent file list, so Roots are never
merged for grep. A tardir inside a Root is restricted to its own subtree, so a
sibling directory never leaks into the results. When no Index covers the
tardir one is created and the request waits for the initial walk; if
`--scan-wait-secs` expires first, the matches come from the files indexed so
far and the text starts with a note that the result is partial. `scanning:
true` in the route response says the same thing.

Cursors are Root-scoped: reusing a `grep` cursor under a different Root (or a
different tardir that resolves to a different Root) starts from the first page
instead of paging someone else's file list.

## How directory switching behaves

- A deeper tardir inside an existing Root reuses that Root's index and narrows
  the query with a subtree glob — no rescan.
- A parent tardir with child Roots below it queries all of them and merges by
  absolute path, so results arrive before the parent's own scan finishes.
- When the parent's scan completes, the child Roots below it are dropped
  (subsumed); registry state converges regardless of request order.
- A tardir never covered by an existing Root gets its own index.

## Memory

`--max-memory` (default `3G`, accepts `512MB`, `3GB`, bare bytes) bounds the
process footprint as reported by the OS (`PrivateUsage` on Windows, `VmRSS` on
Linux), not an estimate from file counts. When the budget is exceeded the
least recently requested Root is dropped, at most three per round, never one
serving the current request, and never the last one. A single Root larger than
the whole budget is kept with a warning rather than thrashing.

`/stats` reports `used_bytes`, `budget_bytes`, `content_indexing`, and per-Index
`root`/`files`/`dirs`/`scanning`/`indexing`/`prefilter`/`last_used`/
`estimated_bytes`, ordered by least recently used.

## Content index

`--content-indexing` (default on, same as the stdio `fff-mcp` server) builds a
bigram filter per Index after its walk finishes: the set of printable byte pairs
each file contains, one bit per file per pair. `grep` and `multi_grep` use it to
skip files that cannot contain the pattern, which is most of a large tree.

Cost, in exchange:

- Roughly 0.6 KB per indexed file per Root (5000 bit columns), on top of the
  file list. It counts against `--max-memory`, so a multi-Root service reaches
  the eviction threshold sooner — that is the budget doing its job.
- The build reads every file up to 2 MB once, in the background after the walk.
  Until it finishes `grep` still works (it just reads every file), and `/stats`
  reports `indexing: true`; `prefilter: true` marks the point where the prefilter
  is installed. A request never waits for it.
- With `--no-watch` the filter is only as fresh as the initial scan, so edits
  made afterwards can be missed by the prefilter. The same is true whenever a
  filesystem event never reaches the watcher (it can happen under very heavy
  load). Use `--no-content-indexing` when grep must always read current bytes.

`--no-content-indexing` drops the filter: less RAM, no background read pass,
slower grep on big trees.

## Scans

At most `--max-concurrent-scans` (default 2) initial scans run at once; the
rest queue. A request waits at most `--scan-wait-secs` (default 20) and then
answers with whatever is indexed so far. Concurrent requests for the same
tardir share one scan.

## Skipping files

Built-in skips: `.svn/`, `.hg/`, `CVS/`. Disable with `--no-default-skips`;
extend with repeatable `--skip "<gitignore pattern>"`. `--no-git` stops reading
gitignore rules and `--no-ignore-files` stops reading `.ignore`. Skip rules
apply when an Index is created, so changing them affects new Roots only — use a
restart (or a different tardir) to rescan an existing Root.

All of the above require the `zlob` walker; a ripgrep-only build refuses to
start with skip options rather than silently indexing everything.

## Access control

The default bind is `127.0.0.1`. Binding to any other address requires
`--token`; then every endpoint except `/healthz` needs
`Authorization: Bearer <token>`, and `/mcp` additionally validates the `Host`
header against the loopback defaults plus any `allowed_hosts` from the config
file (DNS-rebinding protection).

```powershell
# generate a token
-join ((48..57) + (97..102) | Get-Random -Count 32 | % { [char]$_ })
```

## Configuration file

`--config <path>` accepts a TOML file; command-line flags win, and `skip`
lists are merged.

```toml
skip = ["*.bak", "cache/"]
max_memory = "3G"
bind = "127.0.0.1"
port = 8737
token = "…"
scan_wait_secs = 20
max_concurrent_scans = 2
evict_interval_secs = 30
no_watch = false
follow_symlinks = false
content_indexing = true
frecency_db = "C:/Users/me/AppData/Local/fff/frecency"
log_file = "C:/Users/me/AppData/Local/fff/fff_mcp_http.log"
log_level = "info"
default_skips = true
disable_git = false
disable_ignore_files = false
idle_timeout_secs = 3600
allowed_hosts = ["build-box.local"]
```

## Lifecycle

`--idle-timeout-secs` (default 3600, `0` disables) exits the process after that
long without a request; `/healthz` deliberately does not count as activity.
Ctrl-C and SIGTERM stop every watcher before exiting. Each launch writes its
own `<stem>+<timestamp>+<pid>.log` under `%LOCALAPPDATA%\fff\` (or
`$XDG_STATE_HOME/fff/`), so concurrent instances never share a file.

## Known limitations

- Scores from different Roots are only approximately comparable: `fff-core`
  ranks against each Root's own base path, and rescoring by tardir-relative
  path is not exposed. This only affects path-shape queries during the window
  where a parent is still scanning.
- A parent `grep` waits for the parent's own walk, so child Roots below it are
  not merged into a content page (paging needs one coherent file list). The
  first call on a new tardir waits for the scan, or answers partially after
  `--scan-wait-secs`.
- No index persistence: a restart rescans and rebuilds the content index.

## Building

From the repo root, `build_fff_mcp_http.py` finds or downloads zig and builds
the release binary (`--debug` for the dev profile, extra args go to cargo,
`FFF_FEATURES` overrides the `zlob` default):

```powershell
python build_fff_mcp_http.py
python build_fff_mcp_http.py --debug
```

Or drive cargo yourself:

```powershell
$env:ZIG = 'H:\code\rust\fff\tools\zig-0.17.1\zig.exe'
cargo build -p fff-mcp-http --no-default-features --features zlob
cargo test  -p fff-mcp-http --no-default-features --features zlob
```
