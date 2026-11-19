//! Filesystem traversal backend. Selects one implementation at compile time:
//! - `zlob`: zlob's native parallel walker (requires the Zig toolchain).
//! - `ripgrep`: the `ignore` crate (ripgrep's walker), used by default.
//!
//! Both expose [`walk_collect_files`] with identical semantics so the rest of
//! the crate stays backend-agnostic.

use crate::types::FileItem;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

#[cfg(feature = "zlob")]
mod zlob;
#[cfg(feature = "zlob")]
pub(crate) use zlob::walk_collect_files;

#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
mod ripgrep;
#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
pub(crate) use ripgrep::walk_collect_files;

pub(crate) struct WalkOutput {
    // File item, relative path, and the file's modification time in
    // nanoseconds since the Unix epoch (0 when it could not be read).
    pub(crate) pairs: Vec<(FileItem, String, u64)>,
    // Every non-ignored directory the walk visited, relative, ending with /
    pub(crate) dirs: Vec<String>,
    pub(crate) ignore_rules: Option<WalkIgnoreRules>,
    // The walk stopped before exhausting the tree (limit hit or cancelled).
    pub(crate) truncated: bool,
}

// Walks that no signal can stop: the ones without a picker behind them.
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

// How a walk may end early: a file limit and the picker's cancel flag.
// `limit == 0` walks the whole tree, which is the pre-guardrail behaviour.
#[derive(Clone, Copy)]
pub(crate) struct WalkLimits<'a> {
    limit: usize,
    cancel: &'a AtomicBool,
}

impl<'a> WalkLimits<'a> {
    pub(crate) fn new(limit: usize, cancel: &'a AtomicBool) -> Self {
        Self { limit, cancel }
    }

    pub(crate) fn unlimited() -> WalkLimits<'static> {
        WalkLimits {
            limit: 0,
            cancel: &NEVER_CANCELLED,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    // One file past the limit proves the tree is bigger than the limit, so
    // hitting the limit exactly never reports a truncated walk.
    fn exceeded(&self, collected: usize) -> bool {
        self.limit != 0 && collected > self.limit
    }
}

pub(crate) struct WalkIgnoreRules {
    #[cfg(feature = "zlob")]
    inner: ::zlob::walk::WalkerOutcomeRules,
    #[cfg(not(feature = "zlob"))]
    _never: std::convert::Infallible,
}

// SAFETY: the underlying storage is immutable, heap-owned, and thread-safe to
// read from concurrently (mirrors zlob's `IgnoreRules: Send + Sync`).
unsafe impl Send for WalkIgnoreRules {}
unsafe impl Sync for WalkIgnoreRules {}

impl std::fmt::Debug for WalkIgnoreRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalkIgnoreRules")
    }
}

// In ripgrep builds `WalkIgnoreRules` is never constructed (the `_never`
// field is uninhabited), so its methods are legitimately dead there.
#[cfg_attr(not(feature = "zlob"), allow(dead_code))]
impl WalkIgnoreRules {
    /// Returns `true` if the provided path is ignored by the collected rule set
    ///
    /// `relative_path` has to be relative to the walker's provided base path
    pub(crate) fn is_ignored(&self, relative_path: &Path) -> bool {
        #[cfg(feature = "zlob")]
        {
            self.inner
                .rules()
                .is_some_and(|rules| rules.is_ignored(relative_path))
        }
        #[cfg(not(feature = "zlob"))]
        {
            let _ = relative_path;
            match self._never {}
        }
    }

    // The old `is_ignored_untrusted` variant was folded away when zlob's
    // ignore matcher moved to full ancestor enumeration — trailing-slash
    // sniffing on the input is now sufficient for external queries.
}

// Modification time in nanoseconds since the Unix epoch. An unreadable or
// pre-epoch timestamp collapses to 0: the walk must never abort over it.
pub(crate) fn mtime_ns(time: Option<SystemTime>) -> u64 {
    time.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod symlink_tests;

#[cfg(test)]
mod tests {
    use super::{WalkLimits, WalkOutput, mtime_ns, walk_collect_files};
    use crate::file_picker::FilePickerIgnoreOptions;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // Backend-agnostic parity check: both the zlob and ripgrep walkers must
    // respect .gitignore, skip hidden files in a git repo, and surface the
    // expected file set with a correct synced count.
    #[test]
    fn collects_files_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("Cargo.toml"), "x").unwrap();
        fs::write(root.join("debug.log"), "").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("target/out.bin"), "bin").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_all(root, true, false, 1, &counter, &Default::default(), None).unwrap();

        let mut names: Vec<String> = out.pairs.into_iter().map(|(_, rel, _)| rel).collect();
        names.sort();

        assert!(names.contains(&"Cargo.toml".to_string()));
        assert!(names.iter().any(|n| n.ends_with("main.rs")));
        // target/ and *.log are gitignored; .git/ is skipped.
        assert!(!names.iter().any(|n| n.contains("target")));
        assert!(!names.iter().any(|n| n.ends_with(".log")));
        assert!(!names.iter().any(|n| n.contains(".git/")));
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
    }

    // Non-git roots prune known non-code directories (node_modules).
    #[test]
    fn prunes_non_code_dirs_for_non_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/lib.js"), "x").unwrap();
        fs::write(root.join("index.js"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_all(root, false, false, 1, &counter, &Default::default(), None).unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel, _)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("index.js")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }

    // Stow-style file symlinks pointing outside the root must be indexed with
    // follow_symlinks on, with the target's metadata (#912).
    #[cfg(unix)]
    #[test]
    fn follows_file_symlinks_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("dotfiles");
        let root = dir.path().join("config");
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(outside.join("sesh.toml"), "0123456789").unwrap();
        fs::write(outside.join("nested/inner.toml"), "x").unwrap();
        fs::write(root.join("projects.toml"), "x").unwrap();
        std::os::unix::fs::symlink("../dotfiles/sesh.toml", root.join("sesh.toml")).unwrap();
        std::os::unix::fs::symlink("../dotfiles/nested", root.join("linked")).unwrap();
        std::os::unix::fs::symlink("../dotfiles/missing", root.join("broken")).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_all(&root, false, true, 1, &counter, &Default::default(), None).unwrap();
        let mut names: Vec<String> = out.pairs.iter().map(|(_, rel, _)| rel.clone()).collect();
        names.sort();

        assert_eq!(names, ["linked/inner.toml", "projects.toml", "sesh.toml"]);
        assert_eq!(counter.load(Ordering::Relaxed), names.len());

        let (item, rel, _) = out.pairs.iter().find(|(_, r, _)| r == "sesh.toml").unwrap();
        assert_eq!(item.size, 10);
        assert_eq!(&rel[item.filename_offset_in_relative_path()..], "sesh.toml");

        let out = walk_all(&root, false, false, 1, &counter, &Default::default(), None).unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel, _)| rel).collect();
        assert_eq!(names, ["projects.toml"]);
    }

    // Only the zlob backend surfaces reusable ignore rules; they must match
    // the same tree the walk respected.
    #[cfg(feature = "zlob")]
    #[test]
    fn surfaces_reusable_ignore_rules() {
        use std::path::Path;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("Cargo.toml"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_all(root, true, false, 1, &counter, &Default::default(), None).unwrap();

        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(Path::new("target/")));
        assert!(rules.is_ignored(Path::new("debug.log")));
        assert!(!rules.is_ignored(Path::new("Cargo.toml")));
    }

    // Both backends must honor `info/exclude`, which linked worktrees read from
    // the common git dir (#982).
    #[test]
    fn honors_git_info_exclude() {
        use std::path::Path;
        use std::process::Command;

        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("excluded")).unwrap();
        fs::write(root.join("src/a.txt"), "a").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        fs::write(root.join("excluded/b.txt"), "b").unwrap();
        fs::write(root.join(".git/info/exclude"), "# comment\nexcluded/\n").unwrap();

        let worktree = dir.path().join("wt");
        git(
            &root,
            &["worktree", "add", "-q", worktree.to_str().unwrap()],
        );
        fs::create_dir_all(worktree.join("excluded")).unwrap();
        fs::write(worktree.join("excluded/c.txt"), "c").unwrap();

        for base in [&root, &worktree] {
            let counter = Arc::new(AtomicUsize::new(0));
            let out = walk_all(base, true, false, 1, &counter, &Default::default(), None).unwrap();
            let names: Vec<String> = out.pairs.iter().map(|(_, rel, _)| rel.clone()).collect();

            assert!(names.iter().any(|n| n == "src/a.txt"), "{names:?}");
            assert!(
                !names.iter().any(|n| n.starts_with("excluded")),
                "{names:?}"
            );

            // The watcher filters incremental updates with these rules.
            #[cfg(feature = "zlob")]
            {
                let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
                assert!(rules.is_ignored(Path::new("excluded/later/new.txt")));
                assert!(rules.is_ignored(Path::new("debug.log")));
                assert!(!rules.is_ignored(Path::new("src/a.txt")));
            }
        }
    }

    // zlob 1.8 reads core.excludesFile below the root .gitignore, as git does (#874).
    #[cfg(feature = "zlob")]
    #[test]
    fn honors_git_excludes_file_with_git_precedence() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".worktrees/wt1")).unwrap();
        fs::write(root.join("src/hit.ts"), "x").unwrap();
        fs::write(root.join(".worktrees/wt1/hit.ts"), "x").unwrap();
        fs::write(root.join("drop.tmp"), "x").unwrap();
        fs::write(root.join("keep.tmp"), "x").unwrap();
        fs::write(root.join(".gitignore"), "!keep.tmp\n").unwrap();

        let excludes = dir.path().join("excludes");
        fs::write(&excludes, "# comment\n.worktrees/\n*.tmp\nexcludes\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "core.excludesFile", excludes.to_str().unwrap()],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_all(root, true, false, 1, &counter, &Default::default(), None).unwrap();
        let names: Vec<String> = out.pairs.iter().map(|(_, rel, _)| rel.clone()).collect();

        assert!(names.iter().any(|n| n == "src/hit.ts"), "{names:?}");
        assert!(names.iter().any(|n| n == "keep.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n == "drop.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".worktrees")), "{names:?}");

        // The watcher filters incremental updates with these rules.
        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(std::path::Path::new(".worktrees/wt1/new.ts")));
        assert!(!rules.is_ignored(std::path::Path::new("src/hit.ts")));
    }

    // disable_git must also turn off the git exclude files, not only .gitignore.
    #[cfg(feature = "zlob")]
    #[test]
    fn disable_git_skips_every_git_ignore_source() {
        use crate::file_picker::FilePickerIgnoreOptions;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        for name in ["from-gitignore", "from-exclude", "from-global", "from-dot"] {
            fs::write(root.join(name), "x").unwrap();
        }
        fs::write(root.join(".gitignore"), "from-gitignore\n").unwrap();
        fs::write(root.join(".ignore"), "from-dot\n").unwrap();
        let excludes = dir.path().join("excludes");
        fs::write(&excludes, "from-global\n").unwrap();

        for args in [
            vec!["init", "-q"],
            vec!["config", "core.excludesFile", excludes.to_str().unwrap()],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(status.success());
        }
        fs::write(root.join(".git/info/exclude"), "from-exclude\n").unwrap();

        let walk = |disable_git| -> Vec<String> {
            let options = FilePickerIgnoreOptions {
                disable_git,
                ..Default::default()
            };
            let counter = Arc::new(AtomicUsize::new(0));
            walk_all(&root, true, false, 1, &counter, &options, None)
                .unwrap()
                .pairs
                .into_iter()
                .map(|(_, rel, _)| rel)
                .collect()
        };

        let enabled = walk(false);
        for name in ["from-gitignore", "from-exclude", "from-global", "from-dot"] {
            assert!(!enabled.iter().any(|n| n == name), "{name}: {enabled:?}");
        }

        let disabled = walk(true);
        for name in ["from-gitignore", "from-exclude", "from-global"] {
            assert!(disabled.iter().any(|n| n == name), "{name}: {disabled:?}");
        }
        assert!(!disabled.iter().any(|n| n == "from-dot"), "{disabled:?}");
        assert!(
            !disabled.iter().any(|n| n.starts_with(".git/")),
            "{disabled:?}"
        );
    }

    // Two writes inside one second must stay distinguishable, so the identity
    // keeps the full precision the file table drops. Windows rounds
    // `SystemTime` to 100 ns, so the samples are 100 ns aligned.
    #[test]
    fn mtime_ns_keeps_sub_second_precision() {
        use std::time::{Duration, UNIX_EPOCH};

        let first = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_100_000_100);
        let second = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_900_000_300);

        assert_eq!(mtime_ns(Some(first)), 1_700_000_000_100_000_100);
        assert_eq!(mtime_ns(Some(second)), 1_700_000_000_900_000_300);
        assert_ne!(mtime_ns(Some(first)), mtime_ns(Some(second)));
        assert_eq!(mtime_ns(Some(first)) / 1_000_000_000, 1_700_000_000);
    }

    // An unreadable or pre-epoch mtime collapses to 0 instead of aborting.
    #[test]
    fn unreadable_mtime_is_zero() {
        use std::time::{Duration, UNIX_EPOCH};

        assert_eq!(mtime_ns(None), 0);
        assert_eq!(mtime_ns(Some(UNIX_EPOCH - Duration::from_secs(1))), 0);
    }

    // Both backends must report the untruncated mtime, aligned with the pair.
    #[test]
    fn walker_reports_untruncated_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_all(root, false, false, 1, &counter, &Default::default(), None).unwrap();

        assert_eq!(out.pairs.len(), 2);
        for (_, rel, reported) in &out.pairs {
            let on_disk = fs::metadata(root.join(rel)).unwrap().modified().ok();
            assert_eq!(*reported, mtime_ns(on_disk), "{rel}");
        }
    }

    // Past the limit the walk stops and says so; in-flight threads may
    // overshoot by one entry each.
    #[test]
    fn limit_below_file_count_is_truncated() {
        const TOTAL: usize = 64;
        const LIMIT: usize = 8;
        const THREADS: usize = 4;

        let dir = tempfile::tempdir().unwrap();
        tree_with_files(dir.path(), TOTAL);
        let cancel = AtomicBool::new(false);

        let out = walk_capped(dir.path(), LIMIT, THREADS, &cancel);

        assert!(out.truncated, "stopping at the limit truncates the walk");
        assert!(
            out.pairs.len() > LIMIT,
            "collected {} files, never crossed the limit {LIMIT}",
            out.pairs.len()
        );
        assert!(
            out.pairs.len() <= LIMIT + THREADS,
            "collected {} files, past limit + threads {THREADS}",
            out.pairs.len()
        );
    }

    // A limit is a cap, not a truncation trigger: a tree that fits exactly is
    // walked in full.
    #[test]
    fn limit_equal_to_file_count_is_complete() {
        const TOTAL: usize = 24;

        let dir = tempfile::tempdir().unwrap();
        tree_with_files(dir.path(), TOTAL);
        let cancel = AtomicBool::new(false);

        let out = walk_capped(dir.path(), TOTAL, 4, &cancel);

        assert_eq!(out.pairs.len(), TOTAL);
        assert!(!out.truncated);
    }

    #[test]
    fn limit_above_file_count_is_complete() {
        const TOTAL: usize = 24;

        let dir = tempfile::tempdir().unwrap();
        tree_with_files(dir.path(), TOTAL);
        let cancel = AtomicBool::new(false);

        let out = walk_capped(dir.path(), TOTAL * 4, 4, &cancel);

        assert_eq!(out.pairs.len(), TOTAL);
        assert!(!out.truncated);
    }

    // A cancel that lands mid-walk ends it within a few files and marks the
    // result truncated instead of walking the rest of the tree.
    #[test]
    fn cancel_mid_scan_is_truncated() {
        const TOTAL: usize = 1000;
        const CANCEL_AFTER: usize = 32;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        tree_with_files(&root, TOTAL);

        // The walk runs on a worker and the test thread polls its progress, so
        // the cancel normally lands early. A loaded machine can still finish
        // the tree between two polls, so an attempt that saw no cancel retries.
        for _ in 0..5 {
            let counter = Arc::new(AtomicUsize::new(0));
            let cancel = Arc::new(AtomicBool::new(false));
            let walk = {
                let root = root.clone();
                let counter = Arc::clone(&counter);
                let cancel = Arc::clone(&cancel);
                std::thread::spawn(move || {
                    walk_collect_files(
                        &root,
                        false,
                        false,
                        4,
                        &counter,
                        &Default::default(),
                        None,
                        WalkLimits::new(0, &cancel),
                    )
                })
            };

            while counter.load(Ordering::Relaxed) < CANCEL_AFTER {
                std::hint::spin_loop();
            }
            cancel.store(true, Ordering::Release);

            let out = walk.join().unwrap().unwrap();
            if !out.truncated {
                continue;
            }
            assert!(
                out.pairs.len() < TOTAL,
                "cancel cut nothing short, collected all {TOTAL} files"
            );
            return;
        }
        panic!("no cancel landed mid-walk in 5 attempts");
    }

    // The call shape every test predating the limit uses: no cap, no cancel.
    fn walk_all(
        base_path: &Path,
        is_git_repo: bool,
        follow_symlinks: bool,
        threads: usize,
        counter: &Arc<AtomicUsize>,
        options: &FilePickerIgnoreOptions,
        scope: Option<&Path>,
    ) -> crate::Result<WalkOutput> {
        walk_collect_files(
            base_path,
            is_git_repo,
            follow_symlinks,
            threads,
            counter,
            options,
            scope,
            WalkLimits::unlimited(),
        )
    }

    fn walk_capped(root: &Path, limit: usize, threads: usize, cancel: &AtomicBool) -> WalkOutput {
        walk_collect_files(
            root,
            false,
            false,
            threads,
            &Arc::new(AtomicUsize::new(0)),
            &Default::default(),
            None,
            WalkLimits::new(limit, cancel),
        )
        .unwrap()
    }

    // Spreads `count` files over a few subdirectories so the parallel walk has
    // more than one directory to work on.
    fn tree_with_files(root: &Path, count: usize) {
        let dirs = count.clamp(1, 4);
        for d in 0..dirs {
            fs::create_dir_all(root.join(format!("d{d}"))).unwrap();
        }
        for i in 0..count {
            fs::write(root.join(format!("d{}/f{i}.txt", i % dirs)), "x").unwrap();
        }
    }
}
