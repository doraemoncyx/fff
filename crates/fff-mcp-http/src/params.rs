use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde::de;

use crate::error::ApiError;

pub const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Fuzzy,
    Glob,
    Regex,
}

/// Request parameters shared by `GET /search` and the `find_files` MCP tool.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct Params {
    /// Fuzzy filename or path query. Empty lists everything in scope.
    #[serde(
        alias = "query",
        alias = "pattern",
        deserialize_with = "deserialize_string"
    )]
    pub querystr: String,
    /// Absolute directory to search. Indexed on demand and kept in an LRU registry.
    pub tardir: String,
    /// Directory, filename, or one glob relative to tardir.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub path: Option<String>,
    /// Multiple tardir-relative paths. An empty string means the whole tardir.
    #[serde(default, deserialize_with = "deserialize_strings")]
    pub paths: Vec<String>,
    /// Include glob patterns (OR). Prefix with ! to exclude.
    #[serde(default, deserialize_with = "deserialize_strings")]
    pub globs: Vec<String>,
    /// Include hidden paths (default true).
    #[serde(default)]
    pub hidden: Option<bool>,
    /// fuzzy (default), glob, or regex.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Additional regex filters on the tardir-relative path (AND).
    #[serde(default, deserialize_with = "deserialize_strings")]
    pub filters: Vec<String>,
    /// Max results per page (default 20).
    #[serde(
        rename = "maxResults",
        alias = "limit",
        default,
        deserialize_with = "deserialize_optional_number"
    )]
    pub max_results: Option<f64>,
    /// Opaque cursor from a previous response.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub cursor: Option<String>,
    /// Raw offset when no cursor is used.
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub offset: Option<f64>,
}

impl Params {
    /// Build params from `application/x-www-form-urlencoded` pairs (GET /search).
    pub fn from_pairs<'a>(
        pairs: impl Iterator<Item = (Cow<'a, str>, Cow<'a, str>)>,
    ) -> Result<Self, ApiError> {
        let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
        for (key, value) in pairs {
            match grouped.iter_mut().find(|(name, _)| *name == key) {
                Some((_, values)) => values.push(value.into_owned()),
                None => grouped.push((key.into_owned(), vec![value.into_owned()])),
            }
        }

        let mut object = serde_json::Map::new();
        for (key, mut values) in grouped {
            let value = match key.as_str() {
                "hidden" => parse_bool(&values[0])?.map(serde_json::Value::Bool),
                "maxResults" | "offset" => {
                    values.first().map(|v| serde_json::Value::String(v.clone()))
                }
                _ if values.len() == 1 => Some(serde_json::Value::String(values.remove(0))),
                _ => Some(serde_json::Value::Array(
                    values
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect::<Vec<_>>(),
                )),
            };
            if let Some(value) = value {
                object.insert(key, value);
            }
        }

        serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|e| ApiError::invalid_params(format!("invalid parameters: {e}")))
    }
}

fn parse_bool(raw: &str) -> Result<Option<bool>, ApiError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        other => Err(ApiError::invalid_params(format!(
            "expected a boolean, got {other}"
        ))),
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PathSpec {
    Prefix(String),
    Glob(String),
}

/// Validated request ready for execution.
pub(crate) struct Query {
    pub raw: String,
    pub tardir: PathBuf,
    pub path: Option<PathSpec>,
    pub paths: Vec<String>,
    pub globs: Vec<String>,
    pub hidden: Option<bool>,
    pub mode: Mode,
    pub filters: Vec<regex::Regex>,
    pub limit: usize,
    pub offset: usize,
}

impl Query {
    pub fn normalize(params: &Params, offset: usize) -> Result<Self, ApiError> {
        let tardir = crate::root::canonical_dir(&params.tardir)?;
        let path = params
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| resolve_path(path, &tardir))
            .transpose()?;
        let paths = params
            .paths
            .iter()
            .map(|path| resolve_relative(path, &tardir))
            .collect::<Result<Vec<_>, _>>()?;
        let filters = params
            .filters
            .iter()
            .map(|pattern| {
                regex::Regex::new(pattern).map_err(|e| {
                    ApiError::invalid_params(format!("Invalid path regex '{pattern}': {e}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            raw: params.querystr.trim().to_string(),
            tardir,
            path,
            paths,
            globs: params.globs.clone(),
            hidden: params.hidden,
            mode: params.mode.unwrap_or_default(),
            filters,
            limit: normalize_limit(params.max_results),
            offset,
        })
    }
}

pub(crate) fn normalize_limit(raw: Option<f64>) -> usize {
    match raw {
        None => DEFAULT_LIMIT,
        Some(value) if !value.is_finite() || value <= 0.0 => DEFAULT_LIMIT,
        Some(value) => (value.round() as usize).clamp(1, MAX_LIMIT),
    }
}

fn resolve_path(raw: &str, tardir: &Path) -> Result<PathSpec, ApiError> {
    let rel = resolve_relative(raw, tardir)?;
    if rel.contains(['*', '?', '[', '{']) {
        Ok(PathSpec::Glob(rel))
    } else {
        Ok(PathSpec::Prefix(rel))
    }
}

/// Normalize a tardir-relative path to `/`-separated text, rejecting escapes.
fn resolve_relative(raw: &str, tardir: &Path) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let given = Path::new(trimmed);
    let relative = if given.is_absolute() {
        strip_tardir(given, tardir)?
    } else {
        given.to_path_buf()
    };
    if relative
        .components()
        .any(|part| part == Component::ParentDir)
    {
        return Err(ApiError::invalid_params(
            "path must not contain '..' components",
        ));
    }
    let text = relative
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    Ok(text
        .strip_prefix("./")
        .unwrap_or(&text)
        .trim_end_matches('/')
        .to_string())
}

fn strip_tardir(path: &Path, tardir: &Path) -> Result<PathBuf, ApiError> {
    if let Ok(relative) = path.strip_prefix(tardir) {
        return Ok(relative.to_path_buf());
    }
    // Symlinked or short-name spellings still name the tardir; keep the missing tail.
    let (existing, real) = path
        .ancestors()
        .find_map(|dir| Some((dir, std::fs::canonicalize(dir).ok()?)))
        .ok_or_else(|| ApiError::invalid_params("path must be inside tardir"))?;
    let base = std::fs::canonicalize(tardir).unwrap_or_else(|_| tardir.to_path_buf());
    real.strip_prefix(&base)
        .ok()
        .zip(path.strip_prefix(existing).ok())
        .map(|(inside, missing)| inside.join(missing))
        .ok_or_else(|| ApiError::invalid_params("path must be inside tardir"))
}

fn deserialize_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringsVisitor;

    impl<'de> de::Visitor<'de> for StringsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, an array of strings, or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            if value.trim().is_empty() {
                return Ok(Vec::new());
            }
            if value.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(value)
            {
                return Ok(parsed);
            }
            Ok(vec![value.to_string()])
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(StringsVisitor)
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalStringVisitor;

    impl<'de> de::Visitor<'de> for OptionalStringVisitor {
        type Value = Option<String>;

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
            if value.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(value)
            {
                return Ok(Some(parsed.join(" ")));
            }
            Ok(Some(value.to_string()))
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
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

fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(deserialize_optional_string(deserializer)?.unwrap_or_default())
}

fn deserialize_optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
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

        fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
            Ok(Some(value))
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Some(value as f64))
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Some(value as f64))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(value.trim().parse().ok())
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_clamped_and_defaulted() {
        assert_eq!(normalize_limit(None), DEFAULT_LIMIT);
        assert_eq!(normalize_limit(Some(0.0)), DEFAULT_LIMIT);
        assert_eq!(normalize_limit(Some(f64::NAN)), DEFAULT_LIMIT);
        assert_eq!(normalize_limit(Some(5.4)), 5);
        assert_eq!(normalize_limit(Some(1e9)), MAX_LIMIT);
    }

    #[test]
    fn querystr_aliases_and_arrays_are_accepted() {
        let from_alias: Params = serde_json::from_str(r#"{"query":"a","tardir":"C:/x"}"#).unwrap();
        assert_eq!(from_alias.querystr, "a");
        let from_array: Params =
            serde_json::from_str(r#"{"querystr":["a","b"],"tardir":"C:/x"}"#).unwrap();
        assert_eq!(from_array.querystr, "a b");
        let from_null: Params =
            serde_json::from_str(r#"{"querystr":null,"tardir":"C:/x"}"#).unwrap();
        assert_eq!(from_null.querystr, "");
    }

    #[test]
    fn number_fields_tolerate_floats_and_strings() {
        let params: Params = serde_json::from_str(
            r#"{"querystr":"x","tardir":"C:/x","maxResults":7.9,"offset":"3"}"#,
        )
        .unwrap();
        assert_eq!(params.max_results, Some(7.9));
        assert_eq!(params.offset, Some(3.0));
    }

    #[test]
    fn get_pairs_accept_repeated_lists() {
        let pairs = [
            ("querystr", "a"),
            ("tardir", "C:/x"),
            ("globs", "*.rs"),
            ("globs", "!test"),
            ("paths", "src"),
        ];
        let params = Params::from_pairs(
            pairs
                .into_iter()
                .map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))),
        )
        .unwrap();
        assert_eq!(params.globs, vec!["*.rs".to_string(), "!test".to_string()]);
        assert_eq!(params.paths, vec!["src".to_string()]);
    }

    #[test]
    fn relative_paths_reject_parent_dir() {
        let dir = std::env::temp_dir();
        assert!(resolve_relative("../escape", &dir).is_err());
        assert_eq!(resolve_relative("./src/", &dir).unwrap(), "src");
    }
}
