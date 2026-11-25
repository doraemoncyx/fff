use std::path::{Path, PathBuf};

use crate::error::ApiError;

// A tardir names a directory or one file: `dir` is the Index key (a file's
// parent), `file` the name to scope to inside it.
pub(crate) struct Target {
    dir: PathBuf,
    file: Option<String>,
}

impl Target {
    pub fn parse(raw: &str) -> Result<Self, ApiError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ApiError::invalid_params("tardir must not be empty"));
        }
        // Root identity is the canonical path, so `..`, trailing separators,
        // case and `\\?\` spellings all collapse onto one key.
        let canonical = fff::path_utils::canonicalize(Path::new(trimmed)).map_err(|error| {
            match without_trailing_separator(trimmed) {
                // Windows rejects `file.rs\` before any is_dir check, so the
                // separator, not a missing path, is what failed.
                Some(stripped) if std::fs::metadata(stripped).is_ok_and(|meta| meta.is_file()) => {
                    ApiError::invalid_params(format!(
                        "tardir names a file, so its trailing separator must be removed: {stripped}"
                    ))
                }
                _ => {
                    ApiError::invalid_params(format!("tardir does not exist: {trimmed} ({error})"))
                }
            }
        })?;
        let meta = std::fs::metadata(&canonical).map_err(|e| {
            ApiError::invalid_params(format!("tardir is not accessible: {trimmed} ({e})"))
        })?;
        if meta.is_dir() {
            return Ok(Self {
                dir: canonical,
                file: None,
            });
        }
        let file = canonical
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| ApiError::invalid_params(format!("tardir names no file: {trimmed}")))?;
        let dir = canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
            ApiError::invalid_params(format!("tardir has no parent directory: {trimmed}"))
        })?;
        Ok(Self {
            dir,
            file: Some(file),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    // True when a tardir-relative path names the target file itself.
    pub fn names_file(&self, relative: &str) -> bool {
        self.file
            .as_deref()
            .is_some_and(|name| same_path(name, relative))
    }

    // The target's path below `root`: `join(relative_to(dir, root), file)`.
    pub fn prefix_in(&self, root: &Path) -> Option<String> {
        let relative = relative_to(&self.dir, root)?;
        Some(match &self.file {
            Some(name) if relative.is_empty() => name.clone(),
            Some(name) => format!("{relative}/{name}"),
            None => relative,
        })
    }

    // A file target is searchable only while the Index holds a live, non-binary,
    // non-empty entry for it.
    pub fn searchable_in(&self, picker: &fff::FilePicker) -> bool {
        if self.file().is_none() {
            return true;
        }
        let Some(relative) = self.prefix_in(picker.base_path()) else {
            return false;
        };
        picker.get_files().iter().any(|file| {
            !file.is_deleted()
                && !file.is_binary()
                && file.size > 0
                && same_path(&file.relative_path(picker), &relative)
        })
    }

    // A file target whose parent cannot be a Root gets a message naming the file.
    pub fn index_error(&self, error: ApiError) -> ApiError {
        let Some(name) = self.file() else {
            return error;
        };
        ApiError::invalid_params(format!(
            "cannot search {name}: its directory cannot be indexed as a Root ({}); read the file directly instead",
            error.message()
        ))
    }
}

// True when both strings name the same path spelling.
pub(crate) fn same_path(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

// Registry key for a canonical Root.
pub(crate) fn root_key(path: &Path) -> String {
    let text = normalized_text(path);
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

// True when `child_key` lies strictly below `parent_key`; both from `root_key`.
pub(crate) fn key_is_strict_descendant(child_key: &str, parent_key: &str) -> bool {
    child_key
        .strip_prefix(parent_key)
        .is_some_and(|rest| rest.starts_with('/'))
}

// True when `child` lies strictly below `parent`.
pub(crate) fn is_strict_descendant(child: &Path, parent: &Path) -> bool {
    key_is_strict_descendant(&root_key(child), &root_key(parent))
}

// True when `root` is the tardir or an ancestor of it.
pub(crate) fn covers(root: &Path, tardir: &Path) -> bool {
    is_strict_descendant(tardir, root) || root_key(root) == root_key(tardir)
}

// Path of `tardir` relative to `root` as `/`-separated text, empty when equal.
// Keeps the on-disk casing: both sides are canonicalized before this call.
pub(crate) fn relative_to(tardir: &Path, root: &Path) -> Option<String> {
    if let Ok(rest) = tardir.strip_prefix(root) {
        return Some(rest.to_string_lossy().replace('\\', "/"));
    }
    #[cfg(windows)]
    {
        // Short-name or differently cased spellings still name the tardir.
        let tardir_text = normalized_text(tardir);
        let root_text = normalized_text(root);
        if tardir_text.len() > root_text.len()
            && tardir_text[..root_text.len()].eq_ignore_ascii_case(&root_text)
            && tardir_text.as_bytes()[root_text.len()] == b'/'
        {
            return Some(tardir_text[root_text.len() + 1..].to_string());
        }
    }
    None
}

fn normalized_text(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_string();
    text.trim_end_matches('/').to_string()
}

// The same spelling without its trailing separator, unless that is a drive root.
fn without_trailing_separator(raw: &str) -> Option<&str> {
    let stripped = raw.trim_end_matches(['/', '\\']);
    (stripped.len() < raw.len() && !stripped.is_empty() && !stripped.ends_with(':'))
        .then_some(stripped)
}

// The parameter error a tardir over the Root cap gets: what refused it, and
// which subdirectories a narrower tardir could name instead.
pub(crate) fn cap_error(root: &str, cap: usize, counted: usize, subdirs: &[String]) -> ApiError {
    let named = if subdirs.is_empty() {
        "it has no subdirectories".to_string()
    } else {
        format!("its top-level subdirectories are {}", subdirs.join(", "))
    };
    ApiError::invalid_params(format!(
        "tardir is over the Root cap: {root} is limited to {cap} files, and the walk stopped after \
         {counted} files, so nothing was indexed for it. Narrow the tardir and retry — {named}."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_message_names_the_cap_the_count_and_the_subdirectories() {
        let subdirs = vec!["art".to_string(), "src".to_string()];
        let message = cap_error("C:/work/game", 500_000, 500_003, &subdirs)
            .message()
            .to_string();
        for expected in [
            "Root cap",
            "500000",
            "500003",
            "art, src",
            "Narrow the tardir",
        ] {
            assert!(message.contains(expected), "{message}");
        }

        let flat = cap_error("C:/work/game", 500_000, 500_003, &[]);
        let message = flat.message().to_string();
        assert!(message.contains("no subdirectories"), "{message}");
    }

    #[test]
    fn key_folds_separators_and_trailing_slash() {
        let a = root_key(Path::new("C:/work/game/"));
        let b = root_key(Path::new("C:\\work\\game"));
        assert_eq!(a, b);
    }

    #[test]
    fn descendant_is_prefix_bounded() {
        assert!(is_strict_descendant(
            Path::new("C:/work/game/data"),
            Path::new("C:/work/game")
        ));
        assert!(!is_strict_descendant(
            Path::new("C:/work/game2"),
            Path::new("C:/work/game")
        ));
        assert!(!is_strict_descendant(
            Path::new("C:/work/game"),
            Path::new("C:/work/game")
        ));
    }

    #[test]
    fn relative_to_strips_the_root_once() {
        assert_eq!(
            relative_to(Path::new("C:/a/b"), Path::new("C:/a")).as_deref(),
            Some("b")
        );
        assert_eq!(
            relative_to(Path::new("C:/a"), Path::new("C:/a")).as_deref(),
            Some("")
        );
        assert_eq!(relative_to(Path::new("C:/x"), Path::new("C:/a")), None);
    }

    #[test]
    fn file_target_prefix_joins_the_name() {
        let target = Target {
            dir: PathBuf::from("C:/a"),
            file: Some("main.rs".into()),
        };
        assert_eq!(
            target.prefix_in(Path::new("C:/a")).as_deref(),
            Some("main.rs")
        );
        assert_eq!(
            target.prefix_in(Path::new("C:/")).as_deref(),
            Some("a/main.rs")
        );
        assert_eq!(target.prefix_in(Path::new("C:/b")), None);
        assert_eq!(target.names_file("MAIN.RS"), cfg!(windows));
        assert!(!target.names_file("other.rs"));
    }

    #[test]
    fn directory_target_prefix_is_unchanged() {
        let target = Target {
            dir: PathBuf::from("C:/a/src"),
            file: None,
        };
        assert_eq!(target.prefix_in(Path::new("C:/a")).as_deref(), Some("src"));
        assert!(!target.names_file(""));
    }

    #[test]
    fn trailing_separator_only_strips_real_paths() {
        assert_eq!(
            without_trailing_separator("C:/a/file.rs\\"),
            Some("C:/a/file.rs")
        );
        assert_eq!(without_trailing_separator("C:/a/"), Some("C:/a"));
        assert_eq!(without_trailing_separator("C:/"), None);
        assert_eq!(without_trailing_separator("C:"), None);
        assert_eq!(without_trailing_separator(""), None);
    }
}
