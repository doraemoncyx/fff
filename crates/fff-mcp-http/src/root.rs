use std::path::{Path, PathBuf};

use crate::error::ApiError;

/// Canonicalize a tardir. Root identity is the canonical path, so `..`,
/// trailing separators, case and `\\?\` spellings all collapse onto one key.
pub(crate) fn canonical_dir(raw: &str) -> Result<PathBuf, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::invalid_params("tardir must not be empty"));
    }
    let path = Path::new(trimmed);
    let canonical = fff::path_utils::canonicalize(path)
        .map_err(|e| ApiError::invalid_params(format!("tardir does not exist: {trimmed} ({e})")))?;
    let meta = std::fs::metadata(&canonical).map_err(|e| {
        ApiError::invalid_params(format!("tardir is not accessible: {trimmed} ({e})"))
    })?;
    if !meta.is_dir() {
        return Err(ApiError::invalid_params(format!(
            "tardir is not a directory: {trimmed}"
        )));
    }
    Ok(canonical)
}

/// Registry key for a canonical Root.
pub(crate) fn root_key(path: &Path) -> String {
    let text = normalized_text(path);
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

/// True when `child` lies strictly below `parent`.
pub(crate) fn is_strict_descendant(child: &Path, parent: &Path) -> bool {
    let child = root_key(child);
    let parent = root_key(parent);
    child.len() > parent.len() && child.starts_with(&parent) && {
        let rest = &child[parent.len()..];
        rest.starts_with('/')
    }
}

/// True when `root` is the tardir or an ancestor of it.
pub(crate) fn covers(root: &Path, tardir: &Path) -> bool {
    is_strict_descendant(tardir, root) || root_key(root) == root_key(tardir)
}

/// Path of `tardir` relative to `root` as `/`-separated text, empty when equal.
/// Keeps the on-disk casing: both sides are canonicalized before this call.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
