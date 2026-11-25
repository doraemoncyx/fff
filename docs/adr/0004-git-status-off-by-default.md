# Read no git status by default in fff-mcp-http

**Status**: accepted

`fff-mcp-http` adds a `git_status` setting (TOML key `git_status`, flags
`--git-status` / `--no-git-status`) whose default is `false`. It is passed into
`FilePickerOptions::git_status`; with it off the whole chain is absent, not
merely skipped late:

- the scan reads no initial status and never spawns `GitStatusWorker`, so
  `SharedPicker::refresh_git_status` is never called;
- the watcher does not subscribe to `.git`, ignores `.git` events for status
  purposes and enqueues no per-path status update, so `GitStatusCache::read_status`
  never runs;
- `/stats` grows no fields: it never reported git state in the first place.

`disable_git` is untouched. It controls whether `.gitignore` rules are read at
all; `git_status` only controls whether the *dirty state* of an already-indexed
file is read. A Root with `git_status = true, disable_git = true` still reads
statuses and simply applies no gitignore rules.

## Considered Options

**Keep the default on and let operators turn it off.** Rejected: the measured
cost is on the startup path. `refresh_git_status` walks the whole repository
status once per Root and has been observed at 99 s on a large working copy; a
service that indexes many Roots pays it per Root, for a signal that only moves
a file up or down among already-matching results. A default is what the common
case does, and the common case here is an agent asking for files, not a diff.

**Reuse `disable_git` as the off switch.** Rejected: it would silently start
indexing `.gitignore`d files — build output, secrets, `node_modules` — which is
a change to *which files exist*, not to their ranking. Two consequences that
look alike in a log file must not share a switch.

**Cache or lazily schedule the status read.** Rejected as the default: any
schedule still needs a first full read to know what is dirty, so it buys
nothing at startup, and it reintroduces the long libgit2 pass this ticket
exists to remove. The opt-in switch is enough.

## Consequences

- Off, a dirty file loses only its `git_status_boost`: the returned **set** is
  unchanged for a normal query. `git:` / `status:` query filters have no status
  to filter on and therefore match nothing — an honest consequence of not
  reading status, and the reason the flag exists rather than a silent removal.
- nvim, python and C keep the picker default (`true`): nothing about their
  interactive behavior changes, they just gained a switch they do not set.
- Turning it on restores exactly the previous behavior, including the
  `refresh_git_status` and `read_status` spans in the log.
- The service does not persist or report the switch in `/stats`: the absence of
  the spans and of the worker thread is the observable state.
