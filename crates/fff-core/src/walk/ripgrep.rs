use crate::ignore::non_git_repo_overrides;
use crate::types::FileItem;
use crate::walk::WalkOutput;
use crate::watch::is_git_file;
use ignore::WalkBuilder;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tracing::instrument(skip_all, name = "ripgrep walker", level = "info")]
pub(crate) fn walk_collect_files(
    base_path: &Path,
    is_git_repo: bool,
    follow_symlinks: bool,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
) -> crate::Result<WalkOutput> {
    let mut walk_builder = WalkBuilder::new(base_path);
    walk_builder
        // this is a very important guard for the user opening ~/ or other root non-git dir
        .hidden(!is_git_repo)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .follow_links(follow_symlinks)
        .threads(threads);

    if !is_git_repo && let Some(overrides) = non_git_repo_overrides(base_path) {
        walk_builder.overrides(overrides);
    }

    let walker = walk_builder.build_parallel();

    // Single lock for both collections: every entry is either a file or a
    // dir, so this keeps one mutex acquisition per entry.
    let collected =
        parking_lot::Mutex::new((Vec::<(FileItem, String)>::new(), Vec::<String>::new()));
    walker.run(|| {
        let collected = &collected;
        let counter = Arc::clone(synced_files_count);
        let base_path = base_path.to_path_buf();

        Box::new(move |result| {
            let Ok(entry) = result else {
                return ignore::WalkState::Continue;
            };

            let file_type = entry.file_type();
            let path = entry.path();

            // Symlinks are indexed unconditionally, classified by target type.
            // `follow_links(true)` already resolves symlinks to their targets
            // (file → file, dir → descended), so only the unresolved case lands
            // here.
            if file_type.is_some_and(|ft| ft.is_symlink()) {
                if is_git_file(path) {
                    return ignore::WalkState::Continue;
                }

                // file_type() is lstat-like; stat the target for type + size.
                let Ok(target) = std::fs::metadata(path) else {
                    return ignore::WalkState::Continue; // dangling link
                };

                let rel =
                    pathdiff::diff_paths(path, &base_path).unwrap_or_else(|| path.to_path_buf());
                let rel_str =
                    crate::path_utils::to_canonical_slashes(&rel.to_string_lossy()).into_owned();

                if target.is_dir() {
                    // Dir marker: whole relative path is the dir portion.
                    let mut rel_dir = rel_str.clone();
                    rel_dir.push('/');
                    let offset = rel_dir.len() as u16;
                    let item = FileItem::new_raw(offset, 0, 0, None, false);
                    item.set_symlink_dir(true);
                    collected.lock().0.push((item, rel_dir));
                } else {
                    let modified = target
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs());
                    let (file_item, _) = FileItem::new_from_walk_parts(
                        path,
                        &base_path,
                        None,
                        target.len(),
                        modified,
                    );
                    file_item.set_symlink(true);
                    collected.lock().0.push((file_item, rel_str));
                }
                counter.fetch_add(1, Ordering::Relaxed);
                return ignore::WalkState::Continue;
            }

            if file_type.is_some_and(|ft| ft.is_file()) {
                // Ignore walkers sometimes surface files inside `.git/`
                // when the base is itself a git repo — skip them.
                if is_git_file(path) {
                    return ignore::WalkState::Continue;
                }

                let metadata = entry.metadata().ok();
                let (file_item, rel_path) =
                    FileItem::new_from_walk(path, &base_path, None, metadata.as_ref());

                collected.lock().0.push((file_item, rel_path));
                counter.fetch_add(1, Ordering::Relaxed);
            } else if entry.depth() > 0 && entry.file_type().is_some_and(|ft| ft.is_dir()) {
                let path = entry.path();
                if !is_git_file(path)
                    && let Ok(rel) = path.strip_prefix(&base_path)
                {
                    let mut rel = crate::path_utils::to_canonical_slashes(&rel.to_string_lossy())
                        .into_owned();
                    rel.push('/');
                    collected.lock().1.push(rel);
                }
            }
            ignore::WalkState::Continue
        })
    });

    let (pairs, dirs) = collected.into_inner();
    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules: None,
    })
}
