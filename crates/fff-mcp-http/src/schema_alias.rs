use schemars::Schema;
use serde_json::{Map, Value, json};

// Tools that read the identifier from `query`.
pub(crate) fn grep_params_schema(schema: &mut Schema) {
    advertise_alias_keys(schema, "query", &["pattern", "querystr"]);
}

// `find_files` keeps `querystr` primary because GET /search spells it that way.
pub(crate) fn find_files_params_schema(schema: &mut Schema) {
    advertise_alias_keys(schema, "querystr", &["query", "pattern"]);
}

// The serde aliases are the contract; advertise them, and require any one of them.
fn advertise_alias_keys(schema: &mut Schema, primary: &str, aliases: &[&str]) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let properties = object
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(properties) = properties.as_object_mut() {
        for alias in aliases {
            properties.entry(*alias).or_insert_with(|| {
                json!({
                    "type": "string",
                    "description": format!("Alias of {primary}; prefer {primary}."),
                })
            });
        }
    }
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|key| key.as_str() != Some(primary));
    }
    object.insert(
        "anyOf".to_string(),
        Value::Array(
            std::iter::once(primary)
                .chain(aliases.iter().copied())
                .map(|key| json!({ "required": [key] }))
                .collect(),
        ),
    );
}
