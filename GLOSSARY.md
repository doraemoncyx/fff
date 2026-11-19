# Glossary

Terms for the multi-root HTTP search service (`crates/fff-mcp-http`).

## Root

An absolute directory that has been indexed, canonicalized and used as the cache
key. A Root is the identity of an Index. Two spellings of the same directory
(case, trailing separator, `\\?\` prefix, `..`) are the same Root.

_Not_: base path, workspace, project.

## Index

Everything the service holds for one Root: the file and directory tables produced
by a scan, plus the live watcher that keeps them current. An Index is the unit
of caching, eviction, and staleness.

_Not_: cache entry, snapshot.

## Content index

The part of an Index that answers "which files could contain this text": a
per-file set of bigrams, read from file contents and arranged into columns so a
grep query can discard files without opening them.

_Not_: prefilter (that is its use, not its contents), bigram index (an
implementation), file table.

## Binary verdict

The judgement that a file's contents are not text. It has two sources: the file
name, when the extension sits on the built-in binary or text table, and
otherwise the file's own bytes, read by the content pass. A mixed-use
extension — `.blp` names both a Blueprint template and a texture — always goes
to the bytes. The verdict is part of the Content index, it decides whether grep
may read that file at all, and the name-based side of it is a fast first answer
that a later read may still turn binary.

_Not_: ignored file (a binary is indexed, not skipped). A file extension is one
source of the verdict, not the verdict itself.

## Snapshot

The on-disk copy of one Root's Content index: for each file, its bigrams, its
Binary verdict, and the file identity the verdict is valid for. A Snapshot does
not hold the file or directory tables — those are rebuilt by every scan. It is
not an Index: it has no watcher, it answers no request, and it never covers a
tardir. It carries no authority on its own — it counts only once an Index
adopts a row and confirms that row still matches the file on disk.

_Not_: cached Index, index, cache entry, file table backup.

## Map ceiling

The most the LMDB map of a Snapshot may grow to: the free space of the volume
holding the environment, minus a reserve (a gigabyte, or 5% of the volume if
that is more). It is not the Snapshot budget — that one counts row bytes and
shrinks the environment by deleting Roots; the ceiling is the volume's own
limit and is never exceeded, so a full disk loses writes instead of the file
growing until the disk is full.

_Not_: budget, quota, disk size.

## Stale

A Snapshot is stale with respect to a file when the file on disk no longer
matches what the Snapshot recorded for it. Staleness is decided per file, never
per Root: one stale file does not discard the rest of the Snapshot.

_Not_: outdated, dirty, expired.

## tardir

The path named by a single request: a directory, or one file. A tardir is not
necessarily a Root: it is only a Root when an Index happens to exist for exactly
that directory.

_Not_: target, scope, query dir.

## File target

A tardir that names a file. Its Root is the file's parent directory, which
reuses an existing Index when one covers it; the request is scoped to that one
path, so no second Index is created and `/stats` still reports parent
directories. A file the Index does not hold (ignored, binary, empty, over the
size cap) is still a file target: content search answers "not in the Index"
instead of a bare "no matches".

_Not_: per-file Index, file Root.

## Cover

An Index covers a tardir when its Root is the tardir's directory or an ancestor
of it. A covering Index can answer a request for that tardir because every file
under the tardir's directory is already in it. An Index never covers a tardir
above its own Root.

## Serving set

The set of Indexes consulted for one request: the deepest Index covering the
tardir, plus every Index whose Root lies below the tardir's directory. Indexes
outside both relations are never consulted. A file target has no Root below it,
so its serving set is the covering Index alone.

## Merge

Answering one request from several Indexes in the serving set: results are
deduplicated by absolute path and ordered as one list. Merge combines results,
never index structures: two Indexes are never folded into one.

_Not_: union index, index merge.

## Scan-in-progress

The state of an Index whose initial walk has not finished. A request can be
answered while an Index is scan-in-progress, using whatever the walk has
published so far.
