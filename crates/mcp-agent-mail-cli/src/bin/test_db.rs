#![forbid(unsafe_code)]

use std::env;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mcp_agent_mail_db::DbConn;
use sqlmodel_core::Value;

#[derive(Debug, Default, Clone, Copy)]
struct OutputOptions {
    header: bool,
    column_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompatCommand {
    Sql(String),
    Tables,
    Schema(Option<String>),
    Backup(PathBuf),
    Timeout,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let (db_path, options, raw_commands) = parse_cli_args(env::args().skip(1).collect())?;
    let commands = if raw_commands.is_empty() {
        parse_stdin_commands()?
    } else {
        parse_commands_from_args(&raw_commands)?
    };
    if commands.is_empty() {
        return Err("no SQL or dot-command provided".to_string());
    }

    let db_path_str = db_path.to_string_lossy().into_owned();
    let conn = DbConn::open_file(&db_path_str)
        .map_err(|err| format!("cannot open {}: {err}", db_path.display()))?;

    for command in commands {
        run_command(&conn, &db_path, options, command)?;
    }

    Ok(())
}

fn parse_cli_args(args: Vec<String>) -> Result<(PathBuf, OutputOptions, Vec<String>), String> {
    let mut options = OutputOptions::default();
    let mut index = 0_usize;
    while index < args.len() {
        match args[index].as_str() {
            "-batch" => {}
            "-header" => options.header = true,
            "-noheader" => options.header = false,
            "-column" => options.column_mode = true,
            "--help" | "-h" => return Err(print_usage_text().to_string()),
            value if value.starts_with('-') => {
                return Err(format!("unsupported flag: {value}"));
            }
            _ => break,
        }
        index += 1;
    }

    if index >= args.len() {
        return Err("usage: test_db [-batch] [-header|-noheader] [-column] <db_path> [sql_or_dot_command ...]".to_string());
    }

    let db_path = PathBuf::from(&args[index]);
    let commands = args[index + 1..].to_vec();
    Ok((db_path, options, commands))
}

fn parse_stdin_commands() -> Result<Vec<CompatCommand>, String> {
    let mut stdin = String::new();
    io::stdin()
        .read_to_string(&mut stdin)
        .map_err(|err| format!("failed to read stdin: {err}"))?;
    parse_command_script(&stdin)
}

fn parse_commands_from_args(args: &[String]) -> Result<Vec<CompatCommand>, String> {
    let mut commands = Vec::new();
    for arg in args {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('.') {
            commands.push(parse_dot_command(trimmed)?);
        } else {
            commands.extend(
                split_sql_statements(trimmed)
                    .into_iter()
                    .map(CompatCommand::Sql),
            );
        }
    }
    Ok(commands)
}

fn parse_command_script(script: &str) -> Result<Vec<CompatCommand>, String> {
    let mut commands = Vec::new();
    let mut sql_buffer = String::new();

    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('.') {
            commands.extend(
                split_sql_statements(&sql_buffer)
                    .into_iter()
                    .map(CompatCommand::Sql),
            );
            sql_buffer.clear();
            commands.push(parse_dot_command(trimmed)?);
            continue;
        }
        sql_buffer.push_str(line);
        sql_buffer.push('\n');
    }

    commands.extend(
        split_sql_statements(&sql_buffer)
            .into_iter()
            .map(CompatCommand::Sql),
    );
    Ok(commands)
}

fn parse_dot_command(command: &str) -> Result<CompatCommand, String> {
    let mut parts = command.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match verb {
        ".tables" => Ok(CompatCommand::Tables),
        ".schema" => Ok(CompatCommand::Schema(
            (!rest.is_empty()).then(|| rest.to_string()),
        )),
        ".backup" => {
            if rest.is_empty() {
                return Err(".backup requires a destination path".to_string());
            }
            Ok(CompatCommand::Backup(PathBuf::from(strip_shell_quotes(
                rest,
            ))))
        }
        ".timeout" => Ok(CompatCommand::Timeout),
        _ => Err(format!("unsupported dot-command: {command}")),
    }
}

fn strip_shell_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let first = trimmed.as_bytes()[0] as char;
        let last = trimmed.as_bytes()[trimmed.len() - 1] as char;
        if (first == '\'' && last == '\'') || (first == '"' && last == '"') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

fn split_sql_statements(script: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for ch in script.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            ';' if !in_single && !in_double => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    statements.push(trimmed.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        statements.push(trimmed.to_string());
    }

    statements
}

fn run_command(
    conn: &DbConn,
    db_path: &Path,
    options: OutputOptions,
    command: CompatCommand,
) -> Result<(), String> {
    match command {
        CompatCommand::Sql(sql) => run_sql(conn, options, &sql),
        CompatCommand::Tables => print_tables(conn),
        CompatCommand::Schema(table) => print_schema(conn, table.as_deref()),
        CompatCommand::Backup(destination) => backup_database(conn, db_path, &destination),
        CompatCommand::Timeout => Ok(()),
    }
}

fn run_sql(conn: &DbConn, options: OutputOptions, sql: &str) -> Result<(), String> {
    if sql_returns_rows(sql) {
        let rows = conn
            .query_sync(sql, &[])
            .map_err(|err| format!("query failed for `{sql}`: {err}"))?;
        print_rows(&rows, options);
        return Ok(());
    }

    conn.execute_raw(sql)
        .map_err(|err| format!("statement failed for `{sql}`: {err}"))?;
    Ok(())
}

fn sql_returns_rows(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("PRAGMA")
        || upper.starts_with("EXPLAIN")
}

fn print_tables(conn: &DbConn) -> Result<(), String> {
    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
            &[],
        )
        .map_err(|err| format!("list tables failed: {err}"))?;
    let names = rows
        .iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .collect::<Vec<_>>();
    if !names.is_empty() {
        println!("{}", names.join(" "));
    }
    Ok(())
}

fn print_schema(conn: &DbConn, table: Option<&str>) -> Result<(), String> {
    let (sql, params) = if let Some(table) = table {
        (
            "SELECT sql FROM sqlite_master \
             WHERE sql IS NOT NULL AND name = ? \
             ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'trigger' THEN 2 ELSE 3 END, name",
            vec![Value::Text(table.to_string())],
        )
    } else {
        (
            "SELECT sql FROM sqlite_master \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
             ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'trigger' THEN 2 ELSE 3 END, name",
            Vec::new(),
        )
    };
    let rows = conn
        .query_sync(sql, &params)
        .map_err(|err| format!("schema query failed: {err}"))?;
    for row in rows {
        if let Ok(statement) = row.get_named::<String>("sql") {
            if statement.trim_end().ends_with(';') {
                println!("{statement}");
            } else {
                println!("{statement};");
            }
        }
    }
    Ok(())
}

fn backup_database(conn: &DbConn, source: &Path, destination: &Path) -> Result<(), String> {
    let _ = conn.execute_raw("PRAGMA wal_checkpoint(TRUNCATE)");
    std::fs::copy(source, destination).map_err(|err| {
        format!(
            "backup {} from {} failed: {err}",
            destination.display(),
            source.display()
        )
    })?;
    Ok(())
}

fn print_rows(rows: &[sqlmodel_core::Row], options: OutputOptions) {
    if rows.is_empty() {
        return;
    }

    let headers = rows[0]
        .column_names()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let matrix = rows
        .iter()
        .map(|row| row.values().cloned().map(render_value).collect::<Vec<_>>())
        .collect::<Vec<_>>();

    if options.column_mode {
        print_rows_column_mode(&headers, &matrix, options.header);
    } else {
        print_rows_pipe_mode(&headers, &matrix, options.header);
    }
}

fn print_rows_pipe_mode(headers: &[String], matrix: &[Vec<String>], header: bool) {
    if header {
        println!("{}", headers.join("|"));
    }
    for row in matrix {
        println!("{}", row.join("|"));
    }
}

fn print_rows_column_mode(headers: &[String], matrix: &[Vec<String>], header: bool) {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in matrix {
        for (index, value) in row.iter().enumerate() {
            if index >= widths.len() {
                widths.push(value.len());
            } else {
                widths[index] = widths[index].max(value.len());
            }
        }
    }

    if header {
        println!("{}", format_aligned_row(headers, &widths));
    }
    for row in matrix {
        println!("{}", format_aligned_row(row, &widths));
    }
}

fn format_aligned_row(values: &[String], widths: &[usize]) -> String {
    let mut rendered = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            rendered.push_str("  ");
        }
        let width = widths.get(index).copied().unwrap_or(value.len());
        let _ = write!(rendered, "{value:<width$}");
    }
    rendered.trim_end().to_string()
}

fn render_value(value: Value) -> String {
    match value {
        Value::Null | Value::Default => String::new(),
        Value::Bool(flag) => {
            if flag {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Value::TinyInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Double(n) => n.to_string(),
        Value::Decimal(s) => s,
        Value::Text(s) => s,
        Value::Bytes(bytes) => hex::encode(bytes),
        Value::Date(days) => days.to_string(),
        Value::Time(micros) => micros.to_string(),
        Value::Timestamp(micros) => micros.to_string(),
        Value::TimestampTz(micros) => micros.to_string(),
        Value::Uuid(uuid) => hex::encode(uuid),
        Value::Json(json) => json.to_string(),
        Value::Array(items) => serde_json::to_string(&items).unwrap_or_default(),
    }
}

fn print_usage_text() -> &'static str {
    "Usage: test_db [-batch] [-header|-noheader] [-column] <db_path> [sql_or_dot_command ...]\nSupported dot-commands: .tables, .schema [table], .backup <path>, .timeout <ms>"
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CompatCommand, parse_command_script, parse_dot_command, split_sql_statements};

    #[test]
    fn split_sql_statements_handles_multiple_statements() {
        assert_eq!(
            split_sql_statements("SELECT 1; SELECT 'two';"),
            vec!["SELECT 1".to_string(), "SELECT 'two'".to_string()]
        );
    }

    #[test]
    fn parse_command_script_keeps_dot_commands() {
        let commands =
            parse_command_script(".timeout 5000\nSELECT 1;\n.backup '/tmp/snapshot.sqlite3'\n")
                .expect("parse script");
        assert_eq!(
            commands,
            vec![
                CompatCommand::Timeout,
                CompatCommand::Sql("SELECT 1".to_string()),
                CompatCommand::Backup(PathBuf::from("/tmp/snapshot.sqlite3")),
            ]
        );
    }

    #[test]
    fn parse_dot_command_strips_shell_quotes() {
        assert_eq!(
            parse_dot_command(".backup '/tmp/a.sqlite3'").expect("backup command"),
            CompatCommand::Backup(PathBuf::from("/tmp/a.sqlite3"))
        );
    }
}
