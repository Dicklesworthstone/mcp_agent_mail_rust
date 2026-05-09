#![allow(clippy::panic_in_result_fn)]

use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const BASE_TOP_LEVEL: [&str; 5] = ["ts", "level", "target", "name", "fields"];
const BASE_FIELDS: [&str; 6] = [
    "repo_slug",
    "caller",
    "args_hash",
    "duration_ms",
    "outcome",
    "git_version",
];

fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("crate manifest is not under repo root")?;
    Ok(root.to_path_buf())
}

fn read_json(path: &Path) -> Result<Value, Box<dyn Error>> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn schema_paths() -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let schemas_dir = repo_root()?.join("docs/schemas/git_251");
    let mut paths = fs::read_dir(schemas_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".schema.json"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn fixture_dir(schema_path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let file_name = schema_path
        .file_name()
        .ok_or("schema path has no file name")?
        .to_string_lossy();
    let event_dir = file_name
        .strip_suffix(".schema.json")
        .ok_or("schema file does not end with .schema.json")?;
    Ok(repo_root()?
        .join("tests/fixtures/observability")
        .join(event_dir))
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{label} must be a JSON object"))
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn ensure_required_keys(
    actual: &Map<String, Value>,
    required: &[String],
    label: &str,
) -> Result<(), String> {
    for key in required {
        if !actual.contains_key(key) {
            return Err(format!("{label} missing required key {key}"));
        }
    }
    Ok(())
}

fn ensure_no_unknown_keys(
    actual: &Map<String, Value>,
    allowed: &Map<String, Value>,
    label: &str,
) -> Result<(), String> {
    for key in actual.keys() {
        if !allowed.contains_key(key) {
            return Err(format!("{label} contains unknown key {key}"));
        }
    }
    Ok(())
}

fn validate_type(name: &str, value: &Value, property: &Value) -> Result<(), String> {
    if let Some(expected) = property.get("const")
        && value != expected
    {
        return Err(format!("{name} does not match const {expected}"));
    }
    if let Some(allowed) = property.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{name} is not in allowed enum"));
    }

    match property.get("type").and_then(Value::as_str) {
        Some("string") if !value.is_string() => Err(format!("{name} must be a string")),
        Some("number") if !value.is_number() => Err(format!("{name} must be a number")),
        Some("integer") if value.as_u64().is_none() && value.as_i64().is_none() => {
            Err(format!("{name} must be an integer"))
        }
        Some("boolean") if !value.is_boolean() => Err(format!("{name} must be a boolean")),
        _ => {
            if property
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(|pattern| pattern == "^[a-f0-9]{64}$")
            {
                let Some(text) = value.as_str() else {
                    return Err(format!("{name} pattern field must be a string"));
                };
                if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(format!("{name} does not match sha256 hex pattern"));
                }
            }
            Ok(())
        }
    }
}

fn validate_event(schema: &Value, event: &Value) -> Result<(), String> {
    let schema_object = object(schema, "schema")?;
    let event_object = object(event, "event")?;
    let properties = object(
        schema_object
            .get("properties")
            .ok_or("schema missing properties")?,
        "schema.properties",
    )?;
    let top_required = string_array(schema_object.get("required"));
    ensure_required_keys(event_object, &top_required, "event")?;
    ensure_no_unknown_keys(event_object, properties, "event")?;

    for (key, value) in event_object {
        if key == "fields" {
            continue;
        }
        let property = properties
            .get(key)
            .ok_or_else(|| format!("schema missing top-level property {key}"))?;
        validate_type(key, value, property)?;
    }

    let event_name = event_object
        .get("name")
        .and_then(Value::as_str)
        .ok_or("event.name must be a string")?;
    let schema_events = string_array(schema_object.get("x-event-names"));
    if !schema_events.is_empty() && !schema_events.iter().any(|name| name == event_name) {
        return Err(format!("{event_name} is not listed in x-event-names"));
    }

    let fields_schema = properties
        .get("fields")
        .ok_or("schema missing fields property")?;
    let fields_schema_object = object(fields_schema, "schema.fields")?;
    let field_properties = object(
        fields_schema_object
            .get("properties")
            .ok_or("schema.fields missing properties")?,
        "schema.fields.properties",
    )?;
    let event_fields = object(
        event_object.get("fields").ok_or("event missing fields")?,
        "event.fields",
    )?;
    let required_fields = string_array(fields_schema_object.get("required"));
    ensure_required_keys(event_fields, &required_fields, "event.fields")?;
    ensure_no_unknown_keys(event_fields, field_properties, "event.fields")?;

    for (key, value) in event_fields {
        let property = field_properties
            .get(key)
            .ok_or_else(|| format!("schema missing field property {key}"))?;
        validate_type(key, value, property)?;
    }
    Ok(())
}

#[test]
fn schemas_are_draft_2020_12_strict_and_complete() -> Result<(), Box<dyn Error>> {
    let paths = schema_paths()?;
    assert_eq!(paths.len(), 10, "expected exactly ten git_251 schemas");

    for path in paths {
        let schema = read_json(&path)?;
        let schema_object = object(&schema, "schema")?;
        assert_eq!(
            schema_object.get("$schema").and_then(Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema")
        );
        assert_eq!(
            schema_object.get("type").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            schema_object
                .get("additionalProperties")
                .and_then(Value::as_bool),
            Some(false)
        );

        let required: BTreeSet<_> = string_array(schema_object.get("required"))
            .into_iter()
            .collect();
        for key in BASE_TOP_LEVEL {
            assert!(
                required.contains(key),
                "{} missing top-level {key}",
                path.display()
            );
        }

        let fields_required: BTreeSet<_> = schema_object["properties"]["fields"]["required"]
            .as_array()
            .ok_or("schema fields.required must be an array")?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for key in BASE_FIELDS {
            assert!(
                fields_required.contains(key),
                "{} missing field {key}",
                path.display()
            );
        }
        assert_eq!(
            schema_object["properties"]["fields"]["additionalProperties"].as_bool(),
            Some(false),
            "{} fields must reject additional properties",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn schemas_validate_positive_and_negative_samples() -> Result<(), Box<dyn Error>> {
    for schema_path in schema_paths()? {
        let schema = read_json(&schema_path)?;
        let dir = fixture_dir(&schema_path)?;
        let mut valid_count = 0usize;
        let mut invalid_count = 0usize;

        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let Some(name) = path
                .file_name()
                .map(|file_name| file_name.to_string_lossy())
            else {
                continue;
            };
            let event = read_json(&path)?;
            if name.starts_with("valid_") {
                valid_count += 1;
                validate_event(&schema, &event)
                    .map_err(|error| format!("{} should validate: {error}", path.display()))?;
            } else if name.starts_with("invalid_") {
                invalid_count += 1;
                assert!(
                    validate_event(&schema, &event).is_err(),
                    "{} should be rejected",
                    path.display()
                );
            }
        }

        assert!(
            valid_count > 0,
            "{} has no positive fixture",
            schema_path.display()
        );
        assert!(
            invalid_count > 0,
            "{} has no negative fixture",
            schema_path.display()
        );
    }
    Ok(())
}
