use std::borrow::Cow;
use std::path::{Component, Path};

use serde::Deserialize;
use serde::de;

use crate::error::ApiError;
use crate::params::{deserialize_optional_number, deserialize_optional_string, deserialize_string};
use crate::scope::{
    alternatives, compile_glob, descendant_glob, enters_root, escape_glob, names_hidden_dir,
};

const MAX_CONTEXT_LINES: usize = 100;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(crate) struct GrepParams {
    /// Content pattern. Use mode="regex" for alternatives such as deadline|timeout.
    #[serde(
        alias = "pattern",
        alias = "querystr",
        deserialize_with = "deserialize_string"
    )]
    pub query: String,
    /// Absolute directory to search. Indexed on demand and kept in an LRU registry.
    pub tardir: String,
    /// Complete directory name, filename, or glob. Use **/*transport*/** for a partial module name.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub path: Option<String>,
    #[serde(flatten)]
    pub scope: SearchScope,
    /// Pattern syntax: auto (default), regex, or literal. Explicit modes preserve query verbatim.
    #[serde(default)]
    pub mode: Option<PatternMode>,
    /// sensitive, insensitive, or smart. Explicit pattern modes default to sensitive; auto uses smart.
    #[serde(default)]
    pub case: Option<CaseMode>,
    /// Max matching lines per page (default 100, or 30 with definitions_only). More remain available through cursor.
    #[serde(
        rename = "maxResults",
        default,
        deserialize_with = "deserialize_optional_number"
    )]
    pub max_results: Option<f64>,
    /// Cursor from previous result. Only use if previous results weren't sufficient.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub cursor: Option<String>,
    /// Output format (default 'content').
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub output_mode: Option<String>,
    /// Lines before and after each match. Omit for bounded automatic context, or use 0 for matching lines only.
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub context: Option<f64>,
    /// Context lines before each match; overrides context (rg -B).
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub before_context: Option<f64>,
    /// Context lines after each match; overrides context (rg -A).
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub after_context: Option<f64>,
    /// Stop after this many matches per file (rg -m). Omit to keep all matches pageable.
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub max_matches_per_file: Option<f64>,
    /// Filter matches to declaration names. Leave false when searching behavior, comments, or fields.
    #[serde(default)]
    pub definitions_only: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub(crate) struct MultiGrepParams {
    /// Patterns to match (OR logic). Include all naming conventions: snake_case, PascalCase, camelCase.
    #[serde(deserialize_with = "deserialize_patterns")]
    pub patterns: Vec<String>,
    /// Absolute directory to search. Indexed on demand and kept in an LRU registry.
    pub tardir: String,
    /// Known directory, module glob, filename, or extension, e.g. "src/net/" or "**/billing/**".
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub constraints: Option<String>,
    /// Max matching lines (default 20).
    #[serde(
        rename = "maxResults",
        default,
        deserialize_with = "deserialize_optional_number"
    )]
    pub max_results: Option<f64>,
    /// Cursor from previous result.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub cursor: Option<String>,
    /// Output format (default 'content').
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub output_mode: Option<String>,
    /// Context lines before/after each match.
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub context: Option<f64>,
    /// Return matching declarations. If none are found, report that and return other matches.
    #[serde(default)]
    pub definitions_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PatternMode {
    Auto,
    Regex,
    Literal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaseMode {
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Default, Clone, Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchScope {
    /// Files or directories relative to tardir. Search any of these paths.
    #[serde(default, deserialize_with = "optional_paths")]
    pub paths: Option<Vec<String>>,
    /// Include glob patterns (OR). Prefix a glob with ! to exclude matching paths.
    #[serde(default, deserialize_with = "optional_paths")]
    pub globs: Option<Vec<String>>,
    /// Include hidden indexed paths (default true). Explicit paths and include globs override false.
    #[serde(default)]
    pub hidden: Option<bool>,
}

impl SearchScope {
    // Root-relative include/exclude globs; `path_root` is the singular path scope.
    pub fn patterns(
        &self,
        picker: &fff::FilePicker,
        path_root: Option<(&str, bool)>,
    ) -> Result<Vec<(String, bool)>, ApiError> {
        let base = picker.base_path();
        let mut result = Vec::new();
        // The singular `path` scope is an explicit root too, so it can opt into hidden paths.
        let mut roots: Vec<(String, bool)> = path_root
            .map(|(root, nested)| (root.to_owned(), nested))
            .into_iter()
            .collect();
        if let Some(paths) = &self.paths {
            let mut patterns = Vec::new();
            let mut all = false;
            for path in paths {
                let path = Path::new(path);
                let path = if path.is_absolute() {
                    strip_root(path, base)?
                } else {
                    path.to_path_buf()
                };
                if path.components().any(|c| c == Component::ParentDir) {
                    return Err(ApiError::invalid_params("paths must not contain '..'"));
                }
                let path = path
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => Some(s.to_string_lossy()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                if path.is_empty() {
                    all = true;
                } else {
                    let escaped = escape_glob(&path);
                    patterns.push(escaped.clone());
                    patterns.push(format!("{escaped}/**"));
                }
                roots.push((path, false));
            }
            if !all && !patterns.is_empty() {
                result.push((alternatives(&patterns), false));
            }
        }
        let mut includes = Vec::new();
        for glob in self.globs.iter().flatten() {
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
                result.push((pattern, true));
            } else {
                includes.push(descendant_glob(&pattern));
            }
        }
        if !includes.is_empty() {
            result.push((alternatives(&includes), false));
        }
        if self.hidden == Some(false) {
            let explicit_hidden = roots
                .iter()
                .any(|(root, _)| root.split('/').any(|part| part.starts_with('.')));
            if includes.is_empty() && !explicit_hidden {
                result.push(("{**/.*,**/.*/**}".into(), true));
            } else {
                if includes.is_empty() {
                    let mut visible = vec!["**/[!.]*".to_string()];
                    visible.extend(
                        roots
                            .iter()
                            .filter(|(root, _)| !root.is_empty())
                            .map(|(root, _)| escape_glob(root)),
                    );
                    result.push((alternatives(&visible), false));
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
                    result.push(("**/.*/**/?*".into(), true));
                } else if !excluded.is_empty() {
                    result.push((alternatives(&excluded), true));
                }
            }
        }
        for (pattern, _) in &result {
            compile_glob(pattern)?;
        }
        Ok(result)
    }

    pub fn is_explicit(&self) -> bool {
        self.paths.is_some() || self.globs.is_some()
    }
}

// Relocate a user scope from tardir space into the Index's Root space.
pub(crate) fn retarget_scope(
    scope: &mut SearchScope,
    prefix: &str,
    tardir: &Path,
) -> Result<(), ApiError> {
    if prefix.is_empty() {
        return Ok(());
    }
    if let Some(paths) = scope.paths.as_mut() {
        for path in paths.iter_mut() {
            let relative = crate::params::resolve_relative(path, tardir)?;
            *path = join_path(prefix, &relative);
        }
    }
    if let Some(globs) = scope.globs.as_mut() {
        for glob in globs.iter_mut() {
            *glob = retarget_glob(prefix, glob);
        }
    }
    Ok(())
}

pub(crate) fn retarget_path(path: &str, prefix: &str, tardir: &Path) -> Result<String, ApiError> {
    if prefix.is_empty() {
        return Ok(path.to_string());
    }
    let relative = crate::params::resolve_relative(path, tardir)?;
    Ok(join_path(prefix, &relative))
}

// Rooted patterns move down with the tardir; `**/name` already holds at any depth.
fn retarget_glob(prefix: &str, glob: &str) -> String {
    let (negated, pattern) = match glob.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, glob),
    };
    let rooted = pattern.strip_prefix('/').unwrap_or(pattern);
    let anchored = pattern.starts_with('/') || (rooted.contains('/') && !rooted.starts_with("**/"));
    let mapped = if anchored && !rooted.is_empty() {
        join_path(prefix, rooted)
    } else {
        rooted.to_string()
    };
    if negated {
        format!("!{mapped}")
    } else {
        mapped
    }
}

fn join_path(prefix: &str, relative: &str) -> String {
    match (prefix.is_empty(), relative.is_empty()) {
        (_, true) => prefix.to_string(),
        (true, _) => relative.to_string(),
        (false, _) => format!("{prefix}/{relative}"),
    }
}

// Escaping glob so a subtree prefix keeps its literal name.
pub(crate) fn subtree_pattern(prefix: &str) -> String {
    let escaped = escape_glob(prefix);
    format!("{{{escaped},{escaped}/**}}")
}

pub(crate) fn normalize_max_results(raw: Option<f64>, default: usize) -> usize {
    match raw {
        None => default,
        Some(v) if v <= 0.0 || !v.is_finite() => default,
        Some(v) => (v.round() as usize).clamp(1, 10_000),
    }
}

// Context lines are copied per match, so a bogus float must not saturate to usize::MAX.
pub(crate) fn normalize_context(raw: Option<f64>) -> Option<usize> {
    let v = raw?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v.round() as usize).min(MAX_CONTEXT_LINES))
}

pub(crate) fn make_grep_options(
    output_mode: crate::grep_output::OutputMode,
    mode: fff::grep::GrepMode,
    file_offset: usize,
    context: Option<usize>,
) -> (fff::grep::GrepSearchOptions, bool) {
    use crate::grep_output::OutputMode;
    let is_usage = output_mode == OutputMode::Usage;
    let matches_per_file = match output_mode {
        OutputMode::FilesWithMatches => 1,
        _ if is_usage => 8,
        _ => 10,
    };
    let ctx_lines = if is_usage {
        context.unwrap_or(1)
    } else {
        context.unwrap_or(0)
    };
    let auto_expand = !is_usage && context.is_none();
    let after_ctx = if auto_expand { 16 } else { ctx_lines };

    (
        fff::grep::GrepSearchOptions {
            max_file_size: 10 * 1024 * 1024,
            max_matches_per_file: matches_per_file,
            smart_case: true,
            casing: None,
            file_offset,
            page_limit: 50,
            mode,
            time_budget_ms: 0,
            enforce_time_budget: false,
            before_context: if auto_expand { 8 } else { ctx_lines },
            after_context: after_ctx,
            classify_definitions: true.into(),
            trim_whitespace: false,
            abort_signal: None,
        },
        auto_expand,
    )
}

// Escapes a literal pattern that must run through the regex engine.
pub(crate) fn literal_regex(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            // `[\\]` matches a backslash without spelling the `\n` escape core rewrites.
            '\\' => "[\\\\]".to_string(),
            c => regex::escape(c.encode_utf8(&mut [0; 4])),
        })
        .collect()
}

pub(crate) fn parse_scoped_grep<'a>(
    parser: &fff::QueryParser<fff::AiGrepConfig>,
    query: &'a str,
    scope: Option<&'a str>,
) -> fff::FFFQuery<'a> {
    let mut parsed = parser.parse(query);
    if let Some(scope) = scope {
        parsed.constraints.push(fff::Constraint::Glob(scope));
        // A separate path must survive retries that drop ambiguous inline constraints.
        parsed.raw_query = "";
    }
    parsed
}

// Case-insensitive like core's file-path constraint, so `readme.md` finds README.md.
pub(crate) fn indexes_file_path(picker: &fff::FilePicker, suffix: &str) -> bool {
    let suffix = suffix.replace('\\', "/").to_ascii_lowercase();
    picker.get_files().iter().any(|file| {
        let path = file
            .relative_path(picker)
            .replace('\\', "/")
            .to_ascii_lowercase();
        !file.is_deleted()
            && path
                .strip_suffix(suffix.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.ends_with('/'))
    })
}

pub(crate) fn file_path_constraint<'a>(query: &fff::FFFQuery<'a>) -> Option<&'a str> {
    query.constraints.iter().find_map(|c| match c {
        fff::Constraint::FilePath(path) => Some(*path),
        _ => None,
    })
}

#[derive(Debug, PartialEq)]
pub(crate) struct PathScope {
    pub glob: String,
    pub root: String,
    // The name matches at any depth rather than from the checkout root.
    pub nested: bool,
    pub notice: Option<String>,
}

pub(crate) fn path_scope(
    path: Option<&str>,
    label: Option<&str>,
    picker: &fff::FilePicker,
) -> Result<Option<PathScope>, ApiError> {
    let Some(raw) = path.map(str::trim).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let label = label.unwrap_or(raw);
    let given = std::path::Path::new(raw);
    let absolute = given.is_absolute();
    let path = if absolute {
        strip_root(given, picker.base_path())?
    } else {
        given.to_path_buf()
    };
    if path
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(ApiError::invalid_params(
            "path must not contain '..' components",
        ));
    }
    let path = path
        .to_str()
        .ok_or_else(|| ApiError::invalid_params("path must be UTF-8"))?;
    #[cfg(windows)]
    let normalized = path.replace('\\', "/");
    #[cfg(windows)]
    let path = normalized.as_str();
    let path = path
        .strip_prefix("./")
        .unwrap_or(path)
        .trim_end_matches('/');
    if path.is_empty() || path == "." {
        return Ok(None);
    }
    let escaped = escape_glob(path);
    let scope = |glob: String, found: bool, nested: bool| PathScope {
        glob,
        root: path.to_owned(),
        nested,
        notice: (!found).then(|| format!("Path '{label}' matches no indexed file or directory.")),
    };
    let found = lookup_path(picker, path);
    if absolute || found.anchored {
        return Ok(Some(scope(
            format!("{{{escaped},{escaped}/**}}"),
            found.anchored,
            false,
        )));
    }
    if !found.nested
        && let Some((spelled, anchored, _)) = found.respelled
    {
        let escaped = escape_glob(&spelled);
        return Ok(Some(PathScope {
            glob: if anchored {
                format!("{{{escaped},{escaped}/**}}")
            } else {
                format!("{{**/{escaped},**/{escaped}/**}}")
            },
            notice: Some(format!(
                "Path '{label}' matches no indexed file or directory; searched '{spelled}' instead."
            )),
            root: spelled,
            nested: !anchored,
        }));
    }
    // A literal name such as `src` or `[id]` at any depth wins over reading it as a glob.
    if found.nested || !has_wildcards(path) {
        return Ok(Some(scope(
            format!("{{**/{escaped},**/{escaped}/**}}"),
            found.nested,
            true,
        )));
    }
    Ok(Some(scope(descendant_glob(path), true, false)))
}

pub(crate) fn strip_root(
    path: &std::path::Path,
    base: &std::path::Path,
) -> Result<std::path::PathBuf, ApiError> {
    if let Ok(relative) = path.strip_prefix(base) {
        return Ok(relative.to_path_buf());
    }
    // Symlinked or short-name spellings of the root still name the checkout; missing tails are kept.
    let (existing, real) = path
        .ancestors()
        .find_map(|dir| Some((dir, std::fs::canonicalize(dir).ok()?)))
        .ok_or_else(|| ApiError::invalid_params("path must be inside the indexed checkout"))?;
    let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    real.strip_prefix(&base)
        .ok()
        .zip(path.strip_prefix(existing).ok())
        .map(|(inside, missing)| inside.join(missing))
        .ok_or_else(|| ApiError::invalid_params("path must be inside the indexed checkout"))
}

#[derive(Default)]
struct PathLookup {
    anchored: bool,
    nested: bool,
    // Spelling that differs in case or `-`/`_`: (path, at the checkout root, index order).
    respelled: Option<(String, bool, usize)>,
}

// One pass over the index: an exact checkout path wins, otherwise note a match at any depth.
fn lookup_path(picker: &fff::FilePicker, path: &str) -> PathLookup {
    let dirs = picker.get_dirs();
    let mut found = PathLookup::default();
    for (index, dir) in dirs.iter().enumerate() {
        found = merge_lookups(found, visit(index, dir.relative_path(picker), path));
    }
    for (index, file) in picker.get_files().iter().enumerate() {
        if file.is_deleted() {
            continue;
        }
        let order = dirs.len() + index;
        found = merge_lookups(found, visit(order, file.relative_path(picker), path));
    }
    found
}

fn visit(order: usize, candidate: String, path: &str) -> PathLookup {
    let mut found = PathLookup::default();
    #[cfg(windows)]
    let candidate = candidate.replace('\\', "/");
    let candidate = candidate.trim_end_matches('/');
    match candidate.strip_suffix(path) {
        Some("") => found.anchored = true,
        Some(rest) if rest.ends_with('/') => found.nested = true,
        _ => {
            if let Some(start) = candidate.len().checked_sub(path.len())
                && (start == 0 || candidate.as_bytes()[start - 1] == b'/')
                && let Some(tail) = candidate.get(start..)
                && same_spelling(tail, path)
            {
                found.respelled = Some((tail.to_owned(), start == 0, order));
            }
        }
    }
    found
}

fn same_spelling(indexed: &str, given: &str) -> bool {
    indexed.len() == given.len()
        && indexed.bytes().zip(given.bytes()).all(|(a, b)| {
            a.eq_ignore_ascii_case(&b) || (matches!(a, b'-' | b'_') && matches!(b, b'-' | b'_'))
        })
}

// Prefer a checkout-level respelling, then the earliest indexed one.
fn merge_lookups(mut left: PathLookup, right: PathLookup) -> PathLookup {
    left.anchored |= right.anchored;
    left.nested |= right.nested;
    left.respelled = match (left.respelled, right.respelled) {
        (Some(a), Some(b)) => Some(if (!b.1, b.2) < (!a.1, a.2) { b } else { a }),
        (a, b) => a.or(b),
    };
    left
}

fn deserialize_patterns<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct PatternsVisitor;

    impl<'de> de::Visitor<'de> for PatternsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, an array of strings, or a stringified JSON array")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(v)
            {
                return Ok(parsed);
            }
            Ok(vec![v.to_string()])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(PatternsVisitor)
}

// GET /grep accepts repeated `paths`/`globs` keys as well as a JSON array literal.
fn optional_paths<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct PathsVisitor;

    impl<'de> de::Visitor<'de> for PathsVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, an array of strings, or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            self.visit_string(value.to_string())
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            if value.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&value)
            {
                return Ok(Some(parsed));
            }
            Ok(Some(vec![value]))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(Some(values))
        }
    }

    deserializer.deserialize_any(PathsVisitor)
}

const GREP_NUMBER_KEYS: &[&str] = &[
    "maxResults",
    "context",
    "before_context",
    "after_context",
    "max_matches_per_file",
];
const GREP_BOOL_KEYS: &[&str] = &["hidden", "definitions_only"];

impl GrepParams {
    pub fn from_pairs<'a>(
        pairs: impl Iterator<Item = (Cow<'a, str>, Cow<'a, str>)>,
    ) -> Result<Self, ApiError> {
        let object = crate::params::pairs_object(pairs, GREP_NUMBER_KEYS, GREP_BOOL_KEYS)?;
        serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|e| ApiError::invalid_params(format!("invalid parameters: {e}")))
    }
}

impl MultiGrepParams {
    pub fn from_pairs<'a>(
        pairs: impl Iterator<Item = (Cow<'a, str>, Cow<'a, str>)>,
    ) -> Result<Self, ApiError> {
        let object = crate::params::pairs_object(pairs, GREP_NUMBER_KEYS, GREP_BOOL_KEYS)?;
        serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|e| ApiError::invalid_params(format!("invalid parameters: {e}")))
    }
}

fn has_wildcards(path: &str) -> bool {
    fff::glob_detect::has_wildcards(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_results_defaults_and_clamps() {
        assert_eq!(normalize_max_results(None, 100), 100);
        assert_eq!(normalize_max_results(Some(0.0), 30), 30);
        assert_eq!(normalize_max_results(Some(-2.0), 30), 30);
        assert_eq!(normalize_max_results(Some(5.6), 30), 6);
        assert_eq!(normalize_max_results(Some(1e9), 30), 10_000);
    }

    #[test]
    fn context_never_saturates() {
        assert_eq!(normalize_context(None), None);
        assert_eq!(normalize_context(Some(-1.0)), None);
        assert_eq!(normalize_context(Some(f64::INFINITY)), None);
        assert_eq!(normalize_context(Some(2.4)), Some(2));
        assert_eq!(normalize_context(Some(1e9)), Some(MAX_CONTEXT_LINES));
    }

    #[test]
    fn get_pairs_read_repeated_patterns_and_booleans() {
        let pairs = [
            ("query", "needle"),
            ("tardir", "C:/x"),
            ("definitions_only", "true"),
            ("context", "2"),
            ("globs", "*.rs"),
            ("globs", "!vendor"),
        ];
        let params = GrepParams::from_pairs(
            pairs
                .into_iter()
                .map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))),
        )
        .unwrap();
        assert_eq!(params.query, "needle");
        assert!(params.definitions_only);
        assert_eq!(params.context, Some(2.0));
        assert_eq!(
            params.scope.globs,
            Some(vec!["*.rs".to_string(), "!vendor".to_string()])
        );
    }

    #[test]
    fn multi_grep_pairs_become_patterns() {
        let pairs = [
            ("patterns", "alpha"),
            ("patterns", "beta"),
            ("tardir", "C:/x"),
        ];
        let params = MultiGrepParams::from_pairs(
            pairs
                .into_iter()
                .map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))),
        )
        .unwrap();
        assert_eq!(
            params.patterns,
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn flattened_scope_fields_deserialize() {
        let params: GrepParams = serde_json::from_value(serde_json::json!({
            "query": "x",
            "tardir": "C:/x",
            "paths": ["src"],
            "globs": ["*.rs", "!vendor"],
            "hidden": false
        }))
        .unwrap();
        assert_eq!(params.scope.paths, Some(vec!["src".to_string()]));
        assert_eq!(
            params.scope.globs,
            Some(vec!["*.rs".to_string(), "!vendor".to_string()])
        );
        assert_eq!(params.scope.hidden, Some(false));
    }

    #[test]
    fn retargeting_moves_rooted_globs_into_the_root() {
        assert_eq!(retarget_glob("sub", "src/*.rs"), "sub/src/*.rs");
        assert_eq!(retarget_glob("sub", "**/x.rs"), "**/x.rs");
        assert_eq!(retarget_glob("sub", "!/out"), "!sub/out");
        assert_eq!(join_path("sub", "src"), "sub/src");
        assert_eq!(join_path("", "src"), "src");
        assert_eq!(subtree_pattern("data"), "{data,data/**}");
        assert_eq!(subtree_pattern("a[b]"), "{a[[]b[]],a[[]b[]]/**}");
    }
}
