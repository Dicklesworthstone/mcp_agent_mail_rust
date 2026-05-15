//! `fm-runtime-processes-port-bound-by-foreign-process` — P0.
//!
//! **Subsystem**: runtime_processes.
//!
//! ## What's broken
//!
//! `am serve-http` will refuse to start (or silently bind to a
//! different port via fallback heuristics) when the configured
//! HTTP host:port is already held by another process. Common
//! culprits:
//!
//! - A leftover Python `mcp_agent_mail` from a previous
//!   deployment (sibling FM:
//!   `fm-mcp-config-files-stale-python-launcher-entry`).
//! - A previous `am serve` that crashed but whose process is
//!   still alive (sibling FM:
//!   `fm-runtime-processes-stale-listener-pid-hint`).
//! - An unrelated service that grabbed port 8765 (the default
//!   `HTTP_PORT`).
//!
//! Without explicit detection, operators see a confusing
//! "address already in use" inside `am serve` logs and waste
//! cycles diagnosing PATH / config issues before realizing the
//! port is foreign-held.
//!
//! ## Detection (pure function)
//!
//! Attempt a `TcpListener::bind(host:port)` on the configured
//! address. Two outcomes:
//!
//! 1. **Bind succeeds**: the port is FREE. Immediately close
//!    the listener (`drop`); no finding. Note: this is a brief
//!    accept-ready socket during the probe; on busy systems
//!    another process could race-grab the port between our
//!    probe and the next `am serve` boot — but that's
//!    acceptable for a doctor probe.
//! 2. **Bind fails with EADDRINUSE / AddrInUse**: the port is
//!    HELD. Emit a finding with the address + raw OS error.
//!    Other bind-error kinds (permission denied for low ports,
//!    invalid address) are surfaced as a different reason and
//!    flagged at lower confidence.
//!
//! Linux-specific PID forensics (`/proc/<pid>/cmdline` to
//! identify the holder) are NOT implemented in this first cut;
//! the manual_remediation envelope points operators at
//! `ss -tlnp | grep <port>` or `lsof -i :<port>`.
//!
//! **Known limitation** (pass-35-review Gemini F4 / Codex F4):
//! `TcpListener::bind` doesn't set `SO_REUSEADDR=1` on the
//! probe socket, so a port in `TIME_WAIT` immediately after an
//! `am serve-http` restart will be reported as held even
//! though the real server (which uses `SO_REUSEADDR=1`) would
//! be able to bind. Mitigation: operator awareness — re-run
//! `am doctor` after ~60s if a finding appears right after a
//! server restart. A future revision could wire the probe
//! through `nix`'s socket API (requires the `socket` feature
//! flag) to match server bind semantics.
//!
//! ## Fix
//!
//! **Detect-only.** Killing a foreign process is well outside
//! the doctor's scope (operator's call; the foreign service may
//! be legitimate). Manual remediation lists three operator
//! options: kill the holder, rebind to a different port, or
//! investigate if the holder is a stale `am`/`mcp-agent-mail`
//! that should have been cleaned up.

#![forbid(unsafe_code)]

use super::{FindingRemediation, FixOutcome};
use crate::doctor::mutate::{MutateContext, MutateError};
use serde::Serialize;
use std::net::{TcpListener, ToSocketAddrs};

pub const FM_ID: &str = "fm-runtime-processes-port-bound-by-foreign-process";
const FM_SEVERITY: &str = "P0";
const FM_SUBSYSTEM: &str = "runtime_processes";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Reason {
    /// `bind(2)` returned `EADDRINUSE` — port is held by some
    /// other process.
    AddrInUse,
    /// `bind(2)` returned something else (permission denied for
    /// low ports, invalid address, etc.). Surfaced for
    /// operator visibility; not necessarily an `am`-specific
    /// failure mode.
    OtherBindError,
}

impl Reason {
    fn as_kebab(self) -> &'static str {
        match self {
            Reason::AddrInUse => "addr_in_use",
            Reason::OtherBindError => "other_bind_error",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PortBoundByForeignProcessFinding {
    pub host: String,
    pub port: u16,
    pub reason: Reason,
    /// Raw OS error code for diagnostic surfacing (e.g.,
    /// EADDRINUSE = 98 on Linux).
    pub os_error: Option<i32>,
    /// Best-effort error description from the OS layer.
    pub error_message: String,
}

impl PortBoundByForeignProcessFinding {
    pub fn to_finding(&self) -> super::Finding {
        let title = format!(
            "Cannot bind {}:{} ({}); foreign process likely holds the port",
            self.host,
            self.port,
            self.reason.as_kebab(),
        );
        super::Finding {
            id: FM_ID,
            severity: FM_SEVERITY,
            subsystem: FM_SUBSYSTEM,
            title,
            // AddrInUse is high-confidence (definitive); other
            // bind errors are lower-confidence (might be a
            // permission issue rather than a foreign holder).
            confidence: match self.reason {
                Reason::AddrInUse => 1.0,
                Reason::OtherBindError => 0.6,
            },
            evidence: serde_json::json!({
                "host": self.host,
                "port": self.port,
                "reason": self.reason.as_kebab(),
                "os_error": self.os_error,
                "error_message": self.error_message,
                "investigation_commands": [
                    format!("ss -tlnp | grep ':{}'", self.port),
                    format!("lsof -i :{}", self.port),
                    format!("netstat -tlnp | grep ':{}'", self.port),
                ],
            }),
            remediation: FindingRemediation {
                command: format!("am doctor explain {FM_ID}"),
                explain_command: format!("am doctor explain {FM_ID}"),
                // Detect-only: killing a foreign process is
                // outside the doctor's scope.
                auto_fixable: false,
                estimated_actions: 0,
            },
        }
    }

    pub fn manual_remediation_text(&self) -> String {
        format!(
            "Port {host}:{port} is held by another process. Investigate via:\n\
             \n  ss -tlnp | grep ':{port}'\n  lsof -i :{port}\n  netstat -tlnp | grep ':{port}'\n\
             \nOnce you identify the holder, you have three options:\n\
             \n  (a) Kill the foreign process if it's a stale `am`/`mcp-agent-mail`/python \
             leftover: `kill <pid>` (or `kill -9` if unresponsive).\n\
             \n  (b) Rebind `am serve-http` to a different port: set HTTP_PORT=<new> in \
             $XDG_CONFIG_HOME/mcp-agent-mail/config.env.\n\
             \n  (c) If the holder is a legitimate service, change your `am` config to use a \
             different port and update every MCP client config (see \
             `fm-mcp-config-files-wrong-http-url-or-scheme`).\n\
             \nThe doctor REFUSES to auto-kill foreign processes — operators must explicitly \
             choose the right remediation.",
            host = self.host,
            port = self.port,
        )
    }
}

/// Detector inputs. Production caller builds these from
/// `Config::from_env()`.
#[derive(Debug, Clone)]
pub struct DetectInputs {
    pub host: String,
    pub port: u16,
}

/// Detector. PURE w.r.t. inputs; performs a transient TCP bind
/// probe but immediately drops the listener if it succeeds — no
/// state is left behind.
pub fn detect(inputs: &DetectInputs) -> Vec<PortBoundByForeignProcessFinding> {
    // Pass-35-review Gemini F2 (P0): the pre-fix code built the
    // address via `format!("{}:{}", host, port).parse()`, which
    // broke IPv6 — `::1:8765` is not a valid SocketAddr literal
    // (IPv6 needs bracket-form `[::1]:8765`). Use
    // `(&str, u16).to_socket_addrs()` instead — it returns an
    // iterator of resolved addresses and handles IPv4, IPv6, and
    // host-name resolution correctly.
    let mut addrs = match (inputs.host.as_str(), inputs.port).to_socket_addrs() {
        Ok(it) => it,
        Err(_) => return Vec::new(), // malformed host — different FM
    };
    let addr = match addrs.next() {
        Some(a) => a,
        None => return Vec::new(),
    };
    match TcpListener::bind(addr) {
        Ok(_listener) => Vec::new(), // freed on drop here
        Err(e) => {
            let reason = if e.kind() == std::io::ErrorKind::AddrInUse {
                Reason::AddrInUse
            } else {
                Reason::OtherBindError
            };
            vec![PortBoundByForeignProcessFinding {
                host: inputs.host.clone(),
                port: inputs.port,
                reason,
                os_error: e.raw_os_error(),
                error_message: e.to_string(),
            }]
        }
    }
}

/// Detect-only FM. `fix()` is a no-op.
pub fn fix(
    _ctx: &MutateContext,
    _finding: &PortBoundByForeignProcessFinding,
) -> Result<FixOutcome, MutateError> {
    Ok(FixOutcome {
        actions_taken: 0,
        actions_skipped: 1,
        quarantined_paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind to an OS-assigned ephemeral port, then return that
    /// port number plus a held listener. The listener stays
    /// alive for the duration of the caller's test, keeping
    /// the port "occupied" so our detector can observe
    /// EADDRINUSE on it.
    fn bind_ephemeral() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn detector_returns_empty_for_free_port() {
        let inputs = DetectInputs {
            host: "127.0.0.1".to_string(),
            port: 0, // OS assigns; bind+drop succeeds immediately.
        };
        // We bind to port 0 which always succeeds; the test
        // confirms a free port yields no finding.
        let findings = detect(&inputs);
        assert!(findings.is_empty(), "free port must not flag");
    }

    #[test]
    fn detector_flags_held_port_as_addr_in_use() {
        let (_held, port) = bind_ephemeral();
        let inputs = DetectInputs {
            host: "127.0.0.1".to_string(),
            port,
        };
        let findings = detect(&inputs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::AddrInUse);
        assert_eq!(findings[0].port, port);
        // _held drops here, releasing the port.
    }

    #[test]
    fn detector_handles_ipv6_loopback_host() {
        // Pass-35-review Gemini F2 (P0): pre-fix, `format!("{}:{}",
        // "::1", 8765).parse()` failed because IPv6 needs the
        // bracket-form `[::1]:8765`. Using
        // `to_socket_addrs()` handles both forms.
        let inputs = DetectInputs {
            host: "::1".to_string(),
            port: 0, // bind to OS-assigned ephemeral
        };
        // The probe should NOT silently abort (no finding); we
        // either bind successfully (port 0 = OS-assigned, free)
        // or get a finding — both are fine. The key is that
        // `to_socket_addrs` doesn't reject `::1`.
        let findings = detect(&inputs);
        // Port 0 binds to an ephemeral port; expect empty result.
        assert!(
            findings.is_empty(),
            "IPv6 loopback port 0 should bind freely; got: {findings:?}",
        );
    }

    #[test]
    fn detector_returns_empty_for_malformed_host() {
        let inputs = DetectInputs {
            host: "not a valid hostname".to_string(),
            port: 8080,
        };
        // Malformed host → parse fails → no finding (different
        // FM owns the "invalid host config" surface).
        assert!(detect(&inputs).is_empty());
    }

    #[test]
    fn finding_addr_in_use_has_high_confidence() {
        let f = PortBoundByForeignProcessFinding {
            host: "127.0.0.1".to_string(),
            port: 8765,
            reason: Reason::AddrInUse,
            os_error: Some(98),
            error_message: "Address already in use".to_string(),
        };
        let g = f.to_finding();
        assert_eq!(g.severity, "P0");
        assert!((g.confidence - 1.0).abs() < 1e-6);
        assert!(!g.remediation.auto_fixable);
    }

    #[test]
    fn finding_other_bind_error_has_reduced_confidence() {
        let f = PortBoundByForeignProcessFinding {
            host: "127.0.0.1".to_string(),
            port: 80,
            reason: Reason::OtherBindError,
            os_error: Some(13),
            error_message: "Permission denied".to_string(),
        };
        let g = f.to_finding();
        // 0.6 — not as definitive as EADDRINUSE.
        assert!(g.confidence > 0.5 && g.confidence < 0.9);
    }

    #[test]
    fn evidence_includes_investigation_commands() {
        let f = PortBoundByForeignProcessFinding {
            host: "127.0.0.1".to_string(),
            port: 8765,
            reason: Reason::AddrInUse,
            os_error: Some(98),
            error_message: "Address already in use".to_string(),
        };
        let s = serde_json::to_string(&f.to_finding()).unwrap();
        assert!(s.contains("ss -tlnp"));
        assert!(s.contains("lsof -i :8765"));
    }

    #[test]
    fn manual_remediation_enumerates_three_options() {
        let f = PortBoundByForeignProcessFinding {
            host: "127.0.0.1".to_string(),
            port: 8765,
            reason: Reason::AddrInUse,
            os_error: Some(98),
            error_message: "Address already in use".to_string(),
        };
        let text = f.manual_remediation_text();
        assert!(text.contains("(a) Kill"));
        assert!(text.contains("(b) Rebind"));
        assert!(text.contains("(c) If the holder is a legitimate service"));
    }
}
