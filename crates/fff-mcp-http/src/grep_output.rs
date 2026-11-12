use fff::FileItem;
use fff::FilePicker;
use fff::GrepMatch;

use crate::cursor::{CursorStore, GrepCursor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
    Usage,
}

impl OutputMode {
    pub fn new(s: Option<&str>) -> Self {
        match s {
            Some("files_with_matches") => Self::FilesWithMatches,
            Some("count") => Self::Count,
            Some("usage") => Self::Usage,
            _ => Self::Content,
        }
    }
}

const LARGE_FILE_BYTES: u64 = 20_000;
const MAX_PREVIEW: usize = 120;
const MAX_LINE_LEN: usize = 180;
const MAX_DEF_EXPAND_FIRST: usize = 16;
const MAX_DEF_EXPAND: usize = 5;
const MAX_FIRST_MATCH_EXPAND: usize = 8;
const AUTO_CONTEXT_CHARS: usize = 5000;
const EXPLICIT_CONTEXT_CHARS: usize = 8000;

fn size_tag(bytes: u64) -> String {
    if bytes < LARGE_FILE_BYTES {
        String::new()
    } else {
        let kb = (bytes + 512) / 1024;
        format!(" ({}KB - use offset to read relevant section)", kb)
    }
}

fn truncate_line_for_ai(line: &str, match_ranges: Option<&[(u32, u32)]>, max_len: usize) -> String {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }

    // Use first match range to center the window
    if let Some(ranges) = match_ranges
        && let Some(&(match_start, match_end)) = ranges.first()
    {
        let match_start = match_start as usize;
        let match_end = match_end as usize;
        let match_len = match_end.saturating_sub(match_start);

        let budget = max_len.saturating_sub(match_len);
        let before = budget / 3;
        let after = budget - before;

        let win_start = match_start.saturating_sub(before);
        let win_end = (match_end + after).min(trimmed.len());

        let win_start = floor_char_boundary(trimmed, win_start);
        let win_end = ceil_char_boundary(trimmed, win_end);

        let mut result = trimmed[win_start..win_end].to_string();
        if win_start > 0 {
            result.insert(0, '…');
        }
        if win_end < trimmed.len() {
            result.push('…');
        }
        return result;
    }

    let end = ceil_char_boundary(trimmed, max_len);
    format!("{}…", &trimmed[..end])
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// Import lines sort after usages because they carry no evidence of behaviour.
fn is_import_line(line: &str) -> bool {
    let s = line.trim_start().as_bytes();
    s.starts_with(b"import ")
        || s.starts_with(b"import\t")
        || (s.starts_with(b"from ") && s.get(5).is_some_and(|&b| b == b'\'' || b == b'"'))
        || s.starts_with(b"use ")
        || s.starts_with(b"use\t")
        || starts_with_require(s)
        || starts_with_include(s)
}

fn starts_with_require(s: &[u8]) -> bool {
    if !s.starts_with(b"require") {
        return false;
    }
    let rest = &s[b"require".len()..];
    rest.first() == Some(&b'(') || (rest.first() == Some(&b' ') && rest.get(1) == Some(&b'('))
}

// `# include ` with optional spaces after the `#`.
fn starts_with_include(s: &[u8]) -> bool {
    if s.first() != Some(&b'#') {
        return false;
    }
    let rest = s[1..].trim_ascii_start();
    rest.starts_with(b"include ") || rest.starts_with(b"include\t")
}

struct FileMeta<'a> {
    file: &'a FileItem,
    line_number: u64,
    line_content: String,
    is_definition: bool,
    match_ranges: Vec<(u32, u32)>,
    context_after: Vec<String>,
}

pub(crate) struct GrepFormatter<'a> {
    pub matches: &'a [GrepMatch],
    pub files: &'a [&'a FileItem],
    pub total_matched: usize,
    pub cursor: GrepCursor,
    pub output_mode: OutputMode,
    pub max_results: usize,
    pub show_context: bool,
    pub auto_expand_defs: bool,
    pub picker: &'a FilePicker,
    pub root: &'a str,
}

impl GrepFormatter<'_> {
    pub fn format(&self, cursor_store: &mut CursorStore) -> String {
        let GrepFormatter {
            matches,
            files,
            total_matched,
            cursor,
            output_mode,
            max_results,
            show_context,
            auto_expand_defs,
            picker,
            root,
        } = *self;

        let items = if matches.len() > max_results {
            &matches[..max_results]
        } else {
            matches
        };

        if output_mode == OutputMode::FilesWithMatches {
            return format_files_with_matches(
                items,
                files,
                cursor,
                auto_expand_defs,
                cursor_store,
                picker,
                root,
            );
        }

        if output_mode == OutputMode::Count {
            return format_count(items, files, cursor, cursor_store, picker, root);
        }
        if show_context
            && let Some(text) =
                self.format_with_context(items, cursor_store, EXPLICIT_CONTEXT_CHARS, false)
        {
            return text;
        }
        if auto_expand_defs
            && items.len() <= 8
            && !items.iter().any(|m| m.is_definition)
            && let Some(text) =
                self.format_with_context(items, cursor_store, AUTO_CONTEXT_CHARS, true)
        {
            return text;
        }

        let mut lines: Vec<String> = Vec::new();
        let unique_files = {
            let mut seen = std::collections::HashSet::new();
            for m in items {
                seen.insert(m.file_index);
            }
            seen.len()
        };

        let max_output_chars: usize = if output_mode == OutputMode::Usage || unique_files <= 3 {
            5000
        } else if unique_files <= 8 {
            3500
        } else {
            2500
        };

        if total_matched > items.len() {
            lines.push(format!("{}/{} matches shown", items.len(), total_matched));
        }

        let mut def_expanded_files = std::collections::HashSet::new();
        let mut char_count = 0usize;
        let mut current_file = String::new();

        // Definitions first, each file's exact declaration leading its body; then usages, then imports.
        let sorted_items: Vec<usize> = if auto_expand_defs {
            let mut indices: Vec<usize> = (0..items.len()).collect();
            indices.sort_by_cached_key(|&i| {
                let m = &items[i];
                if m.is_definition {
                    (
                        0,
                        m.file_index,
                        !declares_whole_word(m, files[m.file_index]),
                    )
                } else if is_import_line(&m.line_content) {
                    (2, 0, false)
                } else {
                    (1, 0, false)
                }
            });

            indices
        } else {
            (0..items.len()).collect()
        };

        let mut compacted = 0usize;
        let mut body_lines = std::collections::HashSet::new();
        let matching_lines: std::collections::HashMap<_, _> = if auto_expand_defs {
            items
                .iter()
                .map(|m| ((m.file_index, m.line_number), m.match_byte_offsets.as_ref()))
                .collect()
        } else {
            std::collections::HashMap::new()
        };
        for (shown_count, &idx) in sorted_items.iter().enumerate() {
            let m = &items[idx];
            // Already printed inside an expanded definition body.
            if body_lines.contains(&(m.file_index, m.line_number)) {
                continue;
            }
            let file = files[m.file_index];
            let mut match_lines: Vec<String> = Vec::new();
            let over_budget = shown_count > 0 && char_count >= max_output_chars;

            let file_rel_path = file.relative_path(picker);
            if file_rel_path != current_file {
                current_file = file_rel_path;
                match_lines.push(current_file.to_string());
            }

            match_lines.push(format!(
                " {}: {}",
                m.line_number,
                truncate_line_for_ai(
                    &m.line_content,
                    Some(m.match_byte_offsets.as_ref()),
                    MAX_LINE_LEN
                )
            ));

            let file_rel_for_expand = file.relative_path(picker);
            let expands = auto_expand_defs
                && m.is_definition
                && !m.context_after.is_empty()
                && !def_expanded_files.contains(&file_rel_for_expand);
            if expands && over_budget {
                compacted += 1;
            } else if expands {
                let expand_limit = if def_expanded_files.is_empty() {
                    MAX_DEF_EXPAND_FIRST
                } else {
                    MAX_DEF_EXPAND
                };
                def_expanded_files.insert(file_rel_for_expand);
                let start_line = m.line_number + 1;
                for (i, ctx) in m.context_after.iter().take(expand_limit).enumerate() {
                    if ctx.trim().is_empty() {
                        continue;
                    }
                    body_lines.insert((m.file_index, start_line + i as u64));
                    let ranges = matching_lines
                        .get(&(m.file_index, start_line + i as u64))
                        .copied();
                    match_lines.push(format!(
                        "  {}| {}",
                        start_line + i as u64,
                        truncate_line_for_ai(ctx, ranges, MAX_LINE_LEN)
                    ));
                }
            }

            let chunk = match_lines.join("\n");
            char_count += chunk.len();
            lines.push(chunk);
        }
        if compacted > 0 {
            let note =
                format!("Definition bodies omitted for {compacted} matches to fit the output.");
            lines.insert(usize::from(total_matched > items.len()), note);
        }

        if cursor.has_more() {
            let cursor_id = cursor_store.store_grep(root, cursor);
            lines.push(format!("\ncursor: {}", cursor_id));
        }

        lines.join("\n")
    }

    fn format_with_context(
        &self,
        items: &[GrepMatch],
        cursor_store: &mut CursorStore,
        max_chars: usize,
        all_or_nothing: bool,
    ) -> Option<String> {
        let mut rows = std::collections::BTreeMap::new();
        let mut chars = 0;
        let mut with_context = 0;
        for m in items {
            let line =
                truncate_line_for_ai(&m.line_content, Some(&m.match_byte_offsets), MAX_LINE_LEN);
            // Explicit context budgets only context rows; matching lines are always shown.
            if all_or_nothing {
                chars += line.len() + 8;
            }
            rows.insert((m.file_index, m.line_number), (line, true));
        }
        for m in items {
            let start = m.line_number.saturating_sub(m.context_before.len() as u64);
            let context = m
                .context_before
                .iter()
                .enumerate()
                .map(|(i, text)| (start + i as u64, text))
                .chain(
                    m.context_after
                        .iter()
                        .enumerate()
                        .map(|(i, text)| (m.line_number + 1 + i as u64, text)),
                )
                .filter(|(line, _)| !rows.contains_key(&(m.file_index, *line)))
                .map(|(line, text)| (line, truncate_line_for_ai(text, None, MAX_LINE_LEN)))
                .collect::<Vec<_>>();
            let cost: usize = context.iter().map(|(_, text)| text.len() + 8).sum();
            if chars + cost > max_chars {
                if all_or_nothing {
                    return None;
                }
                break;
            }
            chars += cost;
            with_context += 1;
            for (line, text) in context {
                rows.insert((m.file_index, line), (text, false));
            }
        }
        let mut lines = Vec::new();
        if with_context < items.len() {
            lines.push(format!(
                "Context shown for {with_context}/{} matches to fit the output.",
                items.len()
            ));
        }
        let mut previous = None;
        for ((file_index, line), (text, matched)) in rows {
            if previous.is_none_or(|(file, _)| file != file_index) {
                lines.push(self.files[file_index].relative_path(self.picker));
            } else if previous.is_some_and(|(_, last)| line > last + 1) {
                lines.push("--".into());
            }
            lines.push(format!(" {line}{} {text}", if matched { ':' } else { '-' }));
            previous = Some((file_index, line));
        }
        if self.cursor.has_more() {
            let id = cursor_store.store_grep(self.root, self.cursor);
            lines.push(format!("\ncursor: {id}"));
        }
        Some(lines.join("\n"))
    }
}

fn format_files_with_matches(
    items: &[GrepMatch],
    files: &[&FileItem],
    cursor: GrepCursor,
    auto_expand_defs: bool,
    cursor_store: &mut CursorStore,
    picker: &FilePicker,
    root: &str,
) -> String {
    let file_map = collect_file_preview(items, files, picker);

    let mut lines: Vec<String> = Vec::new();
    let file_count = file_map.len();

    let is_small_set = file_count <= 5;
    let mut def_expanded_count = 0usize;

    for (file_idx, fm) in file_map.iter().enumerate() {
        let is_def = fm.is_definition;
        let def_tag = if is_def { " [def]" } else { "" };
        lines.push(format!(
            "{}{}{}",
            fm.file.relative_path(picker),
            def_tag,
            size_tag(fm.file.size)
        ));

        if !fm.line_content.is_empty() && (is_def || file_idx == 0 || is_small_set) {
            let ranges_ref: Option<&[(u32, u32)]> = if fm.match_ranges.is_empty() {
                None
            } else {
                Some(&fm.match_ranges)
            };
            lines.push(format!(
                "  {}: {}",
                fm.line_number,
                truncate_line_for_ai(&fm.line_content, ranges_ref, MAX_PREVIEW)
            ));

            if auto_expand_defs && !fm.context_after.is_empty() {
                let expand_limit = if is_def {
                    let limit = if def_expanded_count == 0 {
                        MAX_DEF_EXPAND_FIRST
                    } else {
                        MAX_DEF_EXPAND
                    };
                    def_expanded_count += 1;
                    limit
                } else if is_small_set && file_idx == 0 {
                    MAX_FIRST_MATCH_EXPAND
                } else if is_small_set {
                    MAX_DEF_EXPAND
                } else {
                    0
                };

                if expand_limit > 0 {
                    let start_line = fm.line_number + 1;
                    for (i, ctx) in fm.context_after.iter().take(expand_limit).enumerate() {
                        if ctx.trim().is_empty() {
                            continue;
                        }
                        lines.push(format!(
                            "  {}| {}",
                            start_line + i as u64,
                            truncate_line_for_ai(ctx, None, MAX_PREVIEW)
                        ));
                    }
                }
            }
        }
    }

    if cursor.has_more() {
        let cursor_id = cursor_store.store_grep(root, cursor);
        lines.push(format!("\ncursor: {}", cursor_id));
    }

    lines.join("\n")
}

fn format_count(
    items: &[GrepMatch],
    files: &[&FileItem],
    cursor: GrepCursor,
    cursor_store: &mut CursorStore,
    picker: &FilePicker,
    root: &str,
) -> String {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for m in items {
        let file = files[m.file_index];
        let path = file.relative_path(picker);
        let count = counts.entry(path.to_string()).or_insert_with(|| {
            order.push(path.to_string());
            0
        });
        *count += 1;
    }

    let mut lines: Vec<String> = Vec::new();
    for path in &order {
        lines.push(format!("{}: {}", path, counts[path.as_str()]));
    }
    if cursor.has_more() {
        let cursor_id = cursor_store.store_grep(root, cursor);
        lines.push(format!("\ncursor: {}", cursor_id));
    }
    lines.join("\n")
}

fn collect_file_preview<'a>(
    items: &[GrepMatch],
    files: &[&'a FileItem],
    picker: &FilePicker,
) -> Vec<FileMeta<'a>> {
    let mut file_preview: Vec<FileMeta<'a>> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in items {
        let file = files[m.file_index];
        if seen.insert(file.relative_path(picker)) {
            file_preview.push(FileMeta {
                file,
                line_number: m.line_number,
                line_content: m.line_content.clone(),
                is_definition: m.is_definition,
                match_ranges: m.match_byte_offsets.iter().copied().collect(),
                context_after: m.context_after.clone(),
            });
        }
    }
    file_preview
}

// Files whose declaration is named by a whole matched word move ahead; the cursor is already set.
pub(crate) fn declarations_first(result: &mut fff::GrepResult<'_>) {
    let mut exact = vec![false; result.files.len()];
    for m in &result.matches {
        exact[m.file_index] |=
            m.is_definition && declares_whole_word(m, result.files[m.file_index]);
    }
    if !exact.contains(&true) {
        return;
    }
    let mut order: Vec<usize> = (0..result.files.len()).collect();
    order.sort_by_key(|&i| !exact[i]);
    let mut position = vec![0; order.len()];
    for (new, &old) in order.iter().enumerate() {
        position[old] = new;
    }
    result.files = order.iter().map(|&old| result.files[old]).collect();
    for m in &mut result.matches {
        m.file_index = position[m.file_index];
    }
    result.matches.sort_by_key(|m| !exact[order[m.file_index]]);
}

// The classifier confirms that the whole-word range is the declared name, not a parameter type.
fn declares_whole_word(m: &GrepMatch, file: &FileItem) -> bool {
    let line = m.line_content.as_bytes();
    let word = |i: usize| {
        line.get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    };
    m.match_byte_offsets.iter().any(|&(start, end)| {
        let (s, e) = (start as usize, end as usize);
        s < e
            && (s == 0 || !word(s - 1))
            && !word(e)
            && file.is_definition_match(&m.line_content, &[(start, end)])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_strips_trailing_whitespace() {
        assert_eq!(truncate_line_for_ai("foo()", None, 180), "foo()");
        assert_eq!(truncate_line_for_ai("bar  ", None, 180), "bar");
        assert_eq!(truncate_line_for_ai("   ", None, 180), "");
    }

    #[test]
    fn truncation_centers_on_the_first_match() {
        let line = format!("match_here{}", "x".repeat(200));
        let result = truncate_line_for_ai(&line, Some(&[(0, 10)]), 50);
        assert!(result.contains("match_here"));
        assert!(result.len() <= 55);
    }

    #[test]
    fn import_lines_are_recognised() {
        assert!(is_import_line("  use std::io;"));
        assert!(is_import_line("import os"));
        assert!(is_import_line("#include <stdio.h>"));
        assert!(!is_import_line("let include = 1;"));
        assert!(!is_import_line("fn main() {}"));
    }
}
