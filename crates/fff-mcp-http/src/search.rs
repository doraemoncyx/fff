use std::borrow::Cow;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use fff::{FuzzySearchOptions, PaginationArgs};
use serde::Serialize;

use crate::error::ApiError;
use crate::index::Index;
use crate::params::{Mode, Params, Query};
use crate::registry::Registry;
use crate::root;
use crate::scope::{self, ScopeInput};

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchHit>,
    pub total: usize,
    pub scanned: usize,
    pub scanning: bool,
    pub served_by: Vec<String>,
    pub merged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub rel: String,
    pub score: i32,
    pub is_dir: bool,
}

pub(crate) struct Serving {
    pub index: Arc<Index>,
    /// tardir relative to this Root; non-empty only for a covering Index.
    pub prefix: String,
    /// Root relative to the tardir; non-empty only for an Index below it.
    pub base: String,
}

pub(crate) fn execute(registry: &Registry, params: &Params) -> Result<SearchResponse, ApiError> {
    let offset = resolve_offset(registry, params);
    let query = Query::normalize(params, offset)?;
    let serving = serving_set(registry, &query.tardir)?;

    let deadline = Instant::now() + registry.config().scan_wait;
    for entry in &serving {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        entry.index.wait_for_scan(remaining);
    }

    // A finished scan subsumes every Index below it. In-flight requests keep
    // their own Arcs, so this only drops the registry reference.
    registry.subsume_finished();

    let capacity = query.offset.saturating_add(query.limit);
    let mut hits = Vec::new();
    let mut total = 0usize;
    let mut scanned = 0usize;
    let mut scanning = false;
    let mut served_by = Vec::new();

    for entry in &serving {
        served_by.push(entry.index.root().to_string_lossy().into_owned());
        let progress = entry.index.progress();
        scanned += progress.files.max(progress.scanned);
        scanning |= progress.scanning;
        let (found, count) = evaluate(entry, &query, capacity)?;
        total += count;
        hits.extend(found);
    }

    // Each Index ranks against its own Root, so scores are only approximately
    // comparable across Roots; the window is bounded by a still-scanning parent.
    hits.sort_by(compare_hits);
    let collected = hits.len();
    let mut seen = std::collections::HashSet::new();
    hits.retain(|hit| seen.insert(hit.key.clone()));
    // Duplicates we merged away were counted once per Index; drop them from the
    // match total so paging does not promise a page that cannot exist.
    total = total.saturating_sub(collected - hits.len());

    let page: Vec<SearchHit> = hits
        .into_iter()
        .skip(query.offset)
        .take(query.limit)
        .map(|hit| SearchHit {
            path: hit.path,
            rel: hit.rel,
            score: hit.score,
            is_dir: false,
        })
        .collect();

    let next = query.offset + page.len();
    let cursor = (next < total).then(|| registry.store_cursor(next));

    Ok(SearchResponse {
        results: page,
        total,
        scanned,
        scanning,
        merged: serving.len() > 1,
        served_by,
        cursor,
    })
}

fn resolve_offset(registry: &Registry, params: &Params) -> usize {
    if let Some(offset) = params
        .cursor
        .as_deref()
        .and_then(|id| registry.cursor_offset(id))
    {
        return offset;
    }
    params
        .offset
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value as usize)
        .unwrap_or(0)
}

/// Serving set: the deepest Index covering the tardir, plus every Index whose
/// Root is strictly below it. Scope is evaluated per Root, so an Index below
/// the tardir answers with its own top matches — that merge only exists while
/// the parent is scan-in-progress.
fn serving_set(registry: &Registry, tardir: &Path) -> Result<Vec<Serving>, ApiError> {
    let registered = registry.indices();
    let covering = registered
        .iter()
        .filter(|index| root::covers(index.root(), tardir))
        .max_by_key(|index| index.key().len())
        .cloned();

    let mut serving = Vec::new();
    match covering {
        Some(index) => {
            let prefix = root::relative_to(tardir, index.root()).ok_or_else(|| {
                ApiError::internal(format!(
                    "cannot express {} below {}",
                    tardir.display(),
                    index.root().display()
                ))
            })?;
            registry.touch(&index);
            serving.push(Serving {
                index,
                prefix,
                base: String::new(),
            });
        }
        None => {
            let index = registry.ensure_index(tardir)?;
            serving.push(Serving {
                index,
                prefix: String::new(),
                base: String::new(),
            });
        }
    }

    let mut below: Vec<Arc<Index>> = registered
        .into_iter()
        .filter(|index| root::is_strict_descendant(index.root(), tardir))
        .collect();
    below.sort_by_key(|index| index.key().len());
    for index in below {
        let base = root::relative_to(index.root(), tardir).unwrap_or_default();
        registry.touch(&index);
        serving.push(Serving {
            index,
            prefix: String::new(),
            base,
        });
    }
    Ok(serving)
}

/// Tardir-relative view of a Root-relative index path.
fn tardir_relative<'a>(rel: &'a str, entry: &Serving) -> Option<Cow<'a, str>> {
    if entry.base.is_empty() {
        return strip_prefix(rel, &entry.prefix).map(Cow::Borrowed);
    }
    (!rel.is_empty()).then(|| Cow::Owned(format!("{}/{}", entry.base, rel)))
}

fn depth_of(rel: &str) -> usize {
    rel.matches('/').count()
}

struct Hit {
    key: String,
    path: String,
    rel: String,
    score: i32,
    tier: Option<u8>,
    depth: usize,
}

fn compare_hits(left: &Hit, right: &Hit) -> Ordering {
    Reverse(left.score)
        .cmp(&Reverse(right.score))
        .then(left.tier.cmp(&right.tier))
        .then(left.depth.cmp(&right.depth))
        .then_with(|| left.rel.cmp(&right.rel))
}

fn evaluate(
    entry: &Serving,
    query: &Query,
    capacity: usize,
) -> Result<(Vec<Hit>, usize), ApiError> {
    let guard = entry
        .index
        .picker()
        .read()
        .map_err(|e| ApiError::internal(format!("picker lock: {e}")))?;
    let picker = guard
        .as_ref()
        .ok_or_else(|| ApiError::internal("file picker not initialized"))?;
    let patterns = scope::patterns(
        &ScopeInput {
            path: query.path.as_ref(),
            paths: &query.paths,
            globs: &query.globs,
            hidden: query.hidden,
        },
        picker,
        &entry.prefix,
    )?;

    let fuzzy = query.mode == Mode::Fuzzy && query.filters.is_empty() && !query.raw.is_empty();
    if fuzzy {
        return Ok(fuzzy_search(entry, picker, query, &patterns, capacity));
    }
    literal_search(entry, picker, query, &patterns, capacity)
}

fn fuzzy_search(
    entry: &Serving,
    picker: &fff::FilePicker,
    query: &Query,
    patterns: &[(String, bool)],
    capacity: usize,
) -> (Vec<Hit>, usize) {
    let parser = fff::QueryParser::default();
    let mut parsed = parser.parse(&query.raw);
    parsed.constraints.extend(scope::constraints(patterns));

    let result = picker.fuzzy_search(
        &parsed,
        None,
        FuzzySearchOptions {
            max_threads: 0,
            current_file: None,
            project_path: Some(picker.base_path()),
            combo_boost_score_multiplier: 100,
            min_combo_count: 3,
            pagination: PaginationArgs {
                offset: 0,
                limit: capacity,
            },
        },
    );

    let hits = result
        .items
        .iter()
        .zip(&result.scores)
        .filter_map(|(file, score)| {
            let rel = file.relative_path(picker);
            let tardir_rel = tardir_relative(&rel, entry)?;
            let depth = depth_of(&tardir_rel);
            Some(Hit {
                key: absolute_key(picker, &rel),
                path: absolute_path(picker, &rel),
                rel: tardir_rel.into_owned(),
                score: score.total,
                tier: None,
                depth,
            })
        })
        .collect();
    (hits, result.total_matched)
}

struct Candidate {
    tier: u8,
    case_miss: bool,
    parent_miss: bool,
    depth: usize,
    frecency: i32,
    tardir_rel: String,
    index: usize,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.tier, self.case_miss, self.parent_miss, self.depth)
            .cmp(&(other.tier, other.case_miss, other.parent_miss, other.depth))
            .then(Reverse(self.frecency).cmp(&Reverse(other.frecency)))
            .then_with(|| self.tardir_rel.cmp(&other.tardir_rel))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

fn literal_search(
    entry: &Serving,
    picker: &fff::FilePicker,
    query: &Query,
    patterns: &[(String, bool)],
    capacity: usize,
) -> Result<(Vec<Hit>, usize), ApiError> {
    let regex = match query.mode {
        Mode::Glob => None,
        Mode::Fuzzy => Some(
            regex::RegexBuilder::new(&fragment_pattern(&query.raw))
                .case_insensitive(true)
                .build()
                .map_err(|e| ApiError::invalid_params(format!("Invalid pattern: {e}")))?,
        ),
        Mode::Regex => Some(
            regex::Regex::new(&query.raw)
                .map_err(|e| ApiError::invalid_params(format!("Invalid regex: {e}")))?,
        ),
    };

    let mut globs = patterns
        .iter()
        .map(|(pattern, exclude)| {
            scope::compile_glob(pattern).map(|glob| (glob.compile_matcher(), *exclude))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if query.mode == Mode::Glob && !query.raw.is_empty() {
        let pattern = if query.raw.contains('/') {
            query.raw.clone()
        } else {
            format!("**/{}", query.raw)
        };
        let pattern = scope::prefix_pattern(&entry.prefix, &pattern);
        globs.push((scope::compile_glob(&pattern)?.compile_matcher(), false));
    }

    let files = picker.get_files();
    let mut count = 0usize;
    let mut best: BinaryHeap<Candidate> = BinaryHeap::new();

    for (index, file) in files.iter().enumerate() {
        if file.is_deleted() {
            continue;
        }
        let rel = file.relative_path(picker);
        let Some(tardir_rel) = tardir_relative(&rel, entry) else {
            continue;
        };
        if regex
            .as_ref()
            .is_some_and(|regex| !regex.is_match(&tardir_rel))
            || query
                .filters
                .iter()
                .any(|filter| !filter.is_match(&tardir_rel))
            || globs
                .iter()
                .any(|(glob, exclude)| glob.is_match(&rel) == *exclude)
        {
            continue;
        }
        count += 1;
        let (tier, case_miss, parent_miss) = literal_rank(&tardir_rel, &query.raw);
        let candidate = Candidate {
            tier,
            case_miss,
            parent_miss,
            depth: depth_of(&tardir_rel),
            frecency: file.total_frecency_score(),
            tardir_rel: tardir_rel.into_owned(),
            index,
        };
        if best.len() < capacity {
            best.push(candidate);
        } else if best.peek().is_some_and(|worst| candidate < *worst) {
            *best.peek_mut().unwrap() = candidate;
        }
    }

    let hits = best
        .into_sorted_vec()
        .into_iter()
        .map(|candidate| {
            let rel = files[candidate.index].relative_path(picker);
            Hit {
                key: absolute_key(picker, &rel),
                path: absolute_path(picker, &rel),
                rel: candidate.tardir_rel,
                score: 0,
                tier: Some(candidate.tier),
                depth: candidate.depth,
            }
        })
        .collect();
    Ok((hits, count))
}

fn absolute_path(picker: &fff::FilePicker, rel: &str) -> String {
    let mut path = picker.base_path().to_path_buf();
    for part in rel.split('/') {
        path.push(part);
    }
    path.to_string_lossy().into_owned()
}

fn absolute_key(picker: &fff::FilePicker, rel: &str) -> String {
    let path = absolute_path(picker, rel).replace('\\', "/");
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path
    }
}

/// Tardir-relative view of a Root-relative index path.
fn strip_prefix<'a>(rel: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(rel);
    }
    if let Some(rest) = rel
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        return Some(rest);
    }
    #[cfg(windows)]
    {
        let lowered_rel = rel.to_ascii_lowercase();
        let lowered_prefix = prefix.to_ascii_lowercase();
        if lowered_rel.starts_with(&lowered_prefix)
            && lowered_rel.as_bytes().get(lowered_prefix.len()) == Some(&b'/')
        {
            return rel.get(lowered_prefix.len() + 1..);
        }
    }
    None
}

// Tier, then ties: an uppercase pattern prefers its exact case; exact names in `lsp/lsp.ts` beat `config/lsp.ts`.
fn literal_rank(path: &str, pattern: &str) -> (u8, bool, bool) {
    let mut parts = path.rsplit('/');
    let name = parts.next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let tier = if equal_fragment(name, pattern) || equal_fragment(stem, pattern) {
        0
    } else if path.split('/').any(|part| equal_fragment(part, pattern)) {
        1
    } else if name
        .get(..pattern.len())
        .is_some_and(|prefix| equal_fragment(prefix, pattern))
    {
        2
    } else {
        3
    };
    let case_miss = pattern.bytes().any(|b| b.is_ascii_uppercase()) && !path.contains(pattern);
    let parent_miss = tier == 0 && !parts.next().is_some_and(|dir| equal_fragment(dir, stem));
    (tier, case_miss, parent_miss)
}

fn fragment_pattern(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for (i, part) in pattern.split(['-', '_']).enumerate() {
        if i > 0 {
            escaped.push_str("[-_]");
        }
        escaped.push_str(&regex::escape(part));
    }
    escaped
}

fn equal_fragment(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left.bytes().zip(right.bytes()).all(|(a, b)| {
            a.eq_ignore_ascii_case(&b) || (matches!(a, b'-' | b'_') && matches!(b, b'-' | b'_'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_pattern_keeps_separator_classes() {
        assert_eq!(fragment_pattern("my_file"), "my[-_]file");
        assert_eq!(fragment_pattern("a.b"), "a\\.b");
    }

    #[test]
    fn literal_rank_prefers_exact_names() {
        assert_eq!(literal_rank("src/button.tsx", "button").0, 0);
        assert_eq!(literal_rank("src/button.tsx.bak", "button").0, 2);
        assert_eq!(literal_rank("src/other.ts", "button").0, 3);
    }

    #[test]
    fn strip_prefix_is_boundary_aware() {
        assert_eq!(strip_prefix("abc/def", ""), Some("abc/def"));
        assert_eq!(strip_prefix("abc/def", "abc"), Some("def"));
        assert_eq!(strip_prefix("abcd/def", "abc"), None);
    }
}
