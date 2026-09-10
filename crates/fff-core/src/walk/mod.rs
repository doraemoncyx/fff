//! Filesystem traversal backend. Selects one implementation at compile time:
//! - `zlob`: zlob's native parallel walker (requires the Zig toolchain).
//! - `ripgrep`: the `ignore` crate (ripgrep's walker), used by default.
//!
//! Both expose [`walk_collect_files`] with identical semantics so the rest of
//! the crate stays backend-agnostic.

use crate::types::FileItem;
use std::path::Path;

#[cfg(feature = "zlob")]
mod zlob;
#[cfg(feature = "zlob")]
pub(crate) use zlob::walk_collect_files;

#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
mod ripgrep;
#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
pub(crate) use ripgrep::walk_collect_files;

pub(crate) struct WalkOutput {
    pub(crate) pairs: Vec<(FileItem, String)>,
    /// Every non-ignored directory the walk visited, relative, ending with /
    pub(crate) dirs: Vec<String>,
    pub(crate) ignore_rules: Option<WalkIgnoreRules>,
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

#[cfg(test)]
mod tests {
    use super::walk_collect_files;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let out = walk_collect_files(root, true, false, 1, &counter).unwrap();

        let mut names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
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
        let out = walk_collect_files(root, false, false, 1, &counter).unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("index.js")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
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
        let out = walk_collect_files(root, true, false, 1, &counter).unwrap();

        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(Path::new("target/")));
        assert!(rules.is_ignored(Path::new("debug.log")));
        assert!(!rules.is_ignored(Path::new("Cargo.toml")));
    }

    // Symlinks are indexed unconditionally, classified by their target:
    // symlink→file lands in the file list, symlink→dir becomes a dir marker.
    // Both backends must behave identically.
    #[test]
    fn collects_symlinks_like_regular_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("real_dir")).unwrap();
        fs::write(root.join("real_dir/file.txt"), "x").unwrap();
        fs::write(root.join("real.txt"), "x").unwrap();

        let mut file_link_created = false;
        if symlink(&root.join("real.txt"), &root.join("link.txt")).is_ok() {
            file_link_created = true;
        }

        // Junction creation on Windows works without admin; real symlinks need
        // developer mode. Try junction first, fall back to a real symlink.
        let dir_link_created = symlink_dir(&root.join("real_dir"), &root.join("link_dir")).is_ok();

        if !file_link_created && !dir_link_created {
            eprintln!("skipping symlink test: symlink creation unsupported");
            return;
        }

        let counter = Arc::new(AtomicUsize::new(0));
        // follow_symlinks=false — symlinks must still be visible.
        let out = walk_collect_files(root, false, false, 1, &counter).unwrap();
        let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

        if file_link_created {
            assert!(
                names.iter().any(|n| n == "link.txt"),
                "symlink→file indexed: {names:?}"
            );
            let file_link = out
                .pairs
                .iter()
                .find(|(_, rel)| rel == "link.txt")
                .expect("file link present");
            assert!(file_link.0.is_symlink());
            assert!(!file_link.0.is_symlink_dir());
        }

        if dir_link_created {
            assert!(
                names.iter().any(|n| n == "link_dir/"),
                "symlink→dir marker present: {names:?}"
            );
            // The marker is flagged so walk_filesystem can filter it from files.
            let marker = out
                .pairs
                .iter()
                .find(|(_, rel)| rel == "link_dir/")
                .expect("marker present");
            assert!(marker.0.is_symlink_dir());
        }

        assert!(
            names.iter().any(|n| n == "real.txt"),
            "regular file still indexed: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "real_dir/file.txt"),
            "real file under real dir indexed: {names:?}"
        );
    }

    // Helpers to create symlinks cross-platform. Windows dir junctions work
    // without admin; real symlinks need developer mode.
    #[cfg(unix)]
    fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[cfg(unix)]
    fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        // `symlink_dir` (real symlink) needs developer mode; junction via
        // `mklink /J` needs only normal user rights.
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => Ok(()),
            Err(_) => {
                let status = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(link)
                    .arg(target)
                    .status();
                match status {
                    Ok(s) if s.success() => Ok(()),
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "symlink_dir + mklink /J both failed",
                    )),
                }
            }
        }
    }
}
