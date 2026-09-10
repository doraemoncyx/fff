//! Live grep. `grep.rs` implements the main plain-text path, the parallel
//! scan engine, and the `grep_search` entry point that picks the matcher/sink
//! for every mode in one place; `regex`, `multi_pattern`, and `fuzzy_grep`
//! hold the mode-specific machinery on top of the shared `prefilter`/`sink`.

#[allow(clippy::module_inception)]
mod grep;
pub use grep::*;

mod fuzzy_grep;
mod multi_pattern;
mod prefilter;
mod regex;
mod sink;
mod types;

#[cfg(feature = "definitions")]
mod classify;
#[cfg(feature = "definitions")]
pub use classify::*;

pub(crate) use multi_pattern::multi_grep_search;
pub use regex::has_regex_metacharacters;
pub use types::*;

// Try to decode bytes as GBK when UTF-8 validation fails. Returns None on the
// valid-UTF-8 fast path (no allocation) or when GBK decode reports errors.
pub(super) fn try_decode_gbk(bytes: &[u8]) -> Option<String> {
    if std::str::from_utf8(bytes).is_ok() {
        return None;
    }
    let (decoded, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if !had_errors {
        Some(decoded.into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod grep_tests;
