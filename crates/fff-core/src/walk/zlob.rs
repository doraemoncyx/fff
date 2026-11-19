use crate::file_picker::FilePickerIgnoreOptions;
use crate::ignore::IGNORED_DIRS;
use crate::types::FileItem;
use crate::walk::{WalkIgnoreRules, WalkLimits, WalkOutput, mtime_ns};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;
use zlob::walk::{WalkBuilder, WalkFlags, WalkMetadata, WalkState};

const PROGRESS_STEP: usize = 13;

#[tracing::instrument(skip_all, name = "zlob walker", level = "info")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_collect_files(
    base_path: &Path,
    is_git_repo: bool,
    follow_symlinks: bool,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
    ignore_options: &FilePickerIgnoreOptions,
    scope: Option<&Path>,
    limits: WalkLimits<'_>,
) -> crate::Result<WalkOutput> {
    let use_non_git_defaults = !is_git_repo && !ignore_options.disable_git;
    let mut flags = WalkFlags::GITIGNORE;
    if ignore_options.disable_git {
        flags |= WalkFlags::SKIP_GITIGNORE | WalkFlags::NO_GIT_EXCLUDE | WalkFlags::NO_GIT_GLOBAL;
    }
    if ignore_options.disable_ignore_files {
        flags |= WalkFlags::SKIP_DOT_IGNORE;
    }
    if use_non_git_defaults {
        flags |= WalkFlags::SKIP_HIDDEN;
    }
    if follow_symlinks {
        flags |= WalkFlags::FOLLOW_SYMLINKS;
    }

    let mut builder = WalkBuilder::new(base_path)
        .map_err(|e| crate::Error::WalkFailed(format!("WalkBuilder::new: {e:?}")))?;
    builder
        .options(flags)
        .threads(threads)
        // Bulk-fetch the only metadata FileItem needs; zlob never stats more.
        .metadata(WalkMetadata::SIZE | WalkMetadata::MTIME);

    let extra_ignore = &ignore_options.extra_ignore;
    let defaults = if use_non_git_defaults {
        IGNORED_DIRS
    } else {
        &[]
    };
    if !defaults.is_empty() || !extra_ignore.is_empty() {
        let rules: Vec<_> = defaults
            .iter()
            .copied()
            .chain(extra_ignore.iter().map(String::as_str))
            .collect();
        builder
            .extra_ignore(&rules)
            .map_err(|e| crate::Error::WalkFailed(format!("invalid extra ignore rules: {e:?}")))?;
    }

    // Single lock for both collections: every entry is either a file or a
    // dir, so this keeps one mutex acquisition per entry.
    let collected = Mutex::new((Vec::new(), Vec::new()));
    // Set when the visitor returns Quit instead of exhausting the tree.
    let stopped = AtomicBool::new(false);

    // `synced_files_count` is throttled, so the limit is judged on the
    // collected length instead. Pushing past the limit ends the walk.
    let push_pair = |item: FileItem, rel_str: String, modified_ns: u64| -> WalkState {
        let mut guard = collected.lock();
        guard.0.push((item, rel_str, modified_ns));
        let n = guard.0.len();
        drop(guard);

        if n % PROGRESS_STEP == 0 {
            synced_files_count.store(n, Ordering::Relaxed);
        }
        if limits.exceeded(n) {
            stopped.store(true, Ordering::Relaxed);
            WalkState::Quit
        } else {
            WalkState::Continue
        }
    };

    let outcome = match builder.run(|entry| {
        // Cancellation is checked per entry so a picker shutdown does not
        // have to wait for the tree.
        if limits.cancelled() {
            stopped.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        if ignore_options.disable_git && entry.path().file_name().is_some_and(|name| name == ".git")
        {
            return WalkState::SkipDir;
        }
        if let Some(scope) = scope {
            let path = entry.path();
            if scope.starts_with(path) {
                return WalkState::Continue;
            }
            if !path.starts_with(scope) {
                return WalkState::SkipDir;
            }
        }

        // Symlinks are indexed unconditionally, classified by target type:
        // symlink→file becomes a searchable file, symlink→dir becomes a dir
        // marker (filtered from the file table in walk_filesystem).
        if entry.is_symlink() {
            let rel_str = entry.relative_path_lossy().into_owned();
            // is_symlink() is lstat-like; stat the target for type + size.
            let Ok(target) = std::fs::metadata(entry.path()) else {
                return WalkState::Continue; // dangling link
            };

            if target.is_dir() {
                // Dir marker: whole relative path is the dir portion.
                let mut rel_dir = rel_str.clone();
                rel_dir.push('/');
                let offset = rel_dir.len() as u16;
                let item = FileItem::new_raw(offset, 0, 0, None, false);
                item.set_symlink_dir(true);
                return push_pair(item, rel_dir, 0);
            } else {
                let time = target.modified().ok();
                let modified = time
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                let basename_offset = entry.basename_offset_in_relative_lossy();
                let basename = &rel_str[basename_offset..];
                let (is_binary, _) = crate::binary_verdict::initial_verdict(entry.path());
                let mut item = FileItem::new_raw(
                    basename_offset as u16,
                    target.len(),
                    modified,
                    None,
                    is_binary,
                );
                item.mark_definition_language(basename);
                item.set_symlink(true);
                return push_pair(item, rel_str, mtime_ns(time));
            }
        }

        // Followed links keep the Symlink kind and lstat metadata, so stat
        // the target to index the ones resolving to files (#912).
        let target = if entry.is_file() {
            None
        } else if follow_symlinks && entry.is_symlink() {
            match std::fs::metadata(entry.path()) {
                Ok(meta) if meta.is_file() => Some(meta),
                _ => return WalkState::Continue,
            }
        } else {
            // unlike ripgrep walker zlob doesnt show .git files
            if entry.is_dir() {
                let relative_path = entry.relative_path_lossy();
                if !relative_path.is_empty() {
                    let mut relative_path = relative_path.into_owned();
                    relative_path.push('/');
                    collected.lock().1.push(relative_path);
                }
            }

            return WalkState::Continue;
        };

        // Lossy pair: the offset must index the decoded string, not the raw
        // bytes, or it lands inside a U+FFFD on invalid-UTF-8 names (#799).
        let basename_offset = entry.basename_offset_in_relative_lossy();
        // zlob emits '/'-separated relative paths, which is fff's canonical
        // internal form on every platform — store them verbatim.
        let relative_path = entry.relative_path_lossy().into_owned();

        // `basename()` returns `&str` for files only.
        let basename = match target {
            None => entry.basename().unwrap_or(""),
            Some(_) => &relative_path[basename_offset..],
        };
        let is_binary = crate::binary_verdict::initial_verdict(entry.path()).0;

        let (size, modified, modified_ns) = match target {
            Some(meta) => {
                let time = meta.modified().ok();
                (
                    meta.len(),
                    time.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs()),
                    mtime_ns(time),
                )
            }
            // zlob reports mtime in ns since the Unix epoch; FileItem wants secs.
            None => (
                entry.size().unwrap_or(0),
                entry
                    .modified_ns()
                    .map(|ns| (ns / 1_000_000_000).max(0) as u64)
                    .unwrap_or(0),
                entry.modified_ns().map_or(0, |ns| ns.max(0) as u64),
            ),
        };

        let mut item = FileItem::new_raw(basename_offset as u16, size, modified, None, is_binary);
        item.mark_definition_language(basename);

        push_pair(item, relative_path, modified_ns)
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            // Preserve whatever we collected before the failure so the caller
            // can still surface a partial index instead of nothing.
            tracing::error!(?e, "zlob walk failed");
            return Err(crate::Error::WalkFailed(format!("{e:?}")));
        }
    };

    let (pairs, dirs) = collected.into_inner();
    // Always report the exact final total regardless of the last step.
    synced_files_count.store(pairs.len(), Ordering::Relaxed);

    // Retain the ignore rules only when the walk actually gathered some
    // (git roots with .gitignore/.ignore). Otherwise callers fall back.
    let ignore_rules = outcome
        .rules()
        .is_some()
        .then(|| WalkIgnoreRules { inner: outcome });

    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules,
        truncated: stopped.load(Ordering::Relaxed),
    })
}
