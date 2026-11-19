use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use crate::file_picker::{FilePickerIgnoreOptions, FileSync};
use crate::parallelism::BACKGROUND_THREAD_POOL;
use crate::walk::{WalkLimits, walk_collect_files};

// A refusal names this many subdirectories before the list is cut short.
const MAX_NAMED_SUBDIRS: usize = 12;

// Nothing but the cap can end the gate's walk: no picker owns it.
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// A tardir that crossed the Root cap, and so never becomes a Root.
#[derive(Debug)]
pub struct RootCapRefusal {
    /// The cap that stopped the walk.
    pub cap: usize,
    /// Files counted when the walk stopped: past `cap`, never the tree's size.
    pub counted: usize,
    /// The directory's own subdirectories, sorted, cut to a readable list.
    pub subdirs: Vec<String>,
}

/// Walk `root` until it crosses `cap` files. `None` admits the directory and
/// `Some` refuses it, without reading the rest of the tree; `cap == 0` admits
/// it without walking at all.
pub fn refusal(
    root: &Path,
    cap: usize,
    follow_symlinks: bool,
    ignore_options: &FilePickerIgnoreOptions,
) -> Option<RootCapRefusal> {
    if cap == 0 {
        return None;
    }
    let is_git_repo = FileSync::discover_git(root, ignore_options.disable_git).is_some();
    let counted = Arc::new(AtomicUsize::new(0));
    // A directory that cannot be walked at all is the scan's error to report.
    let walked = walk_collect_files(
        root,
        is_git_repo,
        follow_symlinks,
        BACKGROUND_THREAD_POOL.current_num_threads(),
        &counted,
        ignore_options,
        None,
        WalkLimits::new(cap, &NEVER_CANCELLED),
    )
    .ok()?;
    if !walked.truncated {
        return None;
    }
    Some(RootCapRefusal {
        cap,
        counted: walked.pairs.len(),
        subdirs: subdirectories(root),
    })
}

// One level below the Root: what a narrower tardir could name instead.
fn subdirectories(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    if names.len() > MAX_NAMED_SUBDIRS {
        names.truncate(MAX_NAMED_SUBDIRS);
        names.push("…".to_string());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::refusal;
    use crate::file_picker::FilePickerIgnoreOptions;
    use std::fs;
    use std::path::Path;

    // `files` files spread over `dirs`; an empty `dirs` keeps them at the root.
    fn tree(files: usize, dirs: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in dirs {
            fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        for index in 0..files {
            let name = match dirs.len() {
                0 => format!("f{index}.rs"),
                len => format!("{}/f{index}.rs", dirs[index % len]),
            };
            fs::write(dir.path().join(name), "x").unwrap();
        }
        dir
    }

    fn cap_of(root: &Path, cap: usize) -> Option<super::RootCapRefusal> {
        refusal(root, cap, false, &FilePickerIgnoreOptions::default())
    }

    // Zero is the escape hatch: no walk, no cap, whatever the tree holds.
    #[test]
    fn a_cap_of_zero_admits_every_tree() {
        let dir = tree(40, &["alpha", "beta"]);
        assert!(cap_of(dir.path(), 0).is_none());
        assert!(cap_of(dir.path(), 10).is_some(), "the same tree is over 10");
    }

    // A tree of exactly `cap` files is inside the cap, one more is outside.
    #[test]
    fn the_cap_is_exclusive_of_the_tree_size() {
        let dir = tree(12, &["alpha"]);
        assert!(cap_of(dir.path(), 12).is_none());
        let refusal = cap_of(dir.path(), 11).expect("11 files is over a cap of 11");
        assert!(refusal.counted > 11, "{refusal:?}");
    }

    // The count is the walk's own stopping point, not the tree's size: the
    // refusal costs one level of walking, not the whole tree.
    #[test]
    fn a_huge_tree_is_refused_near_the_cap_not_at_its_end() {
        let dir = tree(600, &["alpha", "beta", "gamma"]);
        let refusal = cap_of(dir.path(), 20).expect("600 files is over a cap of 20");
        assert_eq!(refusal.cap, 20);
        assert!(
            refusal.counted > 20 && refusal.counted < 300,
            "counted {} of 600 files",
            refusal.counted
        );
        assert_eq!(
            refusal.subdirs,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }

    // Nothing below the root to name: the message says so instead of guessing.
    #[test]
    fn a_directory_with_no_subdirectories_names_none() {
        let dir = tree(30, &[]);
        let refusal = cap_of(dir.path(), 5).expect("30 flat files is over 5");
        assert!(refusal.subdirs.is_empty(), "{refusal:?}");
    }

    #[test]
    fn only_the_first_level_is_named() {
        let dir = tree(600, &["alpha/beta", "gamma"]);
        let refusal = cap_of(dir.path(), 5).expect("600 files is over 5");
        assert_eq!(
            refusal.subdirs,
            vec!["alpha".to_string(), "gamma".to_string()]
        );
    }
}
