# Persist the content index as per-file bigram rows, not as columns

**Status**: proposed

A Snapshot stores one row per file — that file's sorted bigram keys, plus its
binary verdict, `size` and nanosecond `mtime` — and an adopting Index rebuilds
its columns by replaying those rows through the same `BigramIndexBuilder` a full
scan uses. This is what the service caches; the file table is not cached at all.

Measured on a 137,756-file tree: building the content index takes 10.9 s against
a 1.06 s walk, i.e. 72% of a 15.14 s cold scan. That ratio is the whole reason
this cache exists.

## Considered Options

**Cache the file list/table instead.** Rejected: the walk is the cheap half of a
cold scan. Caching it would skip a fast step while requiring the whole staleness
apparatus, and a bad staleness verdict there means serving files that do not
exist.

**Store the finished `BigramFilter` columns and mmap them back.** Rejected: a
column is a bit per file address by *file index*, and the file order is
`(parent_dir_index, filename)`, so inserting one file shifts every index after
it. A stored column block is only valid for a byte-identical file set, which is
a snapshot in the wrong sense — worthless the moment anything changes. Rows are
keyed by file identity instead, so a hit and a miss can be mixed within one
Index.

**Per-Root LMDB environment.** Rejected in favour of one shared environment with
a `root_prefix || kind || payload` key: a Root is then deleted with a single
`delete_range`, at the cost of one process-wide (and cross-process) write lock.

## Consequences

- A hit must produce a `BigramFilter` bit-identical to a full build, including
  the stride-2 skip index. Both paths therefore run the same `compress` +
  `set_skip_index` code; a test asserts equality.
- Rows are larger than the compressed columns they replace: two key lists per
  file, measured at 592 bytes per file on disk against 299 bytes per file in
  memory. Disk, not memory, absorbs this; a 137k-file tree is about 82 MB.
- Writing rows costs the scan that produces them about 16% — the price of the
  cache, paid once, against the ~72% of a later scan that it removes.
- Non-indexable files get rows too — empty key list, verdict only. Without them a
  hit still pays the serial sniff pass, which measured 3.16 s of a 15.14 s cold
  scan. It also means `--no-content-indexing` stays fast, which it currently is
  not (see `.scratch/snapshot/issue-serial-sniff.md`).
- The binary verdict has to travel in the row. Skipping the content read on a
  hit would otherwise lose it, and grep would start treating images and
  binaries as text.
- `mtime` must be validated at nanosecond precision. The walker already has
  nanoseconds and `walk/zlob.rs` truncates them to seconds for `FileItem`;
  the truncation is right for `FileItem` (an FFI type) and wrong as a cache key.
- Writes happen only during a scan, in batches. The watcher never writes: it
  already carries the staleness check, so an unwritten change costs a miss, not
  a wrong answer.
- Any cache failure — corrupt env, spec mismatch, write contention, budget
  exceeded — degrades to "no cache". It must never fail a scan or a request.
