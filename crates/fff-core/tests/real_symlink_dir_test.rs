//! Regression test for the real user scenario: a base dir containing symlinks
//! to directories, where the file INSIDE the symlinked dir must be searchable.
//! Uses the real path g:\tmp2_sprsize_code when present (skips otherwise).

use std::path::Path;

use fff_search::file_picker::{FilePicker, FuzzySearchOptions};
use fff_search::grep::{GrepMode, GrepSearchOptions};
use fff_search::{FilePickerOptions, GrepConfig, PaginationArgs, QueryParser};

#[test]
fn real_symlink_dir_grep() {
    let base = Path::new(r"g:\tmp2_sprsize_code");
    if !base.is_dir() {
        eprintln!("skipping: {} not present", base.display());
        return;
    }

    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: base.to_str().unwrap().into(),
        watch: false,
        follow_symlinks: false,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();

    let parser = QueryParser::new(GrepConfig);
    let query = parser.parse("_parse_args");
    let result = picker.grep(
        &query,
        &GrepSearchOptions {
            max_file_size: 10 * 1024 * 1024,
            max_matches_per_file: 200,
            smart_case: true,
            file_offset: 0,
            page_limit: 50,
            mode: GrepMode::PlainText,
            time_budget_ms: 0,
            enforce_time_budget: false,
            before_context: 0,
            after_context: 0,
            classify_definitions: false,
            trim_whitespace: false,
            abort_signal: None,
        },
    );

    let found: Vec<String> = result
        .files
        .iter()
        .map(|f| f.relative_path(&picker))
        .collect();
    assert!(
        found.iter().any(|f| f == "python3/main.py"),
        "grep for _parse_args should hit python3/main.py, got {found:?}"
    );
}

#[test]
fn repro_def_test_json() {
    let base = Path::new(r"g:\tmp2_sprsize_code");
    if !base.is_dir() {
        eprintln!("skipping: {} not present", base.display());
        return;
    }

    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: base.to_str().unwrap().into(),
        watch: false,
        follow_symlinks: false,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();

    let rels: Vec<String> = picker
        .get_files()
        .iter()
        .map(|f| f.relative_path(&picker))
        .filter(|f| f.contains("client/fashion"))
        .collect();
    eprintln!("=== client/fashion files ({})", rels.len());
    for r in &rels {
        eprintln!("  {r}");
    }

    let parser = QueryParser::new(GrepConfig);
    let query = parser.parse("def test_json");
    let result = picker.grep(
        &query,
        &GrepSearchOptions {
            max_file_size: 10 * 1024 * 1024,
            max_matches_per_file: 200,
            smart_case: true,
            file_offset: 0,
            page_limit: 50,
            mode: GrepMode::PlainText,
            time_budget_ms: 0,
            enforce_time_budget: false,
            before_context: 0,
            after_context: 0,
            classify_definitions: false,
            trim_whitespace: false,
            abort_signal: None,
        },
    );
    let found: Vec<String> = result
        .files
        .iter()
        .map(|f| f.relative_path(&picker))
        .collect();
    eprintln!("=== grep 'def test_json' found {} files", found.len());
    for f in &found {
        eprintln!("  {f}");
    }
    assert!(
        found.iter().any(|f| f == "python3/client/fashion/showroom_manager.py"),
        "grep for def test_json should hit showroom_manager.py, got {found:?}"
    );
}

#[test]
fn real_symlink_dir_fuzzy() {
    let base = Path::new(r"g:\tmp2_sprsize_code");
    if !base.is_dir() {
        eprintln!("skipping: {} not present", base.display());
        return;
    }

    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: base.to_str().unwrap().into(),
        watch: false,
        follow_symlinks: false,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();

    let parser = QueryParser::new(fff_search::FileSearchConfig);
    let query = parser.parse("main.py");
    let result = picker.fuzzy_search(
        &query,
        None,
        FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: 100,
            },
            ..Default::default()
        },
    );
    let found: Vec<String> = result
        .items
        .iter()
        .map(|f| f.relative_path(&picker))
        .collect();
    assert!(
        found.iter().any(|f| f == "python3/main.py"),
        "fuzzy main.py should hit python3/main.py, got {found:?}"
    );
}
