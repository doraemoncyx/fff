use crate::cursor::CursorStore;
use crate::output::{GrepFormatter, OutputMode, file_suffix};
use fff::grep::{GrepMode, GrepSearchOptions, has_regex_metacharacters};
use fff::types::{FileItem, PaginationArgs};
use fff::{FuzzySearchOptions, QueryParser, SharedFilePicker};
use fff_query_parser::AiGrepConfig;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const SCAN_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

const MAX_CONTEXT_LINES: usize = 100;

fn normalize_max_results(raw: Option<f64>, default: usize) -> usize {
    match raw {
        None => default,
        Some(v) if v <= 0.0 || !v.is_finite() => default,
        Some(v) => (v.round() as usize).max(1),
    }
}

// Context lines are copied per match, so a bogus float must not saturate to usize::MAX.
fn normalize_context(raw: Option<f64>) -> Option<usize> {
    let v = raw?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v.round() as usize).min(MAX_CONTEXT_LINES))
}

// Merge a separate `constraints` string (like multi_grep's) into the grep
// query so the parser treats them as file filters alongside inline tokens.
fn merge_constraints<'a>(constraints: &str, query: &'a str) -> Cow<'a, str> {
    if constraints.is_empty() {
        Cow::Borrowed(query)
    } else {
        Cow::Owned(format!("{constraints} {query}"))
    }
}

fn cleanup_fuzzy_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !matches!(c, ':' | '-' | '_') {
            out.extend(c.to_lowercase());
        }
    }
    out
}

fn make_grep_options(
    output_mode: OutputMode,
    mode: GrepMode,
    file_offset: usize,
    context: Option<usize>,
) -> (GrepSearchOptions, bool) {
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
    let auto_expand = !is_usage && ctx_lines == 0;
    let after_ctx = if auto_expand { 8 } else { ctx_lines };

    (
        GrepSearchOptions {
            max_file_size: 10 * 1024 * 1024,
            max_matches_per_file: matches_per_file,
            smart_case: true,
            casing: None,
            file_offset,
            page_limit: 50,
            mode,
            time_budget_ms: 0,
            enforce_time_budget: false,
            before_context: ctx_lines,
            after_context: after_ctx,
            classify_definitions: true,
            trim_whitespace: true,
            abort_signal: None,
        },
        auto_expand,
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindFilesParams {
    /// Fuzzy search query. Supports path prefixes and glob constraints.
    // `pattern` alias for consistency with grep's alias and the common
    // file-search parameter name (#311). Also tolerant of LLMs that emit
    // the multi-value query as a JSON array of terms.
    #[serde(alias = "pattern", deserialize_with = "deserialize_string")]
    pub query: String,
    /// Max results (default 20).
    #[serde(rename = "maxResults")]
    // this has to be float because llms are stupid
    pub max_results: Option<f64>,
    /// Cursor from previous result. Only use if previous results weren't sufficient.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub cursor: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    /// Search text or regex query with optional constraint prefixes.
    /// Matches within single lines only — use ONE specific term, not multiple words.
    // `pattern` alias: LLMs that have seen multi_grep (which uses `patterns`)
    // routinely call grep with `pattern`; accept it instead of erroring out
    // with an unhelpful "missing field `query`" (#311). Also tolerant of LLMs
    // that emit the query as a JSON array of terms.
    #[serde(alias = "pattern", deserialize_with = "deserialize_string")]
    pub query: String,
    /// Max matching lines (default 20).
    #[serde(rename = "maxResults")]
    pub max_results: Option<f64>, // this has to be float because llms are stupid
    /// Cursor from previous result. Only use if previous results weren't sufficient.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub cursor: Option<String>,
    /// Output format (default 'content').
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub output_mode: Option<String>,
    /// File constraints (e.g. '*.{ts,tsx} !test/'). Merged with any inline query constraints.
    // `default` keeps the field optional (absent -> None); `deserialize_with` then
    // only runs when the field is present, and tolerates null / array shapes.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub constraints: Option<String>,
    /// Number of context lines to show before/after each match (integer). NOT free-form notes or commands.
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub context: Option<f64>,
}

fn deserialize_patterns<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct PatternsVisitor;

    impl<'de> de::Visitor<'de> for PatternsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, an array of strings, or a stringified JSON array")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            // Try to parse as JSON array first
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(v)
            {
                return Ok(parsed);
            }
            Ok(vec![v.to_string()])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&v)
            {
                return Ok(parsed);
            }
            Ok(vec![v])
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

// 反序列化一个可选的"单字符串"字段（如 grep 的 constraints / find_files 的 query）。
// LLM 频繁把多值字段生成 JSON 数组（或字符串化数组），这里做形状容错统一归一为
// 空格连接的字符串；null / 缺失返回 None。映射 multi_grep.patterns 的容错策略。
fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct OptionalStringVisitor;

    impl<'de> de::Visitor<'de> for OptionalStringVisitor {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, an array of strings, a stringified JSON array, or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            // Try to parse as a stringified JSON array first
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(v)
            {
                return Ok(Some(parsed.join(" ")));
            }
            Ok(Some(v.to_string()))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&v)
            {
                return Ok(Some(parsed.join(" ")));
            }
            Ok(Some(v))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(Some(values.join(" ")))
        }
    }

    deserializer.deserialize_any(OptionalStringVisitor)
}

// 必填单字符串字段（grep / find_files 的 query）的反序列化器：复用上面的容错逻辑，
// 但 null / 缺失降级为空字符串以避免破坏必填字段的语义。
fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(deserialize_optional_string(deserializer)?.unwrap_or_default())
}

// 可选数字字段（context）的容错反序列化：接受 number / 数字字符串。
// LLM 会把备注对象塞进来——忽略降级为 None，别让整个调用失败。
fn deserialize_optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct OptionalNumberVisitor;

    impl<'de> de::Visitor<'de> for OptionalNumberVisitor {
        type Value = Option<f64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a number, a numeric string, or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v as f64))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v as f64))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(v.trim().parse().ok())
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(v.trim().parse().ok())
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map
                .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }

    deserializer.deserialize_any(OptionalNumberVisitor)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MultiGrepParams {
    /// Patterns to match (OR logic). Include all naming conventions: snake_case, PascalCase, camelCase.
    #[serde(deserialize_with = "deserialize_patterns")]
    pub patterns: Vec<String>,
    /// File constraints (e.g. '*.{ts,tsx} !test/'). ALWAYS provide when possible.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub constraints: Option<String>,
    /// Max matching lines (default 20).
    #[serde(rename = "maxResults")]
    pub max_results: Option<f64>,
    /// Cursor from previous result.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub cursor: Option<String>,
    /// Output format (default 'content').
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub output_mode: Option<String>,
    /// Number of context lines to show before/after each match (integer). NOT free-form notes or commands.
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub context: Option<f64>,
}

#[derive(Clone)]
pub struct FffServer {
    picker: SharedFilePicker,
    cursor_store: Arc<Mutex<CursorStore>>,
    update_notice_sent: Arc<AtomicBool>,
    last_activity: Arc<AtomicU64>,
    scan_ready: Arc<AtomicBool>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl FffServer {
    pub fn new(picker: SharedFilePicker) -> Self {
        Self {
            picker,
            cursor_store: Arc::new(Mutex::new(CursorStore::new())),
            update_notice_sent: Arc::new(AtomicBool::new(false)),
            last_activity: Arc::new(AtomicU64::new(now_secs())),
            scan_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn last_activity(&self) -> Arc<AtomicU64> {
        self.last_activity.clone()
    }

    fn bump_activity(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    fn wait_for_scan(&self, timeout: std::time::Duration) -> Result<(), ErrorData> {
        if self.scan_ready.load(Ordering::Relaxed) {
            return Ok(());
        }

        let deadline = std::time::Instant::now() + timeout;

        loop {
            let is_scanning = self
                .picker
                .read()
                .ok()
                .as_ref()
                .and_then(|g| g.as_ref())
                .map(|p| p.is_scan_active())
                .unwrap_or(true);

            if !is_scanning {
                self.scan_ready.store(true, Ordering::Relaxed);
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(ErrorData::internal_error(
                    "Index is still building; retry shortly",
                    None,
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn lock_cursors(&self) -> Result<std::sync::MutexGuard<'_, CursorStore>, ErrorData> {
        self.cursor_store.lock().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire cursor store lock: {e}"), None)
        })
    }

    fn maybe_append_update_notice(&self, result: &mut CallToolResult) {
        if self.update_notice_sent.swap(true, Ordering::Relaxed) {
            return;
        }
        let notice = crate::update_check::get_update_notice();
        if notice.is_empty() {
            // Reset so the next call can try again (check may still be in flight)
            self.update_notice_sent.store(false, Ordering::Relaxed);
            return;
        }
        result.content.push(Content::text(notice));
    }

    fn perform_grep(
        &self,
        query: &str,
        mode: GrepMode,
        max_results: usize,
        cursor_id: Option<&str>,
        output_mode: OutputMode,
        context: Option<usize>,
    ) -> Result<CallToolResult, ErrorData> {
        let file_offset = cursor_id
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let (options, auto_expand) = make_grep_options(output_mode, mode, file_offset, context);
        let ctx_lines = options.before_context;

        // Acquire picker lock once for the entire operation.
        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;

        let parser = QueryParser::new(AiGrepConfig);
        let parsed = parser.parse(query);
        let result = picker.grep(&parsed, &options);

        if result.matches.is_empty() && file_offset == 0 {
            // Auto-retry: broaden multi-word queries by dropping the first word,
            // but only when there are no parsed constraints (the first word is
            // then a search term, not a file filter like `*.rs` or `src/main.rs`).
            let parts: Vec<&str> = query.split_whitespace().collect();
            if parts.len() >= 2 && parsed.constraints.is_empty() {
                let rest_query = parts[1..].join(" ");
                let rest_parsed = parser.parse(&rest_query);

                let rest_text = rest_parsed.grep_text();
                let retry_mode = if has_regex_metacharacters(&rest_text) {
                    GrepMode::Regex
                } else {
                    mode
                };

                let (retry_options, _) = make_grep_options(output_mode, retry_mode, 0, context);
                let retry_result = picker.grep(&rest_parsed, &retry_options);

                if !retry_result.matches.is_empty() && retry_result.matches.len() <= 10 {
                    let mut cs = self.lock_cursors()?;
                    let text = &GrepFormatter {
                        matches: &retry_result.matches,
                        files: &retry_result.files,
                        total_matched: retry_result.matches.len(),
                        next_file_offset: retry_result.next_file_offset,
                        output_mode,
                        max_results,
                        show_context: ctx_lines > 0,
                        auto_expand_defs: auto_expand,
                        picker,
                    }
                    .format(&mut cs);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "0 matches for '{}'. Auto-broadened to '{}':\n{}",
                        query, rest_query, text
                    ))]));
                }
            }

            // Fuzzy fallback for typo tolerance
            let fuzzy_query = cleanup_fuzzy_query(query);
            let (fuzzy_options, _) = make_grep_options(output_mode, GrepMode::Fuzzy, 0, Some(0));
            let fuzzy_parsed = parser.parse(&fuzzy_query);
            let fuzzy_result = picker.grep(&fuzzy_parsed, &fuzzy_options);

            if !fuzzy_result.matches.is_empty() {
                let mut lines: Vec<String> = Vec::new();
                lines.push(format!(
                    "0 exact matches. {} approximate:",
                    fuzzy_result.matches.len()
                ));
                let mut current_file = String::new();
                for m in fuzzy_result.matches.iter().take(3) {
                    let file = fuzzy_result.files[m.file_index];
                    let file_rel = file.relative_path(picker);
                    if file_rel != current_file {
                        current_file = file_rel;
                        lines.push(current_file.to_string());
                    }
                    lines.push(format!(" {}: {}", m.line_number, m.line_content));
                }
                return Ok(CallToolResult::success(vec![Content::text(
                    lines.join("\n"),
                )]));
            }

            // File path fallback: if query looks like a path, suggest the matching file
            if query.contains('/') {
                let file_parser = QueryParser::default();
                let file_query = file_parser.parse(query);
                let file_opts = FuzzySearchOptions {
                    max_threads: 0,
                    current_file: None,
                    project_path: Some(picker.base_path()),
                    combo_boost_score_multiplier: 100,
                    min_combo_count: 3,
                    pagination: PaginationArgs {
                        offset: 0,
                        limit: 1,
                    },
                };
                let file_result = picker.fuzzy_search(&file_query, None, file_opts);
                if let (Some(top), Some(score)) =
                    (file_result.items.first(), file_result.scores.first())
                {
                    // Only suggest when the match is strong enough.
                    let query_len = query.len() as i32;
                    if score.base_score > query_len * 10 {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "0 content matches. But there is a relevant file path: {}",
                            top.relative_path(picker)
                        ))]));
                    }
                }
            }

            return Ok(CallToolResult::success(vec![Content::text(
                "0 matches.".to_string(),
            )]));
        }

        if result.matches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "0 matches.".to_string(),
            )]));
        }

        let mut cs = self.lock_cursors()?;
        let text = &GrepFormatter {
            matches: &result.matches,
            files: &result.files,
            total_matched: result.matches.len(),
            next_file_offset: result.next_file_offset,
            output_mode,
            max_results,
            show_context: ctx_lines > 0,
            auto_expand_defs: auto_expand,
            picker,
        }
        .format(&mut cs);

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_router]
impl FffServer {
    /// Fuzzy file search by name. Searches FILE NAMES, not file contents.
    /// Use it when you need to find a file, not a definition.
    /// Use grep instead for searching code content (definitions, usage patterns).
    /// Supports fuzzy matching, path prefixes ('shc/'), and glob constraints.
    /// IMPORTANT: Keep queries SHORT — prefer 1-2 terms max.
    #[tool(
        name = "find_files",
        description = "Fuzzy file search by name. Searches FILE NAMES, not file contents. Use it when you need to find a file, not a definition. Use grep instead for searching code content (definitions, usage patterns). Supports fuzzy matching, path prefixes ('src/'), and glob constraints ('name **/src/*.{ts,tsx} !test/'). IMPORTANT: Keep queries SHORT — prefer 1-2 terms max. Multiple words are a waterfall (each narrows results), NOT OR. If unsure, start broad with 1 term and refine.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn find_files(
        &self,
        Parameters(params): Parameters<FindFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.bump_activity();
        self.wait_for_scan(SCAN_READY_TIMEOUT)?;

        let max_results = normalize_max_results(params.max_results, 20);
        let query = &params.query;

        let page_offset = params
            .cursor
            .as_deref()
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;
        let base_path = picker.base_path();
        let make_opts = |offset: usize| FuzzySearchOptions {
            max_threads: 0,
            current_file: None,
            project_path: Some(base_path),
            combo_boost_score_multiplier: 100,
            min_combo_count: 3,
            pagination: PaginationArgs {
                offset,
                limit: max_results,
            },
        };

        let parser = QueryParser::default();
        let fff_query = parser.parse(query);
        let result = picker.fuzzy_search(&fff_query, None, make_opts(page_offset));
        let total_files = result.total_files;

        // Auto-retry with fewer terms if 3+ words return 0 results
        let words: Vec<&str> = query.split_whitespace().collect();
        let shorter = words.get(..2).map(|w| w.join(" "));

        let (items, scores, total_matched) =
            if result.items.is_empty() && words.len() >= 3 && page_offset == 0 {
                if let Some(shorter) = &shorter {
                    let shorter_query = parser.parse(shorter);
                    let retry = picker.fuzzy_search(&shorter_query, None, make_opts(0));

                    (retry.items, retry.scores, retry.total_matched)
                } else {
                    (result.items, result.scores, result.total_matched)
                }
            } else {
                (result.items, result.scores, result.total_matched)
            };

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "0 results ({} indexed)",
                total_files
            ))]));
        }

        let mut lines: Vec<String> = Vec::new();
        let top_item = items[0];
        let is_exact_match = scores[0].exact_match;

        if page_offset == 0 {
            if is_exact_match {
                lines.push(format!(
                    "→ Read {} (exact match!)",
                    top_item.relative_path(picker)
                ));
            } else if scores.len() < 2 || scores[0].total > scores[1].total.saturating_mul(2) {
                lines.push(format!(
                    "→ Read {} (best match — Read this file directly)",
                    top_item.relative_path(picker)
                ));
            }
        }

        let next_offset = page_offset + items.len();
        let has_more = next_offset < total_matched;

        if has_more {
            lines.push(format!("{}/{} matches", items.len(), total_matched));
        }

        for item in &items {
            lines.push(format!(
                "{}{}",
                item.relative_path(picker),
                file_suffix(item.git_status, item.total_frecency_score())
            ));
        }

        if has_more {
            let mut cs = self.lock_cursors()?;
            let cursor_id = cs.store(next_offset);
            lines.push(format!("cursor: {}", cursor_id));
        }

        let mut result = CallToolResult::success(vec![Content::text(lines.join("\n"))]);
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// Search file contents for text patterns. This is the DEFAULT search tool.
    /// Prefer plain text over regex. Filter files with constraints.
    #[tool(
        name = "grep",
        description = "Search file contents. Search for bare identifiers (e.g. 'InProgressQuote', 'ActorAuth'), NOT code syntax or regex. Filter files with constraints (e.g. '*.rs query', 'src/ query'). Use filename, directory (ending with /) or glob expressions to prefilter. See server instructions for constraint syntax and core rules.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.bump_activity();
        self.wait_for_scan(SCAN_READY_TIMEOUT)?;

        let max_results = normalize_max_results(params.max_results, 20);
        let output_mode = OutputMode::new(params.output_mode.as_deref());

        let query = merge_constraints(params.constraints.as_deref().unwrap_or(""), &params.query);
        let parsed = QueryParser::new(AiGrepConfig).parse(&query);
        let grep_text = parsed.grep_text();

        let mode = if has_regex_metacharacters(&grep_text) {
            GrepMode::Regex
        } else {
            GrepMode::PlainText
        };

        let mut result = self.perform_grep(
            &query,
            mode,
            max_results,
            params.cursor.as_deref(),
            output_mode,
            normalize_context(params.context),
        )?;
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// Search file contents for lines matching ANY of multiple patterns (OR logic).
    /// Patterns are literal text — NEVER escape special characters.
    #[tool(
        name = "multi_grep",
        description = "Search file contents for lines matching ANY of multiple patterns (OR logic). IMPORTANT: This returns files where ANY query matches, NOT all patterns. Patterns are literal text — NEVER escape special characters (no \\( \\) \\. etc). Faster than regex alternation for literal text. See server instructions for constraint syntax.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn multi_grep(
        &self,
        Parameters(params): Parameters<MultiGrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.bump_activity();
        self.wait_for_scan(SCAN_READY_TIMEOUT)?;

        let mut result = self.multi_grep_inner(params)?;
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }
}

impl FffServer {
    fn multi_grep_inner(&self, params: MultiGrepParams) -> Result<CallToolResult, ErrorData> {
        let max_results = normalize_max_results(params.max_results, 20);
        let context = normalize_context(params.context);
        let output_mode = OutputMode::new(params.output_mode.as_deref());

        let file_offset = params
            .cursor
            .as_deref()
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let (options, auto_expand) =
            make_grep_options(output_mode, GrepMode::PlainText, file_offset, context);

        let ctx_lines = options.before_context;
        let constraint_query = params.constraints.as_deref().unwrap_or("");
        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;
        let patterns_refs: Vec<&str> = params.patterns.iter().map(|s| s.as_str()).collect();

        let parser = QueryParser::new(AiGrepConfig);
        let constraints = parser.parse_constraints(constraint_query);

        let result = picker.multi_grep(&patterns_refs, &constraints, &options);
        let file_refs: Vec<&FileItem> = result.files.to_vec();

        if result.matches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "0 matches.".to_string(),
            )]));
        }

        let mut cs = self.lock_cursors()?;
        let text = &GrepFormatter {
            matches: &result.matches,
            files: &file_refs,
            total_matched: result.matches.len(),
            next_file_offset: result.next_file_offset,
            output_mode,
            max_results,
            show_context: ctx_lines > 0,
            auto_expand_defs: auto_expand,
            picker,
        }
        .format(&mut cs);

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for FffServer {
    fn get_info(&self) -> ServerInfo {
        let notice = crate::update_check::get_update_notice();
        let instructions = if notice.is_empty() {
            crate::MCP_INSTRUCTIONS.to_string()
        } else {
            format!("{}{}", crate::MCP_INSTRUCTIONS, notice)
        };

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fff", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_max_results_none_uses_default() {
        assert_eq!(normalize_max_results(None, 20), 20);
    }

    #[test]
    fn normalize_max_results_zero_uses_default() {
        // Issue #400: `maxResults: 0` must not return zero items for grep
        // while `find_files` returns the full set. Both tools now map 0 to
        // the default limit.
        assert_eq!(normalize_max_results(Some(0.0), 20), 20);
    }

    #[test]
    fn normalize_max_results_negative_uses_default() {
        assert_eq!(normalize_max_results(Some(-5.0), 20), 20);
    }

    #[test]
    fn normalize_max_results_non_finite_uses_default() {
        assert_eq!(normalize_max_results(Some(f64::NAN), 20), 20);
        assert_eq!(normalize_max_results(Some(f64::INFINITY), 20), 20);
    }

    #[test]
    fn normalize_max_results_rounds_and_clamps() {
        assert_eq!(normalize_max_results(Some(0.4), 20), 1);
        assert_eq!(normalize_max_results(Some(10.0), 20), 10);
        assert_eq!(normalize_max_results(Some(10.7), 20), 11);
    }

    #[test]
    fn grep_params_accepts_pattern_alias() {
        // Issue #311: LLMs flip between `query` and `pattern`; accept both.
        let via_query: GrepParams =
            serde_json::from_str(r#"{"query":"foo"}"#).expect("query field");
        assert_eq!(via_query.query, "foo");

        let via_pattern: GrepParams =
            serde_json::from_str(r#"{"pattern":"foo"}"#).expect("pattern alias");
        assert_eq!(via_pattern.query, "foo");
    }

    #[test]
    fn normalize_context_rejects_bogus_and_caps() {
        assert_eq!(normalize_context(None), None);
        assert_eq!(normalize_context(Some(3.4)), Some(3));
        assert_eq!(normalize_context(Some(-1.0)), None);
        assert_eq!(normalize_context(Some(f64::NAN)), None);
        assert_eq!(normalize_context(Some(1e308)), Some(MAX_CONTEXT_LINES));
    }

    #[test]
    fn grep_params_parses_context() {
        let params: GrepParams =
            serde_json::from_str(r#"{"query":"foo","context":3}"#).expect("context field");
        assert_eq!(params.context, Some(3.0));
    }

    #[test]
    fn context_accepts_numeric_string() {
        let params: GrepParams =
            serde_json::from_str(r#"{"query":"foo","context":"3"}"#).expect("numeric string");
        assert_eq!(params.context, Some(3.0));
    }

    #[test]
    fn context_ignores_freeform_object() {
        // LLMs stuff notes/commands into `context`; degrade to None instead of failing.
        let params: MultiGrepParams =
            serde_json::from_str(r#"{"patterns":["foo"],"context":{"command":"ls"}}"#)
                .expect("object context ignored");
        assert_eq!(params.context, None);
    }

    #[test]
    fn grep_params_accepts_constraints_field() {
        let params: GrepParams =
            serde_json::from_str(r#"{"query":"def show","constraints":"*.rs"}"#)
                .expect("constraints field");
        assert_eq!(params.query, "def show");
        assert_eq!(params.constraints.as_deref(), Some("*.rs"));
    }

    #[test]
    fn grep_params_accepts_constraints_array() {
        // LLMs often emit the multi-value `constraints` as a JSON array; join with spaces.
        let params: GrepParams =
            serde_json::from_str(r#"{"query":"Foo","constraints":["*.rs","!test/"]}"#)
                .expect("constraints array");
        assert_eq!(params.query, "Foo");
        assert_eq!(params.constraints.as_deref(), Some("*.rs !test/"));
    }

    #[test]
    fn grep_params_accepts_stringified_constraints_array() {
        let params: GrepParams =
            serde_json::from_str(r#"{"query":"Foo","constraints":"[\"*.rs\",\"!test/\"]"}"#)
                .expect("stringified constraints array");
        assert_eq!(params.constraints.as_deref(), Some("*.rs !test/"));
    }

    #[test]
    fn grep_params_accepts_null_constraints() {
        let params: GrepParams = serde_json::from_str(r#"{"query":"Foo","constraints":null}"#)
            .expect("null constraints");
        assert!(params.constraints.is_none());
    }

    #[test]
    fn grep_params_accepts_query_array() {
        let params: GrepParams =
            serde_json::from_str(r#"{"query":["fn","main"]}"#).expect("query array");
        assert_eq!(params.query, "fn main");
    }

    #[test]
    fn merge_constraints_prepends_and_handles_empty() {
        assert_eq!(merge_constraints("", "def show"), "def show");
        assert_eq!(merge_constraints("*.rs", "def show"), "*.rs def show");
        assert_eq!(
            merge_constraints("editor2022/imgui_viewer.py", "def show"),
            "editor2022/imgui_viewer.py def show"
        );
    }

    #[test]
    fn find_files_params_accepts_pattern_alias() {
        let via_query: FindFilesParams =
            serde_json::from_str(r#"{"query":"foo"}"#).expect("query field");
        assert_eq!(via_query.query, "foo");

        let via_pattern: FindFilesParams =
            serde_json::from_str(r#"{"pattern":"foo"}"#).expect("pattern alias");
        assert_eq!(via_pattern.query, "foo");
    }

    #[test]
    fn find_files_params_accepts_query_array() {
        let params: FindFilesParams =
            serde_json::from_str(r#"{"query":["src","main"]}"#).expect("query array");
        assert_eq!(params.query, "src main");
    }
}
