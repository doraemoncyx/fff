use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use fff::grep::{GrepMode, GrepSearchOptions, has_regex_metacharacters};
use fff::{FuzzySearchOptions, PaginationArgs, QueryParser};
use serde::Serialize;

use crate::cursor::{CursorStore, GrepCursor, GrepStage};
use crate::error::ApiError;
use crate::grep_output::{GrepFormatter, OutputMode, declarations_first};
use crate::grep_params::{
    CaseMode, GrepParams, MultiGrepParams, PatternMode, file_path_constraint, indexes_file_path,
    literal_regex, make_grep_options, normalize_context, normalize_max_results, parse_scoped_grep,
    path_scope, retarget_path, retarget_scope, subtree_pattern,
};
use crate::index::Index;
use crate::registry::Registry;

// Grep pages print every match, so a huge maxResults would flood the caller.
const MAX_GREP_PAGE: usize = 200;
// Definition-only searches read past usages to fill a page.
const DEFINITION_PAGE: usize = 30;
const FUZZY_STAGE_BUDGET_MS: u64 = 2_000;

const PARTIAL_NOTE: &str = "Note: this Index is still scanning, so these results are partial. Repeat the request for the complete set.";
const PENDING_NOTE: &str = "Note: the Index for this tardir could not start in time, so these results are partial. Repeat the request for the complete set.";

#[derive(Debug, Serialize)]
pub(crate) struct GrepText {
    pub text: String,
    pub scanning: bool,
    pub scanned: usize,
}

pub(crate) fn grep(registry: &Registry, mut params: GrepParams) -> Result<GrepText, ApiError> {
    let mode = match params.mode {
        Some(PatternMode::Regex) => GrepMode::Regex,
        // Plain grep reads `\n` as a newline, so keep literal backslashes in a regex.
        Some(PatternMode::Literal) if params.query.contains("\\n") => {
            params.query = literal_regex(&params.query);
            GrepMode::Regex
        }
        Some(PatternMode::Literal) => GrepMode::PlainText,
        _ => {
            let parsed = QueryParser::new(fff::AiGrepConfig).parse(&params.query);
            if has_regex_metacharacters(&parsed.grep_text()) {
                GrepMode::Regex
            } else {
                GrepMode::PlainText
            }
        }
    };
    let tardir = params.tardir.clone();
    complete(registry, &tardir, |selection, picker, tardir| {
        let label = params.path.clone();
        if let Some(path) = params.path.as_deref() {
            params.path = Some(retarget_path(path, &selection.prefix, tardir)?);
        }
        retarget_scope(&mut params.scope, &selection.prefix, tardir)?;
        perform_grep(
            registry,
            selection.index.key(),
            picker,
            &selection.prefix,
            label.as_deref(),
            &params,
            mode,
        )
    })
}

pub(crate) fn multi_grep(
    registry: &Registry,
    params: MultiGrepParams,
) -> Result<GrepText, ApiError> {
    let tardir = params.tardir.clone();
    complete(registry, &tardir, |selection, picker, _| {
        multi_grep_inner(
            registry,
            selection.index.key(),
            picker,
            &selection.prefix,
            &params,
        )
    })
}

struct Selection {
    index: Arc<Index>,
    // tardir relative to the Index's Root; empty when the Root is the tardir.
    prefix: String,
    scanning: bool,
    scanned: usize,
}

// One request runs against a single Index: merging Roots would break paging.
fn select(registry: &Registry, tardir: &Path) -> Result<Option<Selection>, ApiError> {
    let deadline = Instant::now() + registry.config().scan_wait;
    let covering = registry
        .indices()
        .iter()
        .filter(|index| crate::root::covers(index.root(), tardir))
        .max_by_key(|index| index.key().len())
        .cloned();
    let (index, prefix) = match covering {
        Some(index) => {
            let prefix = crate::root::relative_to(tardir, index.root()).ok_or_else(|| {
                ApiError::internal(format!(
                    "cannot express {} below {}",
                    tardir.display(),
                    index.root().display()
                ))
            })?;
            (Some(index), prefix)
        }
        None => match registry.ensure_index(tardir, deadline)? {
            Some(index) => (Some(index), String::new()),
            None => (None, String::new()),
        },
    };
    let Some(index) = index else {
        return Ok(None);
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    let complete = index.wait_for_scan(remaining);
    registry.subsume_finished();
    registry.touch(&index);
    let progress = index.progress();
    Ok(Some(Selection {
        prefix,
        scanning: !complete,
        scanned: progress.files.max(progress.scanned),
        index,
    }))
}

fn complete(
    registry: &Registry,
    tardir: &str,
    build: impl FnOnce(&Selection, &fff::FilePicker, &Path) -> Result<String, ApiError>,
) -> Result<GrepText, ApiError> {
    let tardir = crate::root::canonical_dir(tardir)?;
    registry.mark_activity();
    let Some(selection) = select(registry, &tardir)? else {
        return Ok(GrepText {
            text: format!("{PENDING_NOTE}\n0 matches."),
            scanning: true,
            scanned: 0,
        });
    };

    let text = {
        let guard = selection
            .index
            .picker()
            .read()
            .map_err(|e| ApiError::internal(format!("picker lock: {e}")))?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ApiError::internal("file picker not initialized"))?;
        build(&selection, picker, &tardir)?
    };
    let text = if selection.scanning {
        format!("{PARTIAL_NOTE}\n{text}")
    } else {
        text
    };
    Ok(GrepText {
        text,
        scanning: selection.scanning,
        scanned: selection.scanned,
    })
}

#[allow(clippy::too_many_arguments)]
fn perform_grep(
    registry: &Registry,
    root_key: &str,
    picker: &fff::FilePicker,
    prefix: &str,
    label: Option<&str>,
    params: &GrepParams,
    mode: GrepMode,
) -> Result<String, ApiError> {
    let query = params.query.as_str();
    let default_page = if params.definitions_only {
        DEFINITION_PAGE
    } else {
        100
    };
    let max_results = normalize_max_results(params.max_results, default_page).min(MAX_GREP_PAGE);
    let output_mode = OutputMode::new(params.output_mode.as_deref());
    let context = normalize_context(params.context);
    let mut cursor = params
        .cursor
        .as_deref()
        .and_then(|id| registry.grep_cursor(root_key, id))
        .unwrap_or(GrepCursor {
            stage: if params.definitions_only {
                GrepStage::Definitions
            } else {
                GrepStage::Normal
            },
            ..Default::default()
        });
    let prefer_definitions = !matches!(cursor.stage, GrepStage::Normal | GrepStage::Broadened);
    let broadened_query = (cursor.stage == GrepStage::Broadened).then(|| {
        query
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    });

    let (mut options, mut auto_expand) =
        make_grep_options(output_mode, mode, cursor.file_offset, context);
    options.page_limit = cursor.page_limit(max_results);
    options.max_matches_per_file = if output_mode == OutputMode::FilesWithMatches {
        1
    } else {
        params
            .max_matches_per_file
            .filter(|n| n.is_finite() && *n >= 1.0)
            .map_or(usize::MAX, |n| n as usize)
            .min(options.page_limit)
    };
    options.casing = params
        .case
        .or_else(|| {
            matches!(params.mode, Some(PatternMode::Regex | PatternMode::Literal))
                .then_some(CaseMode::Sensitive)
        })
        .map(|case| match case {
            CaseMode::Smart => fff::grep::Casing::Smart,
            CaseMode::Sensitive => fff::grep::Casing::Sensitive,
            CaseMode::Insensitive => fff::grep::Casing::Insensitive,
        });
    if params.before_context.is_some() || params.after_context.is_some() {
        options.before_context = normalize_context(params.before_context)
            .or(context)
            .unwrap_or(0);
        options.after_context = normalize_context(params.after_context)
            .or(context)
            .unwrap_or(0);
        auto_expand = false;
    }
    let ctx_lines = options.before_context;
    let show_context = !auto_expand && (ctx_lines > 0 || options.after_context > 0);
    let suggestion_context = (!auto_expand).then_some((ctx_lines, options.after_context));

    let parser = QueryParser::new(fff::AiGrepConfig);
    let path_scope = path_scope(params.path.as_deref(), label, picker)?;
    let scope = path_scope.as_ref().map(|scope| scope.glob.as_str());
    let root = path_scope
        .as_ref()
        .map(|scope| (scope.root.as_str(), scope.nested));
    let mut patterns = params.scope.patterns(picker, root)?;
    if !prefix.is_empty() {
        patterns.insert(0, (subtree_pattern(prefix), false));
    }
    let verbatim = params.scope.is_explicit()
        || matches!(params.mode, Some(PatternMode::Regex | PatternMode::Literal));
    let mut original = if verbatim {
        fff::FFFQuery {
            raw_query: "",
            fuzzy_query: fff::FuzzyQuery::Text(if query.is_empty() && mode == GrepMode::Regex {
                "^"
            } else {
                query
            }),
            constraints: scope.map(fff::Constraint::Glob).into_iter().collect(),
            location: None,
        }
    } else {
        parse_scoped_grep(&parser, query, scope)
    };
    original
        .constraints
        .extend(crate::scope::constraints(&patterns));
    if params.scope.is_explicit() || !patterns.is_empty() {
        original.raw_query = "";
    }
    if let Some(shorter) = &broadened_query {
        original.fuzzy_query = parser.parse(shorter).fuzzy_query;
        original.raw_query = "";
    }
    let mode = if broadened_query.is_some() && has_regex_metacharacters(&original.grep_text()) {
        GrepMode::Regex
    } else {
        mode
    };
    let fresh = cursor.stage == GrepStage::Definitions && cursor.file_offset == 0;
    let missing_path = file_path_constraint(&original)
        .filter(|path| cursor.missing_path || (fresh && !indexes_file_path(picker, path)));
    if missing_path.is_some() {
        original
            .constraints
            .retain(|c| !matches!(c, fff::Constraint::FilePath(_)));
        cursor.missing_path = true;
    }
    let path_notice = path_scope
        .as_ref()
        .and_then(|scope| scope.notice.clone())
        .or_else(|| {
            missing_path.map(|path| {
                format!("No indexed file matches '{path}'. Searched all files instead.")
            })
        });
    let respond = |text: String, definitions_fallback: bool| {
        with_notice(
            grep_response(text, definitions_fallback),
            path_notice.as_deref(),
        )
    };
    let fuzzy_query = prefer_definitions.then(|| cleanup_fuzzy_query(&original.grep_text()));
    let mut fuzzy = None;
    let mut result = loop {
        options.classify_definitions = cursor.stage.definition_mode();
        options.file_offset = cursor.file_offset;
        options.mode = if cursor.stage.is_fuzzy() {
            GrepMode::Fuzzy
        } else {
            mode
        };
        // Fuzzy stages read every candidate file, so bound each one on large indexes.
        (options.time_budget_ms, options.enforce_time_budget) = if cursor.stage.is_fuzzy() {
            (FUZZY_STAGE_BUDGET_MS, true)
        } else {
            (0, false)
        };
        let parsed = if cursor.stage.is_fuzzy() {
            fuzzy.get_or_insert_with(|| {
                let mut parsed =
                    parse_scoped_grep(&parser, fuzzy_query.as_deref().unwrap_or(query), scope);
                parsed.constraints = original.constraints.clone();
                parsed.raw_query = "";
                parsed
            })
        } else {
            &original
        };
        let mut result = picker.grep(parsed, &options);
        let skip = cursor.resume_skip(&result, picker);
        if skip < cursor.match_offset {
            // The resumed file shrank, so search again with the offset that still applies.
            cursor.match_offset = skip;
            options.page_limit = cursor.page_limit(max_results);
            result = picker.grep(parsed, &options);
        }
        if matches!(params.mode, Some(PatternMode::Regex))
            && let Some(error) = &result.regex_fallback_error
        {
            return Err(ApiError::invalid_params(format!(
                "Invalid regex: {error}. Use mode=literal to search this text literally."
            )));
        }
        if prefer_definitions && result.literal_fallback {
            // A literal retry drops exclusions, so treat it as an empty stage.
            result.matches.clear();
            result.files.clear();
            result.next_file_offset = 0;
            result.literal_fallback = false;
        }
        if cursor.stage.is_fuzzy() && result.matches.is_empty() {
            // A budget spent without matches ends the stage like an empty full search.
            result.next_file_offset = 0;
        }
        if result.matches.is_empty()
            && result.next_file_offset == 0
            && !cursor.found_matches
            && let Some(next) = cursor.stage.next()
        {
            cursor.stage = next;
            cursor.file_offset = 0;
            continue;
        }
        break result;
    };
    if cursor.stage == GrepStage::Normal
        && result.matches.is_empty()
        && result.next_file_offset == 0
        && !verbatim
        && original.raw_query.is_empty()
        && original.constraints.len() > usize::from(scope.is_some()) + patterns.len()
        && !query.trim().is_empty()
    {
        // Core's literal retry would drop the path scope, so retry inside it here.
        let literal = fff::FFFQuery {
            raw_query: "",
            constraints: scope
                .map(fff::Constraint::Glob)
                .into_iter()
                .chain(crate::scope::constraints(&patterns))
                .collect(),
            fuzzy_query: fff::FuzzyQuery::Text(query.trim()),
            location: None,
        };
        let retry = picker.grep(&literal, &options);
        if !retry.matches.is_empty() {
            result = retry;
            result.literal_fallback = true;
        }
    }

    if cursor.stage == GrepStage::Normal
        && result.matches.is_empty()
        && result.next_file_offset == 0
        && options.file_offset == 0
    {
        // Auto-retry: try broadening multi-word queries by dropping the first word.
        let parts: Vec<&str> = query.split_whitespace().collect();
        if !verbatim && parts.len() >= 2 {
            let first_word = parts[0];
            let is_valid_constraint = first_word.starts_with('!')
                || first_word.starts_with('*')
                || first_word.ends_with('/');

            if !is_valid_constraint {
                let rest_query = parts[1..].join(" ");
                let mut rest_parsed = parse_scoped_grep(&parser, &rest_query, scope);
                rest_parsed.constraints = original.constraints.clone();
                rest_parsed.raw_query = "";

                let rest_text = rest_parsed.grep_text();
                let retry_mode = if has_regex_metacharacters(&rest_text) {
                    GrepMode::Regex
                } else {
                    mode
                };

                let mut retry_options = options.clone();
                retry_options.mode = retry_mode;
                retry_options.file_offset = 0;
                let mut retry_result = picker.grep(&rest_parsed, &retry_options);

                if !retry_result.matches.is_empty() && retry_result.matches.len() <= 10 {
                    cursor.stage = GrepStage::Broadened;
                    cursor.paginate(&mut retry_result, max_results, picker);
                    declarations_first(&mut retry_result);
                    let text = registry.with_cursors(|cs| {
                        GrepFormatter {
                            matches: &retry_result.matches,
                            files: &retry_result.files,
                            total_matched: retry_result.matches.len(),
                            cursor,
                            output_mode,
                            max_results,
                            show_context,
                            auto_expand_defs: auto_expand,
                            picker,
                            root: root_key,
                        }
                        .format(cs)
                    });
                    return Ok(respond(
                        format!(
                            "0 matches for '{}'. Auto-broadened to '{}':\n{}",
                            query, rest_query, text
                        ),
                        false,
                    ));
                }
            }
        }

        // Fuzzy fallback for typo tolerance
        let fuzzy_query = cleanup_fuzzy_query(&original.grep_text());
        let (mut fuzzy_options, _) = make_grep_options(output_mode, GrepMode::Fuzzy, 0, Some(0));
        fuzzy_options.classify_definitions = options.classify_definitions;
        fuzzy_options.casing = options.casing;
        let mut fuzzy_parsed = parse_scoped_grep(&parser, &fuzzy_query, scope);
        fuzzy_parsed.constraints = original.constraints.clone();
        fuzzy_parsed.raw_query = "";
        let mut fuzzy_result = if original
            .grep_text()
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '-'))
        {
            picker.grep(&fuzzy_parsed, &fuzzy_options)
        } else {
            fff::GrepResult::default()
        };
        fuzzy_result
            .matches
            .retain(|m| strong_suggestion(m, fuzzy_query.len(), original.grep_text().len()));
        fuzzy_result
            .matches
            .sort_by_key(|m| std::cmp::Reverse(m.fuzzy_score));

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
            return Ok(respond(lines.join("\n"), false));
        }

        // File path fallback: if the query looks like a path, suggest the matching file.
        if query.contains('/') {
            let file_parser = QueryParser::default();
            let mut file_query = file_parser.parse(query);
            file_query
                .constraints
                .extend(scope.map(fff::Constraint::Glob));
            file_query
                .constraints
                .extend(crate::scope::constraints(&patterns));
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
                let query_len = query.len() as i32;
                if score.base_score > query_len * 10 {
                    return Ok(respond(
                        format!(
                            "0 content matches. But there is a relevant file path: {}",
                            top.relative_path(picker)
                        ),
                        false,
                    ));
                }
            }
        }

        return Ok(respond(
            suggest_other_files(
                picker,
                &original,
                mode,
                options.effective_casing(),
                params,
                false,
                suggestion_context,
            )
            .unwrap_or_else(|| "0 matches.".into()),
            false,
        ));
    }

    if result.matches.is_empty() && result.next_file_offset == 0 {
        if prefer_definitions
            && !cursor.found_matches
            && let Some(suggestions) = suggest_other_files(
                picker,
                &original,
                mode,
                options.effective_casing(),
                params,
                true,
                suggestion_context,
            )
        {
            return Ok(respond(suggestions, false));
        }
        return Ok(respond(
            "0 matches.".into(),
            prefer_definitions && !cursor.found_matches,
        ));
    }

    let notice = if !cursor.found_matches || cursor.stage.is_fuzzy() {
        cursor.stage.notice()
    } else {
        None
    };
    cursor.paginate(&mut result, max_results, picker);
    declarations_first(&mut result);
    if result.matches.is_empty() && !cursor.has_more() {
        return Ok(respond("0 matches.".into(), false));
    }

    let text = registry.with_cursors(|cs| {
        GrepFormatter {
            matches: &result.matches,
            files: &result.files,
            total_matched: result.matches.len(),
            cursor,
            output_mode,
            max_results,
            show_context,
            auto_expand_defs: auto_expand,
            picker,
            root: root_key,
        }
        .format(cs)
    });

    let text = match (broadened_query, notice) {
        (Some(shorter), _) => {
            format!("0 matches for '{query}'. Auto-broadened to '{shorter}':\n{text}")
        }
        (_, Some(notice)) => format!("{notice}\n{text}"),
        _ => text,
    };
    Ok(respond(text, false))
}

fn multi_grep_inner(
    registry: &Registry,
    root_key: &str,
    picker: &fff::FilePicker,
    prefix: &str,
    params: &MultiGrepParams,
) -> Result<String, ApiError> {
    let max_results = normalize_max_results(params.max_results, 20).min(MAX_GREP_PAGE);
    let context = normalize_context(params.context);
    let output_mode = OutputMode::new(params.output_mode.as_deref());

    let mut cursor = params
        .cursor
        .as_deref()
        .and_then(|id| registry.grep_cursor(root_key, id))
        .unwrap_or(GrepCursor {
            stage: if params.definitions_only {
                GrepStage::Definitions
            } else {
                GrepStage::Normal
            },
            ..Default::default()
        });

    let (mut options, auto_expand) = make_grep_options(
        output_mode,
        GrepMode::PlainText,
        cursor.file_offset,
        context,
    );
    options.page_limit = cursor.page_limit(max_results);
    options.max_matches_per_file = if output_mode == OutputMode::FilesWithMatches {
        1
    } else {
        options.page_limit
    };
    if cursor.stage == GrepStage::Definitions {
        options.classify_definitions = fff::DefinitionMode::DefinitionOnly;
    }

    let ctx_lines = options.before_context;
    let constraint_query = params.constraints.as_deref().unwrap_or("");
    let patterns_refs: Vec<&str> = params.patterns.iter().map(|s| s.as_str()).collect();

    let parser = QueryParser::new(fff::AiGrepConfig);
    let mut constraints = parser.parse_constraints(constraint_query);
    let subtree = (!prefix.is_empty()).then(|| subtree_pattern(prefix));
    if let Some(subtree) = &subtree {
        constraints.push(fff::Constraint::Glob(subtree));
    }

    let mut result = picker.multi_grep(&patterns_refs, &constraints, &options);
    let skip = cursor.resume_skip(&result, picker);
    if skip < cursor.match_offset {
        cursor.match_offset = skip;
        options.page_limit = cursor.page_limit(max_results);
        result = picker.multi_grep(&patterns_refs, &constraints, &options);
    }
    let definitions_fallback = cursor.stage == GrepStage::Definitions
        && !cursor.found_matches
        && result.matches.is_empty()
        && result.next_file_offset == 0;
    if definitions_fallback {
        cursor = GrepCursor {
            stage: GrepStage::References,
            ..Default::default()
        };
        options.file_offset = 0;
        options.classify_definitions = fff::DefinitionMode::Classify;
        result = picker.multi_grep(&patterns_refs, &constraints, &options);
    }
    if result.matches.is_empty() && result.next_file_offset == 0 {
        return Ok(grep_response("0 matches.".into(), definitions_fallback));
    }

    cursor.paginate(&mut result, max_results, picker);
    declarations_first(&mut result);
    if result.matches.is_empty() && !cursor.has_more() {
        return Ok(grep_response("0 matches.".into(), false));
    }
    let text = registry.with_cursors(|cs| {
        GrepFormatter {
            matches: &result.matches,
            files: &result.files,
            total_matched: result.matches.len(),
            cursor,
            output_mode,
            max_results,
            show_context: !auto_expand && ctx_lines > 0,
            auto_expand_defs: auto_expand,
            picker,
            root: root_key,
        }
        .format(cs)
    });

    Ok(grep_response(text, definitions_fallback))
}

fn grep_response(text: String, definitions_fallback: bool) -> String {
    if definitions_fallback && text == "0 matches." {
        "No matching definitions or other matches found.".to_string()
    } else if definitions_fallback {
        format!("No matching definitions found. Showing other matches:\n{text}")
    } else {
        text
    }
}

fn with_notice(text: String, notice: Option<&str>) -> String {
    match notice {
        Some(notice) => format!("{notice}\n{text}"),
        None => text,
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

fn suggest_other_files(
    picker: &fff::FilePicker,
    query: &fff::FFFQuery<'_>,
    mode: GrepMode,
    casing: fff::grep::Casing,
    params: &GrepParams,
    definitions: bool,
    context: Option<(usize, usize)>,
) -> Option<String> {
    let mut broader = query.clone();
    broader.raw_query = "";
    broader
        .constraints
        .retain(|c| matches!(c, fff::Constraint::Exclude(_) | fff::Constraint::Not(_)));
    let explicit = broader.constraints.len();
    if params.scope.hidden == Some(false) {
        // A named hidden root does not open other hidden paths to suggestions.
        let hidden = fff::Constraint::Glob("{**/.*,**/.*/**}");
        broader
            .constraints
            .push(fff::Constraint::Not(Box::new(hidden)));
    }
    if explicit < query.constraints.len() {
        let started = std::time::Instant::now();
        let mut options = GrepSearchOptions {
            mode,
            casing: Some(casing),
            max_matches_per_file: 3,
            page_limit: 8,
            before_context: context.map_or(8, |(before, _)| before),
            after_context: context.map_or(8, |(_, after)| after),
            classify_definitions: true.into(),
            trim_whitespace: false,
            time_budget_ms: 100,
            enforce_time_budget: true,
            ..Default::default()
        };
        if let Some(fragment) = params.path.as_deref().map(str::trim).filter(|path| {
            !path.is_empty()
                && path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        }) {
            let glob = format!("{{**/*{fragment}*,**/*{fragment}*/**}}");
            let mut nearby = broader.clone();
            nearby.constraints.push(fff::Constraint::Glob(&glob));
            let result = picker.grep(&nearby, &options);
            if !result.matches.is_empty() {
                return Some(format_broader_matches(
                    picker,
                    &result,
                    context,
                    &format!(
                        "No matches in the requested scope. Suggestions from paths containing '{fragment}' (broader scope):"
                    ),
                ));
            }
        }
        options.time_budget_ms = 100_u64.saturating_sub(started.elapsed().as_millis() as u64);
        if options.time_budget_ms > 0 {
            let result = picker.grep(&broader, &options);
            if !result.matches.is_empty() {
                return Some(format_broader_matches(
                    picker,
                    &result,
                    context,
                    "No matches in the requested scope. Suggestions elsewhere in the indexed project:",
                ));
            }
        }
    }
    let text = cleanup_fuzzy_query(&query.grep_text());
    if text.is_empty() {
        return None;
    }
    broader.fuzzy_query = fff::FuzzyQuery::Text(&text);
    let result = picker.fuzzy_search(
        &broader,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: 3,
            },
            ..Default::default()
        },
    );
    let files = result
        .items
        .iter()
        .zip(&result.scores)
        .filter(|(_, score)| score.base_score > text.len() as i32 * 10)
        .map(|(file, _)| file.relative_path(picker))
        .collect::<Vec<_>>();
    let empty = if definitions {
        "No matching definitions or other matches found"
    } else {
        "No matches found"
    };
    (!files.is_empty()).then(|| format!("{empty} in the requested scope. These filenames may be relevant; their contents are unverified:\n{}", files.join("\n")))
}

fn format_broader_matches(
    picker: &fff::FilePicker,
    result: &fff::GrepResult<'_>,
    context: Option<(usize, usize)>,
    notice: &str,
) -> String {
    let text = GrepFormatter {
        matches: &result.matches,
        files: &result.files,
        total_matched: result.matches.len(),
        cursor: GrepCursor::default(),
        output_mode: OutputMode::Content,
        max_results: 8,
        show_context: context.is_some_and(|(before, after)| before + after > 0),
        auto_expand_defs: context.is_none(),
        picker,
        root: "",
    }
    .format(&mut CursorStore::new());
    format!("{notice}\n{text}\nSuggestions are limited; search a suggested path for more matches.")
}

// The span allows the separators the cleaned fuzzy query dropped (`get_ful_help`).
fn strong_suggestion(m: &fff::GrepMatch, needle_len: usize, query_len: usize) -> bool {
    let Some((start, _)) = m.match_byte_offsets.first() else {
        return false;
    };
    let Some((_, end)) = m.match_byte_offsets.last() else {
        return false;
    };
    let matched: usize = m
        .match_byte_offsets
        .iter()
        .map(|(start, end)| (end - start) as usize)
        .sum();
    let typos = if needle_len < 9 { 1 } else { 2 };
    matched + typos >= needle_len && (*end - *start) as usize <= needle_len.max(query_len) + 2
}
