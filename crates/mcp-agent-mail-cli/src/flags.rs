//! Operator-facing feature flag registry commands.

use clap::Subcommand;
use mcp_agent_mail_core::Config;
use mcp_agent_mail_core::flags::{self as core_flags, FlagRegistryError, FlagSnapshot};

use crate::{CliError, CliResult, output};

#[derive(Subcommand, Debug)]
pub enum FlagsCommand {
    #[command(name = "list")]
    List {
        /// Only show flags whose current value differs from the default.
        #[arg(long, default_value_t = false)]
        set: bool,
        /// Only show experimental flags.
        #[arg(long, default_value_t = false)]
        experimental: bool,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "status")]
    Status {
        /// Flag name or env var.
        name: String,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "explain")]
    Explain {
        /// Flag name or env var.
        name: String,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "on")]
    On {
        /// Dynamic boolean flag name or env var.
        name: String,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(name = "off")]
    Off {
        /// Dynamic boolean flag name or env var.
        name: String,
        /// Output format: table, json, or toon.
        #[arg(long, value_parser)]
        format: Option<output::CliOutputFormat>,
        /// Output JSON (shorthand for --format json).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub fn handle_flags(action: FlagsCommand) -> CliResult<()> {
    match action {
        FlagsCommand::List {
            set,
            experimental,
            format,
            json,
        } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = Config::from_env();
            let mut snapshots = core_flags::flag_snapshots(&config);
            if set {
                snapshots.retain(|snapshot| snapshot.current_value != snapshot.default_value);
            }
            if experimental {
                snapshots.retain(|snapshot| snapshot.stability == "experimental");
            }
            render_flag_list(&snapshots, fmt)
        }
        FlagsCommand::Status { name, format, json } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = Config::from_env();
            let snapshot = resolve_snapshot(&config, &name)?;
            render_flag_status(&snapshot, fmt)
        }
        FlagsCommand::Explain { name, format, json } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = Config::from_env();
            let snapshot = resolve_snapshot(&config, &name)?;
            render_flag_explain(&snapshot, fmt)
        }
        FlagsCommand::On { name, format, json } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = Config::from_env();
            let snapshot =
                core_flags::toggle_bool_flag(&config, &name, true).map_err(|error| {
                    map_flag_error(error, &name)
                })?;
            render_flag_mutation(&snapshot, "enabled", fmt)
        }
        FlagsCommand::Off { name, format, json } => {
            let fmt = output::CliOutputFormat::resolve(format, json);
            let config = Config::from_env();
            let snapshot =
                core_flags::toggle_bool_flag(&config, &name, false).map_err(|error| {
                    map_flag_error(error, &name)
                })?;
            render_flag_mutation(&snapshot, "disabled", fmt)
        }
    }
}

fn resolve_snapshot(config: &Config, name: &str) -> CliResult<FlagSnapshot> {
    let flag = core_flags::find_flag(name).ok_or_else(|| unknown_flag_error(name))?;
    Ok(core_flags::flag_snapshot(config, flag))
}

fn dynamic_display(snapshot: &FlagSnapshot) -> &'static str {
    if snapshot.dynamic_toggle {
        "yes"
    } else if snapshot.restart_required {
        "restart"
    } else {
        "no"
    }
}

fn render_flag_list(snapshots: &[FlagSnapshot], fmt: output::CliOutputFormat) -> CliResult<()> {
    match fmt {
        output::CliOutputFormat::Json => {
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::to_string_pretty(snapshots)
                    .map_err(|error| CliError::Other(format!("failed to serialize flags: {error}")))?
            );
            Ok(())
        }
        output::CliOutputFormat::Toon => {
            if snapshots.is_empty() {
                ftui_runtime::ftui_println!("flags{{count}}:0");
                return Ok(());
            }
            for snapshot in snapshots {
                ftui_runtime::ftui_println!(
                    "flag{{name,subsystem,value,source,stability,dynamic}}:{},{},{},{},{},{}",
                    snapshot.name,
                    snapshot.subsystem,
                    snapshot.current_value,
                    snapshot.source,
                    snapshot.stability,
                    dynamic_display(snapshot)
                );
            }
            Ok(())
        }
        output::CliOutputFormat::Table => {
            if snapshots.is_empty() {
                ftui_runtime::ftui_println!("No flags matched the requested filters.");
                return Ok(());
            }

            let mut current_subsystem: Option<&str> = None;
            let mut table: Option<output::CliTable> = None;

            for snapshot in snapshots {
                if current_subsystem != Some(snapshot.subsystem.as_str()) {
                    if let Some(rendered) = table.take() {
                        rendered.render();
                        ftui_runtime::ftui_println!("");
                    }
                    current_subsystem = Some(snapshot.subsystem.as_str());
                    ftui_runtime::ftui_println!("## {}", snapshot.subsystem);
                    table = Some(output::CliTable::new(vec![
                        "NAME",
                        "VALUE",
                        "SOURCE",
                        "STABILITY",
                        "DYNAMIC",
                    ]));
                }

                table
                    .as_mut()
                    .expect("table should be initialized for subsystem")
                    .add_row(vec![
                        snapshot.name.clone(),
                        snapshot.current_value.clone(),
                        snapshot.source.clone(),
                        snapshot.stability.clone(),
                        dynamic_display(snapshot).to_string(),
                    ]);
            }

            if let Some(rendered) = table.take() {
                rendered.render();
            }
            Ok(())
        }
    }
}

fn render_flag_status(snapshot: &FlagSnapshot, fmt: output::CliOutputFormat) -> CliResult<()> {
    match fmt {
        output::CliOutputFormat::Json => {
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::to_string_pretty(snapshot).map_err(|error| {
                    CliError::Other(format!("failed to serialize flag status: {error}"))
                })?
            );
            Ok(())
        }
        output::CliOutputFormat::Toon => {
            ftui_runtime::ftui_println!(
                "flag{{name,value,source,default,stability,dynamic}}:{},{},{},{},{},{}",
                snapshot.name,
                snapshot.current_value,
                snapshot.source,
                snapshot.default_value,
                snapshot.stability,
                dynamic_display(snapshot)
            );
            Ok(())
        }
        output::CliOutputFormat::Table => {
            let mut table =
                output::CliTable::new(vec!["NAME", "VALUE", "SOURCE", "DEFAULT", "DYNAMIC"]);
            table.add_row(vec![
                snapshot.name.clone(),
                snapshot.current_value.clone(),
                snapshot.source.clone(),
                snapshot.default_value.clone(),
                dynamic_display(snapshot).to_string(),
            ]);
            table.render();
            Ok(())
        }
    }
}

fn render_flag_explain(snapshot: &FlagSnapshot, fmt: output::CliOutputFormat) -> CliResult<()> {
    match fmt {
        output::CliOutputFormat::Json => {
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::to_string_pretty(snapshot).map_err(|error| {
                    CliError::Other(format!("failed to serialize flag explanation: {error}"))
                })?
            );
            Ok(())
        }
        output::CliOutputFormat::Toon => {
            ftui_runtime::ftui_println!(
                "flag_explain{{name,env_var,value,source,stability,dynamic,kind}}:{},{},{},{},{},{},{}",
                snapshot.name,
                snapshot.env_var,
                snapshot.current_value,
                snapshot.source,
                snapshot.stability,
                dynamic_display(snapshot),
                snapshot.kind
            );
            ftui_runtime::ftui_println!("flag_doc{{name}}:{}", snapshot.doc);
            if let Some(notes) = &snapshot.notes {
                ftui_runtime::ftui_println!("flag_notes{{name}}:{}", notes);
            }
            Ok(())
        }
        output::CliOutputFormat::Table => {
            let mut table = output::CliTable::new(vec!["FIELD", "VALUE"]);
            table.add_row(vec!["Name".to_string(), snapshot.name.clone()]);
            table.add_row(vec!["Env var".to_string(), snapshot.env_var.clone()]);
            table.add_row(vec!["Current".to_string(), snapshot.current_value.clone()]);
            table.add_row(vec!["Source".to_string(), snapshot.source.clone()]);
            table.add_row(vec!["Default".to_string(), snapshot.default_value.clone()]);
            table.add_row(vec!["Kind".to_string(), snapshot.kind.clone()]);
            table.add_row(vec!["Allowed".to_string(), snapshot.allowed_values.join(" | ")]);
            table.add_row(vec!["Stability".to_string(), snapshot.stability.clone()]);
            table.add_row(vec![
                "Dynamic toggle".to_string(),
                dynamic_display(snapshot).to_string(),
            ]);
            table.add_row(vec!["Config path".to_string(), snapshot.config_path.clone()]);
            table.add_row(vec![
                "Subsystems".to_string(),
                snapshot.affected_subsystems.join(", "),
            ]);
            table.render();
            ftui_runtime::ftui_println!("");
            ftui_runtime::ftui_println!("{}", snapshot.doc);
            if let Some(notes) = &snapshot.notes {
                ftui_runtime::ftui_println!("");
                ftui_runtime::ftui_println!("Notes: {notes}");
            }
            Ok(())
        }
    }
}

fn render_flag_mutation(
    snapshot: &FlagSnapshot,
    action: &str,
    fmt: output::CliOutputFormat,
) -> CliResult<()> {
    match fmt {
        output::CliOutputFormat::Json => {
            ftui_runtime::ftui_println!(
                "{}",
                serde_json::to_string_pretty(snapshot).map_err(|error| {
                    CliError::Other(format!("failed to serialize updated flag: {error}"))
                })?
            );
            Ok(())
        }
        output::CliOutputFormat::Toon => {
            ftui_runtime::ftui_println!(
                "flag_change{{name,action,value,source,config_path}}:{},{},{},{},{}",
                snapshot.name,
                action,
                snapshot.current_value,
                snapshot.source,
                snapshot.config_path
            );
            Ok(())
        }
        output::CliOutputFormat::Table => {
            ftui_runtime::ftui_println!(
                "Updated {} ({}) in {}.",
                snapshot.name,
                action,
                snapshot.config_path
            );
            render_flag_status(snapshot, output::CliOutputFormat::Table)
        }
    }
}

fn map_flag_error(error: FlagRegistryError, requested: &str) -> CliError {
    match error {
        FlagRegistryError::UnknownFlag(_) => unknown_flag_error(requested),
        other => CliError::Other(other.to_string()),
    }
}

fn unknown_flag_error(name: &str) -> CliError {
    let suggestions = flag_suggestions(name);
    if suggestions.is_empty() {
        CliError::Other(format!("unknown flag '{name}'"))
    } else {
        CliError::Other(format!(
            "unknown flag '{name}'. Did you mean: {}?",
            suggestions.join(", ")
        ))
    }
}

fn flag_suggestions(name: &str) -> Vec<String> {
    let needle = name.to_ascii_lowercase();
    let mut scored = core_flags::flag_registry()
        .iter()
        .map(|flag| {
            let name_score = levenshtein(&needle, &flag.name.to_ascii_lowercase());
            let env_score = levenshtein(&needle, &flag.env_var.to_ascii_lowercase());
            (name_score.min(env_score), flag.name)
        })
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, flag_name)| (*score, *flag_name));
    scored
        .into_iter()
        .filter(|(score, _)| *score <= 8)
        .map(|(_, flag_name)| flag_name.to_string())
        .take(5)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    if left == right {
        return 0;
    }
    if left.is_empty() {
        return right.chars().count();
    }
    if right.is_empty() {
        return left.chars().count();
    }

    let right_chars = right.chars().collect::<Vec<_>>();
    let mut prev = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; right_chars.len() + 1];

    for (i, left_char) in left.chars().enumerate() {
        curr[0] = i + 1;
        for (j, right_char) in right_chars.iter().enumerate() {
            let substitution_cost = usize::from(left_char != *right_char);
            curr[j + 1] = (prev[j + 1] + 1)
                .min(curr[j] + 1)
                .min(prev[j] + substitution_cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[right_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::sync::{LazyLock, Mutex};

    static STDIO_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn capture_output(run: impl FnOnce() -> CliResult<()>) -> (CliResult<()>, String) {
        let _guard = STDIO_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let capture = ftui_runtime::StdioCapture::install().expect("install capture");
        let result = run();
        let output = capture.drain_to_string();
        (result, output)
    }

    fn extract_json(output: &str) -> serde_json::Value {
        let trimmed = output.trim();
        let start = trimmed
            .find(['{', '['])
            .expect("expected JSON object or array in output");
        serde_json::from_str(&trimmed[start..]).expect("parse json output")
    }

    #[test]
    fn clap_parses_flags_list_filters() {
        let cli = crate::Cli::try_parse_from([
            "am",
            "flags",
            "list",
            "--set",
            "--experimental",
            "--format",
            "json",
        ])
        .expect("parse flags list");

        match cli.command.expect("expected command") {
            crate::Commands::Flags {
                action:
                    FlagsCommand::List {
                        set,
                        experimental,
                        format,
                        json,
                    },
            } => {
                assert!(set);
                assert!(experimental);
                assert_eq!(format, Some(output::CliOutputFormat::Json));
                assert!(!json);
            }
            other => panic!("expected Flags List, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_flags_on() {
        let cli = crate::Cli::try_parse_from(["am", "flags", "on", "ATC_LEARNING_DISABLED"])
            .expect("parse flags on");

        match cli.command.expect("expected command") {
            crate::Commands::Flags {
                action: FlagsCommand::On { name, format, json },
            } => {
                assert_eq!(name, "ATC_LEARNING_DISABLED");
                assert!(format.is_none());
                assert!(!json);
            }
            other => panic!("expected Flags On, got {other:?}"),
        }
    }

    #[test]
    fn unknown_flag_reports_suggestion() {
        let error = handle_flags(FlagsCommand::Status {
            name: "ATC_LEARNING_DISABLE".to_string(),
            format: Some(output::CliOutputFormat::Json),
            json: false,
        })
        .expect_err("unknown flag should fail");

        assert!(
            error.to_string().contains("Did you mean: ATC_LEARNING_DISABLED"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn handle_flags_on_writes_console_persist_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.env");
        let config_path_str = config_path.to_string_lossy().to_string();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("CONSOLE_PERSIST_PATH", &config_path_str)],
            || {
                let (result, output) = capture_output(|| {
                    handle_flags(FlagsCommand::On {
                        name: "ATC_LEARNING_DISABLED".to_string(),
                        format: Some(output::CliOutputFormat::Json),
                        json: false,
                    })
                });
                assert!(result.is_ok(), "flags on failed: {result:?}");
                let parsed = extract_json(&output);
                assert_eq!(parsed["name"], "ATC_LEARNING_DISABLED");
                assert_eq!(parsed["current_value"], "true");
                assert_eq!(parsed["source"], "config");

                let written = std::fs::read_to_string(&config_path).expect("read config env");
                assert!(
                    written.contains("ATC_LEARNING_DISABLED=true"),
                    "expected persisted toggle, got: {written}"
                );
            },
        );
    }

    #[test]
    fn handle_flags_status_json_reports_config_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.env");
        std::fs::write(&config_path, "ATC_LEARNING_DISABLED=true\n").expect("seed config env");
        let config_path_str = config_path.to_string_lossy().to_string();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("CONSOLE_PERSIST_PATH", &config_path_str)],
            || {
                let (result, output) = capture_output(|| {
                    handle_flags(FlagsCommand::Status {
                        name: "ATC_LEARNING_DISABLED".to_string(),
                        format: Some(output::CliOutputFormat::Json),
                        json: false,
                    })
                });
                assert!(result.is_ok(), "flags status failed: {result:?}");
                let parsed = extract_json(&output);
                assert_eq!(parsed["current_value"], "true");
                assert_eq!(parsed["source"], "config");
            },
        );
    }
}
