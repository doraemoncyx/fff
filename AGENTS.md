# To Clankers

This repository contains **FFF.nvim (Fast File Finder)**, a high-performance file picker for Neovim inspired by blink.cmp's fuzzy matching technology. It's NOT a completion plugin, but rather a standalone file finder with advanced fuzzy search and frecency scoring. The project aims to be the drop-in replacement for telescope, fzf-lua, snacks.picker and similar plugins, focusing on speed, accuracy search and usability features.

## Development Commands

`tools/check.ps1` is the one entry point. It exports the pinned zig that `zlob`
needs (the zig on PATH is often too old), so cargo is never invoked by hand.

Its default mode is scoped, on purpose:

- `tools/check.ps1` (or `-Mode fast`) - clippy, comment rules, and the tests of
  the crates you changed. This is the loop: run it after every edit.
- `tools/check.ps1 -Mode fast -Filter <test name>` - a single test. Use this
  while iterating on one function; it finishes in well under a second.
- `tools/check.ps1 -Mode lint` / `-Mode fmt` / `-Mode comments`
- `tools/check.ps1 -Mode fix` - apply `cargo fmt`

`-Mode all` runs the whole suite and **refuses unless `FFF_FULL_CHECK=1` is
set**. That is deliberate: the full suite links 46 integration binaries, which
is minutes of work to re-prove changes a single crate cannot reach. CI sets the
variable; an implementation agent should not. The release gate is the only other
place it belongs.

Do not run `cargo test --workspace` directly for the same reason.

### Building

- `make build` - build everything
- `build.py` - fetches the pinned zig and libclang, then builds `fff-mcp`

### Testing

Rust: inline unit tests per module plus integration targets under
`crates/*/tests/`. Both walker backends must compile and pass; `zlob` is the CI
feature, `ripgrep` is the default.

- Create e2e local test file for Neovim: Load any Lua test file with `nvim -l <test_file>`

Two `watch::rescan_tests` tests fail on Windows with `PermissionDenied` (they
need a real POSIX rename); they pass on CI.

### Code Quality

- `tools/check.ps1 -Mode lint` - clippy plus the comment rules below
- `tools/check.ps1 -Mode comments` - the mechanical half of the style guide

Comment length and "no doc comments on private items" are checked by
`tools/check-comments.ps1` against `tools/comment-baseline.txt`. Fixing a
violation is always welcome; rerun that script with `-WriteBaseline` afterwards
to shrink the ratchet. Everything else in the style guide is a judgement call
for the reviewer.

When doing code make sure to REDUCE SIZE OF COMMENTS. This is very important. Every comment should be concise 1-2 liner maximum 4 lines if describes really extensive and unnatural concept.

### Important coding rules

- Do not add doc comments to the private structs and functions.
- Do not make public structs if something can be private


## Style guide

- NO MODULES COMMENTS
- NO TOP FILE COMMENTS
- NO COMMENT LONGER THAN 2 LINES UNLESS ASKED EXPLICITLY
- UTILITY FUNCTIONS GO INTO THE END OF FILE

## Architecture

Everything that is performance critical happens in rust world, everything that is neovim specific happens in the lua code.

There are 3 main components:

- Rust binary with the global file picker state containing index of all files
- Background thread with the file system watcher that updates the index in real time
- Lua UI layer that renders the picker, handles user input, and calls the rust functions via FFI

There are 2 databases:

- Frecency database (LMDB) that tracks file access patterns for scoring
- Query history database used to track the user's previous search queries

### Key Files

Lua side:

- `lua/fff.lua` - Entry point, delegates to main.lua
- `lua/fff/main.lua` - Public API (find_files, search, change_directory)
- `lua/fff/core.lua` - Initialization, autocmds, global state management
- `lua/fff/picker_ui.lua` - UI rendering, layout calculation, keymaps
- `lua/fff/file_picker/preview.lua` - File preview with syntax highlighting
- `lua/fff/file_picker/image.lua` - Image preview (snacks.nvim integration)
- `lua/fff/conf.lua` - Default config
- `lua/fff/rust/init.lua` - Loads compiled Rust shared library

Rust side is a workspace under `crates/`. The engine is `fff-core`
(package name `fff-search`):

- `crates/fff-core/src/file_picker.rs` - Core FilePicker struct, indexing, background watcher
- `crates/fff-core/src/scan.rs` - Scan pipeline: walk, commit, post-scan index
- `crates/fff-core/src/types.rs` - `FileItem`, `DirItem`, flags, content cache budget
- `crates/fff-core/src/simd_path.rs` - Chunked path arena (relative, `/`-canonical)
- `crates/fff-core/src/index/bigram_filter.rs` - Content index build and query
- `crates/fff-core/src/index/bigram_row.rs` - Persisted per-file bigram row codec
- `crates/fff-core/src/dbs/` - LMDB stores: frecency, query history, snapshot
- `crates/fff-core/src/walk/` - Two backends: `zlob.rs`, `ripgrep.rs`
- `crates/fff-core/src/score.rs` - Fuzzy match scoring with frizbee integration
- `crates/fff-core/src/git.rs` - Git status caching and repository detection
- `crates/fff-nvim/src/lib.rs` - FFI bindings and process-wide statics (FILE_PICKER, FRECENCY)
- `crates/fff-mcp-http/` - The multi-root HTTP service; see its README and `GLOSSARY.md`

### Scoring Algorithm

Located at `crates/fff-core/src/score.rs`.

### Persisted state

Vocabulary for the on-disk content-index cache is in `GLOSSARY.md`; the decision
and its rejected alternatives are in `docs/adr/0001-per-file-bigram-snapshot.md`.
`crates/fff-mcp-http/README.md` documents the flags and the `/stats` section.

### Build System

- `Cargo.toml` - workspace root; the engine crate is `fff-search`
- `rust-toolchain.toml` - Specifies Rust nightly toolchain with required components
- `Cross.toml` - Cross-compilation settings using Zig for Linux targets
- **CI/CD Workflows**:
  - `.github/workflows/rust.yml` - fmt, clippy, comment rules, tests
  - `.github/workflows/performance*.yml` - Criterion benchmark comparison per PR
- **Cross-compilation Support**: Uses `cross` tool with Zig backend for efficient cross-compilation

## Development Notes

### Working with Rust Code

- Prefer struct methods over functions
- If there is more than 2 impls in the file - create new file
- Smaller concise comments over giant comment blocks
- Do not add doc comments to the private functions/structs
- Be very careful around locking and better double check with the human if something is going to require potentially long lock on a mutex/rwlock

### Working with lua code

- Document the types of public functions in every module
- Use `vim.validate()` for validating user inputs in public functions
- Try to reuse as much of existing functions as possible
- When working on new features for the UI **IT IS EXTREMELY IMPORTANT** to keep the core functionality of navigating between files, selecting, and seeing the preview working as is. NEVER break anything from the core UI functionality, only add new features on top of the current UI.
- When making a large chunk of code make lua test that opens neovim at `~/dev/lightsource` and opens the picker to test the ui functionality across the actual code.
- When adding a new highlights or any new shortcuts and configurable UI options add them to the neovim config. AND IMPORTANT: update the README.md with the new configuration options.

### UI rendering

When working on the UI changeds IT IS EXTREMELY important for you to test it for both prompt_position="bottom" and prompt_position="top" as the rendering logic is different for both of them in both rust and lua world. When the prompt is positioed in the bottom everything should work the same way as the top but would be reversed in order. (though navigation is same for both)

## Top level API that can not introduce breaking changes under any circumstance

Top level rust, lua, C, and bun APIs can not be changed under any circumstance
