# fff-mcp-http

A long-lived file finder that takes a query **and** a target path (tardir)
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

Any one of `querystr` (canonical), `query` or `pattern` names the query; the
tool's `inputSchema` advertises all three, and a JSON array is joined with
spaces. `tardir` is the only required key, and no identifier key is required:
omitting it is the same as passing `""`, which lists every file in scope.
Passing two identifier keys in one call is a duplicate-field error, not a
precedence rule. `tardir` is an absolute path, a directory or a single file: the
directory — or the file's parent — is indexed on demand and an error is returned
if the path does not exist.

Optional, same semantics as `fff-mcp`: `path`, `paths`, `globs` (`!` prefix
excludes), `hidden` (default `true`), `mode` (`fuzzy` default, `glob`,
`regex`), `filters` (regex AND), `maxResults` (default 20, clamped to
1..=10000), `cursor`. `path` and `paths` are relative to `tardir`; absolute
paths must stay inside it and `..` is rejected.

A file `tardir` scopes the request to exactly that file: its parent directory
becomes the Root (an existing Index covering it is reused), and `rel` in
`/search` results is the bare filename. Naming that file again in `path` or
`paths` changes nothing; naming anything else yields an empty result. When the
file is not in the Index (ignored, binary, empty, or over the size cap), content
search answers `0 matches.` with a note saying so — the file was not searched,
so read it directly instead.

## Content search

The `grep` and `multi_grep` MCP tools and the `GET /grep` / `GET /multi_grep`
routes search file contents. Both tools require `tardir`, exactly like
`find_files`.

`grep` takes any one of `query` (canonical), `pattern` and `querystr` — the same
three keys are advertised on both servers, so the curl name matches `/search`; a
JSON array is joined with spaces. Omitting all three means an empty query, which
matches nothing. It also takes `path`, `paths`, `globs`,
`hidden`, `mode` (`auto` default, `regex`, `literal`), `case` (`smart`,
`sensitive`, `insensitive`), `maxResults` (default 100, or 30 with
`definitions_only`, clamped to 200), `cursor`, `output_mode` (`content` default,
`files_with_matches`, `count`), `context`, `before_context`, `after_context`,
`max_matches_per_file` and `definitions_only`.

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
sibling directory never leaks into the results; a tardir that names a file is
restricted to that one file. When no Index covers the tardir one is created and
the request waits for the initial walk; if
`--scan-wait-secs` expires first, the matches come from the files indexed so
far and the text starts with a note that the result is partial. `scanning:
true` in the route response says the same thing.

Cursors are Root-scoped: reusing a `grep` cursor under a different Root (or a
different tardir that resolves to a different Root) starts from the first page
instead of paging someone else's file list.

## How directory switching behaves

- A deeper tardir inside an existing Root reuses that Root's index and narrows
  the query with a subtree glob — no rescan.
- A tardir that names a file is answered from its parent directory's Index,
  narrowed to that one file; no per-file Index is created.
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

`/stats` reports `used_bytes`, `budget_bytes`, `content_indexing`,
`refused_roots`, and per-Index
`root`/`files`/`dirs`/`scanning`/`indexing`/`prefilter`/`last_used`/
`estimated_bytes`, ordered by least recently used. `snapshot` holds the disk
cache's `hits`/`misses`/`bytes`/`roots` and its `map_bytes`/`map_ceiling`/
`map_grows`, or `null` when it is not in use.

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

## Snapshot

A **Snapshot** is the on-disk copy of one Root's Content index: for each file, its
bigrams, its Binary verdict, and the file identity (`size`, nanosecond `mtime`)
the row is valid for. It holds no file or directory tables — every scan still
walks the disk and rebuilds those — and it answers no request on its own: a row
counts only once an Index adopts it and confirms the file still matches. A file
whose identity no longer matches is **stale** for that row alone; one stale file
never discards the rest of the Snapshot.

By default the environment lives in the platform cache directory:
`%LOCALAPPDATA%\fff\snapshot-db` on Windows, `~/.cache/fff/snapshot-db`
elsewhere. `--snapshot-db <PATH>` names a different environment directory, with
the same semantics as `--frecency-db`; `--snapshot-reset` deletes the
environment's data before opening it.

- `--no-snapshot` turns the Snapshot off, and `--snapshot-budget-mb 0` does the
  same. Off means nothing is written and nothing is read.
- `--snapshot-budget-mb <MB>` (default `512`) caps the disk every Root's rows may
  use. After each batch commit the store deletes whole Roots, least recently used
  first, until the total fits; a Root unused for 14 days is deleted in the same
  pass, and a Root that is being scanned is never a victim.
- Writing never delays a scan: rows go to one writer thread that commits in
  batches of 2000. If that thread stalls the oldest pending batch is dropped, and
  a lost batch is a row miss on the next scan, never a wrong answer.
- The database file does not shrink after deletions: LMDB keeps the space a
  deleted Root occupied, so `data.mdb` stays at its high-water mark.
  `--snapshot-reset` is the way to reclaim it.
- The map LMDB writes into starts at 2 GiB and grows by doubling when a commit
  answers `MDB_MAP_FULL`, up to the free space of the volume minus a reserve
  (1 GiB, or 5% of the volume if that is larger). Unlike the budget, this is not
  a knob: it exists so a longer cache never silently stops being written, and it
  never takes the last gigabyte of a disk. A commit that cannot grow is a lost
  batch and the usual one `warn` line.

A second scan of an unchanged tree reads no file contents at all: it replays each
row into the same content-index builder the file pass uses, then serves grep from
that index exactly as before.

Every failure — a corrupt environment, a directory it did not create, an
unwritable path, a full disk, a lost write race — ends the same way: one `warn`
line naming the cause, the Snapshot off for the rest of the process, and scans
that read files exactly as they did before the feature existed. No error reaches
the request. A latched-off Snapshot is silent afterwards, never one warning per
file.

An environment that will not open is recreated only when its directory carries
the store's own `.fff-snapshot` marker; an unmarked directory is never touched,
and `--snapshot-reset` obeys the same rule. This is the one guard between a
mistyped `--snapshot-db` and someone's data: two LMDB environments are
indistinguishable by content, so a content test cannot tell a corrupt Snapshot
from the frecency database living at the wrong path.

`/stats` gains a `snapshot` object once a store is open:

```json
"snapshot": {
  "hits": 120848, "misses": 0, "bytes": 80936960, "roots": 1,
  "map_bytes": 2147483648, "map_ceiling": 695784701952, "map_grows": 1,
  "commits": 412, "rows": 120848, "dropped": 0, "queued_bytes": 0, "busy": false
}
```

`hits` and `misses` are process-wide and accumulate across scans (a scan's own
line is the `snapshot hit rate` log entry, with its content reads and sniffed
count); `bytes` and `roots` come from the live budget index. `map_bytes` is the
map LMDB may use, `map_ceiling` the volume limit it grows towards (`null` when
the volume cannot be measured), and `map_grows` how many times it has grown this
process. `commits`, `rows`,
`dropped`, `queued_bytes` and `busy` are the drain thread's own counters.
`dropped` counts batches the queue could not hold: each one is rows missing
until the next scan of that Root writes them, so a non-zero `dropped` means
the Snapshot is not fully populated — never that an answer was wrong. A
`queued_bytes` that stops falling while `commits` stalls is a writer occupying
the CPU, not a slow grep. `null` means no Snapshot is in use — off by config,
unopenable, or latched off by a failure — never a zeroed object that looks like a
cold cache. `docs/PROFILING.md` covers what to do when these numbers point at
the writer.

## Scans

At most `--max-concurrent-scans` (default 2) initial scans run at once; the
rest queue. A request waits at most `--scan-wait-secs` (default 20) and then
answers with whatever is indexed so far. Concurrent requests for the same
tardir share one scan.

## Root cap

`--root-cap-files <N>` (default `500000`, `0` = no cap) bounds how many files a
tardir may hold and still become a Root. A tardir that would cross the cap is
not indexed at all: the walk stops at the crossing, so no Index, no watcher and
no Snapshot row is ever created for it, and the request fails with
`INVALID_PARAMS` naming the cap, the file count the walk stopped at, and that
directory's top-level subdirectories — the candidates for a narrower retry.

A request cannot raise the cap: `path`, `paths` and `globs` narrow a tardir,
they never rescue one, so a request naming several sub-paths of a refused
directory fails as a whole instead of answering for the parts that fit.

The check is a walk of its own, capped the same way and run before an Index
exists or a scan slot is taken. Refusing a 2M-file tree therefore costs one walk
up to the cap, not a scan of the tree; an under-cap directory pays one extra
walk each time it becomes a Root.

`/stats` reports `refused_roots`, and each refusal logs one `warn` line with the
Root and the count — the numbers to judge 500,000 by. A Root that is already
indexed is never re-gated; the cap decides whether a directory becomes a Root.

## Binary extensions

`--extra-binary-extension <EXT>` (repeatable) and the `extra_binary_extensions`
TOML list add extensions to the built-in binary table, for the resource formats
of one machine (`.mtl`, `.mha`, `.atlas`) that the table does not know. An added
extension is a Binary verdict by name: such files are never opened to decide,
and grep skips them without reading a byte.

The list adds, it never replaces: no value removes a built-in entry and there is
no switch to empty the table. Values are trimmed, a leading dot is dropped, case
is folded and repeats are one entry, so `mtl`, `.MTL` and ` mtl ` are one
extension. A value that is not a bare extension (`tar.gz`, a path, `!png`) or
that names a text extension (`.rs`, `.md`) fails the startup with an
`extra_binary_extensions: …` message instead of being ignored — text read as
binary is a silently missed grep match.

The startup log line `binary extension table` carries `builtin` and `merged`
counts, so "did my extension take effect" is one look at the log. The table is
fixed for the process; a change needs a restart.

## Skipping files

Built-in skips: `.svn/`, `.hg/`, `CVS/`. Disable with `--no-default-skips`;
extend with repeatable `--skip "<gitignore pattern>"`. `--no-git` stops reading
gitignore rules and `--no-ignore-files` stops reading `.ignore`. Skip rules
apply when an Index is created, so changing them affects new Roots only — use a
restart (or a different tardir) to rescan an existing Root.

All of the above require the `zlob` walker; a ripgrep-only build refuses to
start with skip options rather than silently indexing everything.

## Git status

`git_status` (TOML) / `--git-status` reads each Root's dirty files so they can
outrank clean ones; `--no-git-status` turns it off and is the default. Off, a
scan reads no status, no `GitStatusWorker` thread starts, and the watcher does
not subscribe to `.git` — so nothing in the log, no `refresh_git_status` span
and no per-event `read_status` for any Root.

The trade is ranking only: **the set of files a query returns is identical**,
a dirty file just no longer gets its boost. `git:` / `status:` query filters
have nothing to read when it is off, so they match no file. Ignore rules are
untouched — `.gitignore` / `.ignore` skipping belongs to `disable_git` and
`disable_ignore_files`, which stay independent of this switch.

## Symlinked directories

Files inside a symlinked directory are indexed even with `--follow-symlinks`
off (the zlob Windows backend cannot follow links, so the walk marks the link
and a second pass adds its target's files). That pass stops at 50k added files
and 32 levels deep, whichever comes first. `--no-symlink-expand` skips it
entirely — the link is still listed, its contents are not. Use it for Roots
whose links point at resource trees (game assets, another checkout), where the
expansion adds thousands of files the Root does not own.

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

`--config <path>` accepts a TOML file; command-line flags win, and `skip` and
`extra_binary_extensions` lists are merged.

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
git_status = false
idle_timeout_secs = 3600
allowed_hosts = ["build-box.local"]
snapshot_db = "C:/Users/me/AppData/Local/fff/snapshot-db"
no_snapshot = false
snapshot_budget_mb = 512
snapshot_reset = false
root_cap_files = 500000
extra_binary_extensions = ["mtl", "atlas"]
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
