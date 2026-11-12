use std::path::Path;

use crate::error::ApiError;
use crate::params::PathSpec;

/// Scope fields that become glob constraints on the index.
pub(crate) struct ScopeInput<'a> {
    pub path: Option<&'a PathSpec>,
    pub paths: &'a [String],
    pub globs: &'a [String],
    pub hidden: Option<bool>,
}

/// Build Root-relative include/exclude globs for one index.
/// `prefix` is the tardir relative to that Root, empty when they are equal.
pub(crate) fn patterns(
    input: &ScopeInput<'_>,
    picker: &fff::FilePicker,
    prefix: &str,
) -> Result<Vec<(String, bool)>, ApiError> {
    let mut result = Vec::new();
    let mut roots: Vec<(String, bool)> = Vec::new();
    let mut includes = Vec::new();

    // A covering Index answers for the tardir subtree only: `{rel, rel/**}`.
    if !prefix.is_empty() {
        let escaped = escape_glob(prefix);
        result.push((format!("{{{escaped},{escaped}/**}}"), false));
    }

    if let Some(spec) = input.path {
        match spec {
            PathSpec::Prefix(rel) if !rel.is_empty() => {
                let escaped = escape_glob(rel);
                result.push((
                    with_prefix(prefix, &format!("{{{escaped},{escaped}/**}}")),
                    false,
                ));
                roots.push((rel.clone(), false));
            }
            PathSpec::Prefix(_) => {}
            PathSpec::Glob(glob) => {
                result.push((with_prefix(prefix, &descendant_glob(glob)), false));
            }
        }
    }

    let mut path_patterns = Vec::new();
    let mut all = false;
    for path in input.paths {
        let path = path.trim_matches('/');
        if path.is_empty() {
            all = true;
        } else {
            let escaped = escape_glob(path);
            path_patterns.push(escaped.clone());
            path_patterns.push(format!("{escaped}/**"));
            roots.push((path.to_string(), false));
        }
    }
    if !all && !path_patterns.is_empty() {
        result.push((with_prefix(prefix, &alternatives(&path_patterns)), false));
    }

    for glob in input.globs {
        let (pattern, exclude) = match glob.strip_prefix('!') {
            Some(pattern) => (pattern, true),
            None => (glob.as_str(), false),
        };
        if pattern.is_empty() {
            return Err(ApiError::invalid_params("glob must not be empty"));
        }
        let pattern = if let Some(rooted) = pattern.strip_prefix('/') {
            rooted.to_string()
        } else if !pattern.contains('/') {
            format!("**/{pattern}")
        } else {
            pattern.to_string()
        };
        if exclude {
            let pattern = if pattern.ends_with("/**") {
                descendant_glob(&pattern)
            } else {
                format!("{{{pattern},{pattern}/**}}")
            };
            result.push((with_prefix(prefix, &pattern), true));
        } else {
            includes.push(descendant_glob(&pattern));
        }
    }
    if !includes.is_empty() {
        result.push((with_prefix(prefix, &alternatives(&includes)), false));
    }

    if input.hidden == Some(false) {
        let explicit_hidden = roots
            .iter()
            .any(|(root, _)| root.split('/').any(|p| p.starts_with('.')));
        if includes.is_empty() && !explicit_hidden {
            result.push((with_prefix(prefix, "{**/.*,**/.*/**}"), true));
        } else {
            if includes.is_empty() {
                let mut visible = vec!["**/[!.]*".to_string()];
                visible.extend(
                    roots
                        .iter()
                        .filter(|(root, _)| !root.is_empty())
                        .map(|(root, _)| escape_glob(root)),
                );
                result.push((with_prefix(prefix, &alternatives(&visible)), false));
            }
            let mut builder = globset::GlobSetBuilder::new();
            for pattern in &includes {
                builder.add(compile_glob(pattern)?);
            }
            let whitelist = builder
                .build()
                .map_err(|e| ApiError::invalid_params(format!("Invalid glob: {e}")))?;
            let mut excluded = Vec::new();
            let mut overrides_hidden_directory = false;
            for dir in picker.get_dirs() {
                let path = dir.relative_path(picker);
                let path = path.trim_end_matches('/');
                if !under_prefix(path, prefix) {
                    continue;
                }
                if !path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with('.'))
                {
                    continue;
                }
                if whitelist.is_match(path)
                    || includes
                        .iter()
                        .any(|pattern| names_hidden_dir(pattern, path))
                    || roots
                        .iter()
                        .any(|(root, nested)| enters_root(root, *nested, path))
                {
                    overrides_hidden_directory = true;
                } else {
                    excluded.push(format!("{}/**", escape_glob(path)));
                }
            }
            if !overrides_hidden_directory {
                result.push((with_prefix(prefix, "**/.*/**/?*"), true));
            } else if !excluded.is_empty() {
                result.push((with_prefix(prefix, &alternatives(&excluded)), true));
            }
        }
    }

    for (pattern, _) in &result {
        compile_glob(pattern)?;
    }
    Ok(result)
}

pub(crate) fn compile_glob(pattern: &str) -> Result<globset::Glob, ApiError> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|e| ApiError::invalid_params(format!("Invalid glob: {e}")))
}

pub(crate) fn constraints(
    patterns: &[(String, bool)],
) -> impl Iterator<Item = fff::Constraint<'_>> {
    patterns.iter().map(|(pattern, exclude)| {
        let constraint = fff::Constraint::Glob(pattern);
        if *exclude {
            fff::Constraint::Not(Box::new(constraint))
        } else {
            constraint
        }
    })
}

/// Escape glob metacharacters so a literal path keeps its name.
pub(crate) fn escape_glob(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '*' | '?' | '[' | ']' | '{' | '}' | ',' | '\\' => escaped.extend(['[', c, ']']),
            _ => escaped.push(c),
        }
    }
    escaped
}

pub(crate) fn descendant_glob(pattern: &str) -> String {
    if pattern.ends_with("/**") {
        format!("{pattern}/?*")
    } else {
        pattern.to_string()
    }
}

fn with_prefix(prefix: &str, pattern: &str) -> String {
    if prefix.is_empty() {
        pattern.to_string()
    } else {
        format!("{}/{pattern}", escape_glob(prefix))
    }
}

/// Build a Root-relative pattern from a tardir-relative one.
pub(crate) fn prefix_pattern(prefix: &str, pattern: &str) -> String {
    with_prefix(prefix, pattern)
}

fn under_prefix(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
}

fn alternatives(patterns: &[String]) -> String {
    if patterns.len() == 1 {
        patterns[0].clone()
    } else {
        format!("{{{}}}", patterns.join(","))
    }
}

// True when a pattern spells out the hidden directory's own name, like `.github/**`.
fn names_hidden_dir(pattern: &str, dir: &str) -> bool {
    fn walk(pattern: &[&str], dir: &[&str]) -> bool {
        let (Some((&part, rest)), Some(&name)) = (pattern.split_first(), dir.first()) else {
            return false;
        };
        if part == "**" {
            return (dir.len() > 1 && walk(pattern, &dir[1..])) || walk(rest, dir);
        }
        let matches = globset::Glob::new(part)
            .map(|glob| glob.compile_matcher().is_match(name))
            .unwrap_or(false);
        match dir {
            [_] => matches && part.starts_with('.') && !rest.is_empty(),
            _ => matches && walk(rest, &dir[1..]),
        }
    }
    !pattern.contains('{')
        && walk(
            &pattern.split('/').collect::<Vec<_>>(),
            &dir.split('/').collect::<Vec<_>>(),
        )
}

// True when hidden `dir` holds the root, read from the root or, if nested, at any depth.
fn enters_root(root: &str, nested: bool, dir: &str) -> bool {
    let parts: Vec<&str> = root.split('/').collect();
    Path::new(root).starts_with(dir)
        || nested
            && (1..=parts.len()).any(|n| {
                let prefix = parts[..n].join("/");
                dir.strip_suffix(prefix.as_str())
                    .is_some_and(|rest| rest.ends_with('/'))
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_nested_roots_open_same_named_hidden_directories_elsewhere() {
        assert!(enters_root(".config", false, ".config"));
        assert!(enters_root(".github/workflows", false, ".github"));
        assert!(!enters_root(".config", false, "src/.config"));
        assert!(enters_root(".config", true, "src/.config"));
        assert!(enters_root(".github/workflows", true, "pkg/.github"));
        assert!(!enters_root(".config", true, "src/x.config"));
    }

    #[test]
    fn prefix_is_escaped_and_applied_once() {
        assert_eq!(with_prefix("", "a/**"), "a/**");
        assert_eq!(with_prefix("data", "a/**"), "data/a/**");
        assert!(with_prefix("a[b]", "x").starts_with("a["));
    }

    #[test]
    fn descendant_glob_requires_a_child() {
        assert_eq!(descendant_glob("sub/**"), "sub/**/?*");
        assert_eq!(descendant_glob("sub"), "sub");
    }
}
