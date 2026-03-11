#![forbid(unsafe_code)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use chrono::DateTime;

pub const FIXTURE_PATH: &str = "tests/conformance/fixtures/python_reference.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixtures {
    pub version: String,
    pub generated_at: String,

    #[serde(default)]
    pub tools: IndexMap<String, ToolFixture>,

    #[serde(default)]
    pub resources: IndexMap<String, ResourceFixture>,
}

impl Fixtures {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, FixtureLoadError> {
        let raw = fs::read_to_string(path).map_err(FixtureLoadError::Io)?;
        let fixtures: Self = serde_json::from_str(&raw).map_err(FixtureLoadError::Json)?;
        fixtures.validate()?;
        Ok(fixtures)
    }

    pub fn load_default() -> Result<Self, FixtureLoadError> {
        Self::load(FIXTURE_PATH)
    }

    fn validate(&self) -> Result<(), FixtureLoadError> {
        if self.version.trim().is_empty() {
            return Err(FixtureLoadError::Validation(
                "fixtures.version must be non-empty".to_string(),
            ));
        }
        if self.generated_at.trim().is_empty() {
            return Err(FixtureLoadError::Validation(
                "fixtures.generated_at must be non-empty".to_string(),
            ));
        }
        
        // Ensure generated_at is a valid ISO 8601 timestamp
        if DateTime::parse_from_rfc3339(&self.generated_at).is_err() {
            return Err(FixtureLoadError::Validation(
                format!("fixtures.generated_at must be valid RFC3339 (ISO 8601), got: '{}'", self.generated_at)
            ));
        }

        for (tool, fixture) in &self.tools {
            fixture
                .validate()
                .map_err(|e| FixtureLoadError::Validation(format!("tool {tool}: {e}")))?;
        }
        for (uri, fixture) in &self.resources {
            fixture
                .validate()
                .map_err(|e| FixtureLoadError::Validation(format!("resource {uri}: {e}")))?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFixture {
    #[serde(default)]
    pub cases: Vec<Case>,
}

impl ToolFixture {
    fn validate(&self) -> Result<(), String> {
        for (idx, case) in self.cases.iter().enumerate() {
            case.validate().map_err(|e| format!("case[{idx}]: {e}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceFixture {
    #[serde(default)]
    pub cases: Vec<Case>,
}

impl ResourceFixture {
    fn validate(&self) -> Result<(), String> {
        for (idx, case) in self.cases.iter().enumerate() {
            case.validate().map_err(|e| format!("case[{idx}]: {e}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub name: String,

    /// For tools: argument object.
    /// For resources: query input (if any).
    #[serde(default)]
    pub input: Value,

    pub expect: Expectation,

    #[serde(default)]
    pub normalize: Normalize,
}

impl Case {
    fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("case.name must be non-empty".to_string());
        }
        self.expect.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Normalize {
    /// JSON Pointer paths (RFC 6901) to ignore when comparing.
    #[serde(default)]
    pub ignore_json_pointers: Vec<String>,

    /// Replace exact JSON Pointer targets with constant values before comparing.
    ///
    /// Note: keys are JSON Pointers, values are the replacement JSON value.
    #[serde(default)]
    pub replace: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expectation {
    #[serde(default)]
    pub ok: Option<Value>,

    #[serde(default)]
    pub err: Option<ExpectedError>,
}

impl Expectation {
    fn validate(&self) -> Result<(), String> {
        match (&self.ok, &self.err) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (None, None) => {
                Err("expect must contain exactly one of {ok, err} (found neither)".to_string())
            }
            (Some(_), Some(_)) => {
                Err("expect must contain exactly one of {ok, err} (found both)".to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExpectedError {
    /// A stable error code like `DCG-3001` (optional).
    #[serde(default)]
    pub code: Option<String>,

    /// Stable error_type category, if the server provides it (optional).
    #[serde(default)]
    pub error_type: Option<String>,

    /// Substring the error message must contain (optional; used when full message is unstable).
    #[serde(default)]
    pub message_contains: Option<String>,

    /// Optional structured error data (if present in the error payload).
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug)]
pub enum FixtureLoadError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Validation(String),
}

impl std::fmt::Display for FixtureLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error loading fixtures: {e}"),
            Self::Json(e) => write!(f, "JSON error loading fixtures: {e}"),
            Self::Validation(e) => write!(f, "Fixture validation error: {e}"),
        }
    }
}

impl std::error::Error for FixtureLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Validation(_) => None,
        }
    }
}
