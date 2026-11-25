# Refuse a tardir over the Root cap instead of degrading it

**Status**: accepted

A tardir that would cross the Root cap never becomes a Root.
`crates/fff-core/src/root_cap.rs` walks the directory with the same walker, the
same ignore rules and the same symlink setting a scan would use, with the cap as
its `WalkLimits` limit; the walk stops one entry past the cap, and the directory
is refused. `crates/fff-mcp-http/src/registry.rs` runs that check before it takes
a scan slot and before it creates a Picker, so a refused tardir has no Index, no
watcher and no Snapshot row — there is nothing to clean up afterwards, because
nothing was built.

The default is 500,000 files, `--root-cap-files` / `root_cap_files`, `0` for no
cap. That number sits between the two kinds of tree this service actually meets:
a 194,000-file working copy it must index, with about three times of headroom,
and the 1,570,000-file code workspace root and 2,360,000-file resource tree it
must not, with about three times of margin on the other side. The rejection
message carries the cap, the file count at the crossing and the directory's
top-level subdirectories — the candidates for a narrower tardir — and no other
file statistic, because a capped walk's count is where the walk stopped, not
what the tree holds, and a number in an error message gets repeated as fact.

## Considered Options

**Scan the whole tree, then judge.** Rejected: it is the same refusal with the
cost of the whole scan paid first. A rejected 2.36M-file tree would be walked in
full, its memory allocated, and only then thrown away. The cap exists to keep
that scan from happening, so a judge that needs it has already lost.

**Degrade to a depth-limited or partial walk.** Rejected: a Root that silently
holds half a tree answers every later request wrongly, and the answer looks
exactly like a complete one. The service would have to remember, per Index, that
it is partial, and every consumer of it — `/search`, `grep`, the merge between
Roots, the Snapshot — would have to honour that flag. The failure mode is a
silent omission in a file picker, which is the one kind of error a user cannot
detect by reading the result.

**Let the client raise or disable the cap per request.** Rejected: the cap
protects the process from the client's own request. A cap a caller can override
is a default, and the caller that needs it most — an agent pointed at `H:\code`
— is exactly the one that will pass `0`.

**Count files from the directory metadata instead of walking.** Rejected: no
portable way to count a tree without walking it, and the walk is what applies
the ignore rules; a count that disagrees with what the scan would index would
refuse trees that fit.

## Consequences

- The check is a walk of its own, so an under-cap directory is walked twice on
  Root creation: once to admit it, once to index it. It is bounded by the cap
  and it is the price of refusing before anything exists; a tree far over the
  cap pays it only up to the crossing, which is the point.
- The gate stops at `cap + 1` files at best and `cap + threads` at worst: the
  walker decides on the collected count, and in-flight threads each finish their
  entry. The reported count is therefore always slightly past the cap.
- Exactly `cap` files is admitted. The cap bounds the tree, it does not equal
  the refusal threshold, and the walker's `exceeded` keeps that distinction for
  both backends.
- A multi-root request fails wholly: the refusal is an error from the one place
  that creates Indexes, and there is no path that keeps the roots it managed to
  create.
- The gate counts the walk's file entries, so the symlink expansion that adds up
  to 50,000 files after the walk can push an admitted Root past the cap. Accepted:
  the cap is a guard against a tree two orders of magnitude too big, not a
  guarantee held per file.
- A Root that is already indexed is never re-gated: the cap decides whether a
  directory becomes a Root, not whether an existing one stays. A tree that grows
  past the cap keeps its Index until it is evicted; re-indexing it then refuses.
- Each refusal increments `refused_roots` in `/stats` and logs one `warn` line
  with the Root and the count. The 500,000 is meant to be re-judged from those
  numbers rather than guessed again.

_Amended_: `refused_roots` is now the list of held refusal records rather than a
counter, and a repeat request for a refused directory is answered from its record
without a second walk. The gate itself is unchanged — a refused tardir is still
never indexed, watched or snapshotted.
