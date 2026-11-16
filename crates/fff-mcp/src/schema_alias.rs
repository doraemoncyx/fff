use schemars::Schema;
use serde_json::{Map, Value, json};

pub(crate) fn find_files_params_schema(schema: &mut Schema) {
    advertise_alias_keys(schema, "query", &["pattern", "querystr"]);
}

pub(crate) fn grep_params_schema(schema: &mut Schema) {
    advertise_alias_keys(schema, "query", &["pattern", "querystr"]);
}

// The serde aliases are the contract; advertise every key. None of them is
// required, so an omitted identifier means an empty one.
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
}
