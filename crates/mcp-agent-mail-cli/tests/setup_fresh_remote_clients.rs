//! Real `am setup run` regressions for fresh Codex/OMP installs (GH#327).
//!
//! The decisive path is the built CLI, real discovery, real credential/config
//! writers, and real Git. Agent executable fixtures are presence witnesses only;
//! executing one is a test failure. These are not signed-release installer tests.

#![cfg(unix)]
#![forbid(unsafe_code)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};

const URL: &str = "http://127.0.0.1:8765/mcp/";

struct Sandbox {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    project: PathBuf,
    tools: PathBuf,
}

impl Sandbox {
    fn new(presence: &str) -> Self {
        let temp = tempfile::tempdir().expect("private test root");
        // Stock macOS /tmp is a symlink; the production writer correctly
        // refuses symlink ancestry, so use the physical root, not a bypass.
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let project = root.join("project");
        let tools = root.join("tools");
        for path in [&home, &project, &tools, &root.join("tmp")] {
            fs::create_dir_all(path).unwrap();
        }
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|path| path.join("git"))
            .find(|path| {
                fs::metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .expect("these setup tests require real Git on the test runner's PATH")
            .canonicalize()
            .unwrap();
        symlink(git, tools.join("git")).unwrap();
        match presence {
            "path" | "non_executable" => {
                for name in ["codex", "omp"] {
                    let path = tools.join(name);
                    fs::write(
                        &path,
                        "#!/bin/sh\nprintf executed > \"$AM_TEST_AGENT_EXECUTED\"\nexit 91\n",
                    )
                    .unwrap();
                    let mode = if presence == "path" { 0o755 } else { 0o644 };
                    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
                }
            }
            "config" | "config_xdg" => {
                let codex = if presence == "config" {
                    ".codex"
                } else {
                    ".config/codex"
                };
                fs::create_dir_all(home.join(codex)).unwrap();
                fs::create_dir_all(home.join(".omp")).unwrap();
            }
            "none" => {}
            _ => panic!("unknown presence fixture: {presence}"),
        }
        let sandbox = Self {
            _temp: temp,
            root,
            home,
            project,
            tools,
        };
        require_success(
            &sandbox.git(&["-c", "init.defaultBranch=main", "init", "--quiet"]),
            "initialize isolated Git project",
        );
        sandbox
    }

    fn command(&self, binary: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("PATH", &self.tools)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("AM_INTERFACE_MODE", "cli")
            .env("STORAGE_ROOT", self.root.join("storage"))
            .env(
                "DATABASE_URL",
                format!("sqlite:///{}", self.root.join("storage.sqlite3").display()),
            )
            .env("HTTP_HOST", "127.0.0.1")
            .env("HTTP_PORT", "8765")
            .env("HTTP_PATH", "/mcp/")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("AM_TEST_AGENT_EXECUTED", self.root.join("agent-executed"))
            .current_dir(&self.project);
        command
    }

    fn setup(&self) -> Output {
        self.command(env!("CARGO_BIN_EXE_am"))
            // No --agent: automatic discovery is the behavior under test.
            .args(["setup", "run", "--yes", "--no-hooks"])
            .output()
            .expect("execute built am CLI")
    }

    fn git(&self, args: &[&str]) -> Output {
        self.command(self.tools.join("git"))
            .args(args)
            .output()
            .expect("execute real Git")
    }

    fn token_path(&self) -> PathBuf {
        self.home.join(".config/mcp-agent-mail/config.env")
    }

    fn credential_paths(&self) -> [PathBuf; 4] {
        [
            self.token_path(),
            self.home.join(".codex/config.toml"),
            self.home.join(".omp/agent/mcp.json"),
            self.project.join(".omp/mcp.json"),
        ]
    }

    fn assert_not_executed(&self) {
        assert!(
            !self.root.join("agent-executed").exists(),
            "setup executed a presence-only agent fixture"
        );
    }

    fn assert_credentials(&self) -> String {
        for path in self.credential_paths() {
            let metadata = fs::symlink_metadata(&path).expect("credential file exists");
            assert!(
                metadata.is_file(),
                "{} is not a regular file",
                path.display()
            );
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                "{} must be private",
                path.display()
            );
        }
        let env_text = fs::read_to_string(self.token_path()).unwrap();
        let tokens: Vec<&str> = env_text
            .lines()
            .filter_map(|line| line.strip_prefix("HTTP_BEARER_TOKEN="))
            .collect();
        assert_eq!(tokens.len(), 1, "one canonical token assignment");
        let token = tokens[0];
        assert!(
            !token.is_empty(),
            "a zero exit must not hide a missing token"
        );
        let authorization = format!("Bearer {token}");
        let codex: toml_edit::DocumentMut =
            fs::read_to_string(self.home.join(".codex/config.toml"))
                .unwrap()
                .parse()
                .expect("real Codex TOML parses");
        let entry = &codex["mcp_servers"]["mcp_agent_mail"];
        assert_eq!(entry["url"].as_str(), Some(URL));
        assert_eq!(
            entry["http_headers"]["Authorization"].as_str(),
            Some(authorization.as_str())
        );
        for path in [
            self.home.join(".omp/agent/mcp.json"),
            self.project.join(".omp/mcp.json"),
        ] {
            let omp: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            let entry = &omp["mcpServers"]["mcp-agent-mail"];
            assert_eq!(entry["url"].as_str(), Some(URL));
            assert_eq!(entry["enabled"].as_bool(), Some(true));
            assert_eq!(
                entry["headers"]["Authorization"].as_str(),
                Some(authorization.as_str())
            );
        }
        self.assert_not_executed();
        token.to_owned()
    }
}

fn require_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation}: {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fresh_remote_clients_get_one_durable_credential_and_private_configs() {
    for presence in ["path", "config", "config_xdg"] {
        let sandbox = Sandbox::new(presence);
        assert!(!sandbox.home.join(".codex/sessions").exists());
        assert!(!sandbox.home.join(".omp/agent/sessions").exists());
        require_success(&sandbox.setup(), presence);
        let token = sandbox.assert_credentials();
        // An idempotent re-run must repair disclosure-prone permissions without
        // rotating the credential and splitting client/server authentication.
        for path in sandbox.credential_paths() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        require_success(&sandbox.setup(), "repeat setup and repair permissions");
        assert_eq!(sandbox.assert_credentials(), token);
        require_success(
            &sandbox.git(&["check-ignore", "--no-index", "--", ".omp/mcp.json"]),
            "native setup must protect the project credential with Git ignore",
        );
        require_success(&sandbox.git(&["add", "--all"]), "stage isolated project");
        let tracked = sandbox.git(&["ls-files", "--", ".omp/mcp.json"]);
        require_success(&tracked, "inspect tracked project paths");
        assert!(
            tracked.stdout.is_empty(),
            "git add --all staged a credential"
        );
    }
}

#[test]
fn no_clients_or_nonexecutable_names_do_not_create_credentials() {
    for presence in ["none", "non_executable"] {
        let sandbox = Sandbox::new(presence);
        let output = sandbox.setup();
        require_success(&output, "no-client setup");
        assert!(String::from_utf8_lossy(&output.stdout).contains("No coding agents detected."));
        for path in sandbox.credential_paths() {
            assert!(!path.exists(), "no-client setup wrote {}", path.display());
        }
        sandbox.assert_not_executed();
    }
}

#[test]
fn symlinked_token_authority_fails_before_any_client_write() {
    let sandbox = Sandbox::new("path");
    let sentinel = sandbox.root.join("outside-token.env");
    let original = "HTTP_BEARER_TOKEN=do-not-read-or-overwrite\n";
    fs::write(&sentinel, original).unwrap();
    fs::create_dir_all(sandbox.token_path().parent().unwrap()).unwrap();
    symlink(&sentinel, sandbox.token_path()).unwrap();
    let output = sandbox.setup();
    assert!(
        !output.status.success(),
        "unsafe token authority reported success"
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), original);
    assert!(
        fs::symlink_metadata(sandbox.token_path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    for path in sandbox.credential_paths().iter().skip(1) {
        assert!(
            !path.exists(),
            "unsafe authority still configured {}",
            path.display()
        );
    }
    sandbox.assert_not_executed();
}

#[test]
fn tracked_project_config_is_not_overwritten_or_reported_successful() {
    let sandbox = Sandbox::new("path");
    let config = sandbox.project.join(".omp/mcp.json");
    let original = "{\"mcpServers\":{\"unrelated\":{\"command\":\"/bin/false\"}}}\n";
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, original).unwrap();
    require_success(
        &sandbox.git(&["add", "--", ".omp/mcp.json"]),
        "track fixture config",
    );
    let output = sandbox.setup();
    assert!(
        !output.status.success(),
        "tracked secret destination reported success"
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    let index = sandbox.git(&["show", ":.omp/mcp.json"]);
    require_success(&index, "read original tracked config");
    assert_eq!(index.stdout, original.as_bytes());
    sandbox.assert_not_executed();
}
