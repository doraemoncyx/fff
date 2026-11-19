# Let the extension answer the Binary verdict, and read bytes only when it cannot

**Status**: accepted

The name decides first. `crates/fff-core/src/binary_verdict.rs` holds three
built-in sets — `BINARY_EXTENSIONS` (148), `TEXT_EXTENSIONS` (95) and
`MIXED_USE_EXTENSIONS` (`blp`) — and one lookup that returns Binary, Text,
MixedUse or Unknown for a file name. Binary is binary without opening the file;
Text is not binary for the same reason; MixedUse and Unknown go to the bytes,
which for indexable files is the content pass that reads them anyway and for the
rest is the head sniff (`sniff_binary_for_non_indexable`). Nothing but a content
read can turn a text verdict into binary, and nothing can turn binary into text.

Measured on this repository's working tree, 493 files after the ignore rules:
10 binary names, 406 text names, 0 mixed-use, 77 unknown, 4 empty. 85% of the
names answer without a byte, and in `--no-content-indexing` mode — where no
bigram pass reads anything — those 416 names each paid a classification read of
the whole file (up to 10 MB, `detect_binary_per_byte`'s cap) only to confirm that
no NUL byte is in it. ADR 0001 measured that same serial sniff at 3.16 s of a
15.14 s cold scan on a 137k-file tree and 71.5 s when it ran alone. The walk also
reopened files it had already called binary from the name: a `.png` reached the
sniff pass on every scan, and no longer does.

## Considered Options

**Name only the binaries, leave everything else to the bytes.** Rejected: the
read is the cost, and a binary-only list leaves every source, config and log file
reading itself to settle a question its own name answers. It also keeps the sniff
pass reopening files whose name the walk had already settled.

**Keep reading bytes for every file.** Rejected: a 2.36M-file asset tree pays one
open per file for a judgement the name holds, which is the reason this slice
exists at all. The name answer is a binary search over a sorted array, no
allocation, one per file.

**Let the caller supply the table.** Rejected: a caller that passes a short list,
or a typo'd key, silently drops the built-in defence for every extension it
forgets. The configuration is additive on purpose (`add_extra_binary_extensions`),
so a mistake can only widen the binary table — the visible direction (below) —
and can never narrow it.

**Store the source in the Snapshot row.** Rejected: the row has exactly one flag
bit, and its decoder refuses anything else (`flags & !FLAG_BINARY != 0` → no
row), so recording "extension" versus "content" is a schema bump plus a rewrite
of every existing row for a diagnostic. A tally on the walk and the existing
`sniffed` / `content_reads` counters answer the same question for free.

## Consequences

- The two misjudgements do not cost the same. A text file judged binary is
  **silently missed**: grep skips it and no result hints at it. A binary judged
  text is **noisy**: grep reads the blob and returns matching garbage that
  somebody reports. That asymmetry is why `TEXT_EXTENSIONS` is a fixed, reviewed
  list and is not configurable, while the binary side accepts operator additions.
- `.txt`, `.csv` and `.dat` are absent from the text table, so they stay on the
  byte path. They are what people name UTF-16 and other NUL-carrying files, and
  three existing fixtures (`header.txt`, `healed.txt`, `mixed.txt`) are exactly
  that: content must keep winning for those names.
- A content read that happens anyway still overrides a text name — an indexable
  `.rs` full of NULs is judged binary by the bigram pass. The table buys a read,
  not the right to be wrong.
- A row keeps whatever its source concluded. A text-named file whose row says
  binary stays binary on the next scan instead of being re-read to disagree, so
  verdicts from before this ADR and after it coexist in one Snapshot.
- The tables are not persisted either: rows are keyed by `(size, mtime_ns)`, so a
  widened binary table re-decides nothing a row already covers until the file
  changes. Accepted — widening only ever moves verdicts in the noisy direction,
  and the next real change to the file picks it up.
- One classifier serves both walker backends and the symlink expansion
  (`initial_verdict`); the zlob backend's inline copy of the extension check and
  the two string-shaped helpers it used are gone.
- The walk tallies its four answer kinds and logs one `binary verdict triage`
  line per scan, which is where the source split is observable. The content side
  of the split was already counted as `sniffed` and `content_reads`.
- Default table entries are cross-platform by rule. Machine-specific asset
  formats (`.mtl`, `.mha`, `.atlas`) are the operator's business, through the
  additive list, and the defaults themselves are meant to be re-derived from
  measured Snapshot rows rather than guessed
  (see the `Root cap` guardrails: the defaults are meant to be re-derived from
  measured Snapshot rows rather than guessed).
