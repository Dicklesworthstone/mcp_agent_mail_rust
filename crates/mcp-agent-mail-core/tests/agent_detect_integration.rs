#![forbid(unsafe_code)]

#[cfg(not(feature = "agent-detect"))]
mod feature_disabled {
    use mcp_agent_mail_core::{AgentDetectError, AgentDetectOptions, detect_installed_agents};

    #[test]
    fn detect_installed_agents_feature_disabled_is_explicit() {
        let err =
            detect_installed_agents(&AgentDetectOptions::default()).expect_err("expected error");
        assert!(matches!(err, AgentDetectError::FeatureDisabled));
    }
}

#[cfg(feature = "agent-detect")]
mod feature_enabled {
    use mcp_agent_mail_core::{
        AgentDetectOptions, AgentDetectRootOverride, detect_installed_agents,
    };
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
    }

    fn write_failure_artifact(filename: &str, content: &str) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let run_id = format!("{}_{}", ts_ms, std::process::id());
        let dir = repo_root()
            .join("tests")
            .join("artifacts")
            .join("agents")
            .join(run_id);

        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(filename), content);
    }

    fn validate_report_schema(v: &Value) -> Result<(), String> {
        let format_version = v
            .get("format_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| "missing or invalid `format_version`".to_string())?;
        if format_version != 1 {
            return Err(format!("unexpected format_version: {format_version}"));
        }

        let generated_at = v
            .get("generated_at")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing or invalid `generated_at`".to_string())?;
        if generated_at.trim().is_empty() {
            return Err("`generated_at` is empty".to_string());
        }

        let summary = v
            .get("summary")
            .and_then(Value::as_object)
            .ok_or_else(|| "missing or invalid `summary`".to_string())?;
        let detected_count = summary
            .get("detected_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| "missing or invalid `summary.detected_count`".to_string())?;
        let total_count = summary
            .get("total_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| "missing or invalid `summary.total_count`".to_string())?;
        if detected_count > total_count {
            return Err(format!(
                "detected_count ({detected_count}) > total_count ({total_count})"
            ));
        }

        let agents = v
            .get("installed_agents")
            .and_then(Value::as_array)
            .ok_or_else(|| "missing or invalid `installed_agents`".to_string())?;
        for entry in agents {
            let obj = entry
                .as_object()
                .ok_or_else(|| "installed_agents entry is not an object".to_string())?;
            let slug = obj
                .get("slug")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing or invalid entry.slug".to_string())?;
            if slug.trim().is_empty() {
                return Err("entry.slug is empty".to_string());
            }
            obj.get("detected")
                .and_then(Value::as_bool)
                .ok_or_else(|| "missing or invalid entry.detected".to_string())?;
            obj.get("evidence")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing or invalid entry.evidence".to_string())?;
            obj.get("root_paths")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing or invalid entry.root_paths".to_string())?;
        }

        Ok(())
    }

    #[test]
    fn detect_installed_agents_report_schema_is_stable() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let codex_root = tmp.path().join("codex-home").join("sessions");
        std::fs::create_dir_all(&codex_root).expect("create codex sessions");

        let gemini_root = tmp.path().join("gemini-home").join("tmp");
        std::fs::create_dir_all(&gemini_root).expect("create gemini root");

        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string(), "gemini".to_string()]),
            include_undetected: true,
            root_overrides: vec![
                AgentDetectRootOverride {
                    slug: "codex".to_string(),
                    root: codex_root,
                },
                AgentDetectRootOverride {
                    slug: "gemini".to_string(),
                    root: gemini_root,
                },
            ],
        })
        .expect("detect");

        let json = serde_json::to_string_pretty(&report).expect("json");
        let v: Value = serde_json::from_str(&json).expect("parse");

        if let Err(e) = validate_report_schema(&v) {
            write_failure_artifact("installed_agents_report.json", &json);
            panic!("agent detection report schema validation failed: {e}");
        }

        // A couple load-bearing content checks for determinism.
        let slugs: Vec<&str> = v["installed_agents"]
            .as_array()
            .expect("installed_agents array")
            .iter()
            .filter_map(|e| e.get("slug").and_then(Value::as_str))
            .collect();
        assert_eq!(slugs, vec!["codex", "gemini"]);
        assert_eq!(v["summary"]["total_count"].as_u64(), Some(2));
        assert_eq!(v["summary"]["detected_count"].as_u64(), Some(2));
    }
}
