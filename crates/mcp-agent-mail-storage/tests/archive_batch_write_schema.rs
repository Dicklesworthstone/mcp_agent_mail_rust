use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("crate manifest is not under repo root")?;
    Ok(root.to_path_buf())
}

fn read_json(path: impl AsRef<Path>) -> Result<Value, Box<dyn Error>> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn schema() -> Result<Value, Box<dyn Error>> {
    read_json(repo_root()?.join("docs/schemas/git_251/archive_batch_write.schema.json"))
}

fn field_required_set(schema: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    Ok(schema["properties"]["fields"]["required"]
        .as_array()
        .ok_or("fields.required must be an array")?
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect())
}

fn event_names(schema: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    Ok(schema["x-event-names"]
        .as_array()
        .ok_or("x-event-names must be an array")?
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect())
}

#[test]
fn archive_batch_write_schema_lists_all_phase_events() -> Result<(), Box<dyn Error>> {
    let schema = schema()?;
    let names = event_names(&schema)?;
    assert_eq!(
        names,
        BTreeSet::from([
            "complete".to_string(),
            "disk_phase".to_string(),
            "git_phase".to_string(),
            "sqlite_phase".to_string(),
            "start".to_string(),
        ])
    );
    assert_eq!(
        schema["properties"]["target"]["const"].as_str(),
        Some("mcp_agent_mail::storage::archive::batch_write")
    );
    Ok(())
}

#[test]
fn archive_batch_write_schema_requires_e6_baseline_fields() -> Result<(), Box<dyn Error>> {
    let schema = schema()?;
    let required = field_required_set(&schema)?;
    for field in [
        "repo_slug",
        "caller",
        "args_hash",
        "duration_ms",
        "outcome",
        "git_version",
    ] {
        assert!(required.contains(field), "missing required field {field}");
    }
    Ok(())
}

#[test]
fn archive_batch_write_fixtures_cover_valid_and_invalid_outcomes() -> Result<(), Box<dyn Error>> {
    let root = repo_root()?.join("tests/fixtures/observability/archive_batch_write");
    let valid = read_json(root.join("valid_1.json"))?;
    let invalid = read_json(root.join("invalid_1.json"))?;

    assert_eq!(valid["name"].as_str(), Some("complete"));
    assert_eq!(valid["fields"]["outcome"].as_str(), Some("success"));
    assert_eq!(valid["fields"]["success"].as_bool(), Some(true));
    assert_eq!(invalid["fields"]["outcome"].as_str(), Some("skipped"));
    Ok(())
}
