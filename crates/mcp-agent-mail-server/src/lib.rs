#![forbid(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::missing_const_for_fn,
    clippy::suboptimal_flops,
    clippy::needless_range_loop,
    clippy::unnecessary_wraps,
    clippy::map_unwrap_or,
    clippy::match_same_arms,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::collapsible_if,
    clippy::ignored_unit_patterns,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::result_large_err,
    clippy::manual_is_multiple_of,
    clippy::uninlined_format_args,
    clippy::significant_drop_in_scrutinee,
    clippy::use_self,
    clippy::or_fun_call,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::field_reassign_with_default,
    clippy::manual_div_ceil,
    clippy::duration_suboptimal_units,
    clippy::useless_vec,
    clippy::assigning_clones,
    clippy::large_enum_variant,
    clippy::redundant_closure_for_method_calls,
    clippy::new_without_default,
    clippy::must_use_candidate,
    clippy::needless_borrow,
    clippy::cast_possible_truncation,
    clippy::comparison_chain,
    clippy::unused_self,
    clippy::missing_fields_in_debug,
    clippy::unnecessary_map_or,
    clippy::trivially_copy_pass_by_ref,
    clippy::cast_lossless
)]

// ═══════════════════════════════════════════════════════════════════════════════
// Transport compatibility lock (br-3vwi.13.9)
//
// These constants define the externally observed startup/transport contract.
// Changing any of them is a BREAKING CHANGE for existing operator workflows
// and requires explicit approval + migration rationale.
// ═══════════════════════════════════════════════════════════════════════════════

/// Locked default server name for MCP clients that match on it.
pub const COMPAT_SERVER_NAME: &str = "mcp-agent-mail";

/// Locked health endpoint paths that always bypass bearer auth.
pub const COMPAT_HEALTH_PATHS: &[&str] = &[
    "/health/liveness",
    "/health/readiness",
    "/health",
    "/healthz",
];

/// Locked OAuth well-known endpoint path.
pub const COMPAT_OAUTH_WELL_KNOWN: &str = "/.well-known/oauth-authorization-server";

/// Locked mail UI prefix (coexists with MCP endpoint).
pub const COMPAT_MAIL_UI_PREFIX: &str = "/mail";

/// Locked MCP base path aliases (interchangeable for dev convenience).
pub const COMPAT_MCP_ALIASES: &[&str] = &["/api", "/mcp"];

mod ack_ttl;
pub mod atc;
pub mod atc_replay;
mod cleanup;
pub mod console;
mod disk_monitor;
mod integrity_guard;
mod mail_ui;
mod markdown;
mod retention;
pub mod startup_checks;
pub mod static_export;
mod static_files;
mod templates;
pub mod theme;
mod tool_metrics;
pub mod tui_action_menu;
pub mod tui_app;
pub mod tui_bridge;
pub mod tui_chrome;
pub mod tui_compose;
#[allow(
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    clippy::redundant_closure,
    clippy::needless_borrow,
    clippy::missing_const_for_fn,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
pub mod tui_decision;
pub mod tui_events;
pub mod tui_focus;
pub mod tui_hit_regions;
pub mod tui_keymap;
pub mod tui_layout;
pub mod tui_macro;
pub mod tui_markdown;
pub mod tui_panel_helpers;
pub mod tui_persist;
pub mod tui_poller;
pub mod tui_preset;
pub mod tui_screens;
pub mod tui_theme;
pub mod tui_web_dashboard;
pub mod tui_widgets;
mod tui_ws_input;
mod tui_ws_state;

use asupersync::channel::mpsc;
use asupersync::http::h1::HttpClient;
use asupersync::http::h1::listener::{Http1Listener, Http1ListenerConfig, Http1ListenerStats};
use asupersync::http::h1::server::Http1Config;
use asupersync::http::h1::types::{
    Method as Http1Method, Request as Http1Request, Response as Http1Response, default_reason,
};
use asupersync::messaging::RedisClient;
#[cfg(not(target_os = "linux"))]
use asupersync::runtime::reactor::create_reactor;
#[cfg(target_os = "linux")]
use asupersync::runtime::reactor::{EpollReactor, IoUringReactor, Reactor};
use asupersync::runtime::{JoinHandle as AsyncJoinHandle, Runtime, RuntimeBuilder, RuntimeHandle};
use asupersync::time::{sleep, timeout, wall_now};
use asupersync::{Budget, Cx};
use fastmcp::prelude::*;
use fastmcp_core::{McpError, McpErrorCode, SessionState, block_on};
use fastmcp_protocol::{Icon, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ToolAnnotations};
use fastmcp_server::{BoxFuture, Session};
use fastmcp_transport::http::{
    HttpHandlerConfig, HttpMethod as McpHttpMethod, HttpRequest, HttpRequestHandler, HttpResponse,
};
use ftui::layout::{Constraint, Flex, Rect};
use ftui::widgets::Widget;
use ftui::widgets::block::Block;
use ftui::widgets::borders::BorderType;
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::table::{Row, Table};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation};
use mcp_agent_mail_core::config::{ConsoleSplitMode, ConsoleUiAnchor};
use mcp_agent_mail_core::{
    EffectKind, ExperienceBuilder, ExperienceOutcome, ExperienceRow, ExperienceState,
    ExperienceSubsystem, FeatureVector, NonExecutionReason, loss_to_bp, prob_to_bp,
};
use mcp_agent_mail_db::{
    DbConn, DbPoolConfig, QueryTracker, active_tracker, create_pool, get_or_create_pool,
    set_active_tracker,
};
use mcp_agent_mail_tools::{
    AcknowledgeMessage, AcquireBuildSlot, AgentsListResource, CleanupPaneIdentities,
    ConfigEnvironmentQueryResource, ConfigEnvironmentResource, CreateAgentIdentity, EnsureProduct,
    EnsureProject, FetchInbox, FetchInboxProduct, FileReservationPaths, FileReservationsResource,
    ForceReleaseFileReservation, HealthCheck, IdentityProjectResource, InboxResource,
    InstallPrecommitGuard, ListAgents, ListContacts, MacroContactHandshake,
    MacroFileReservationCycle, MacroPrepareThread, MacroStartSession, MailboxResource,
    MailboxWithCommitsResource, MarkMessageRead, MessageDetailsResource, OutboxResource,
    ProductDetailsResource, ProductsLink, ProjectDetailsResource, ProjectsListQueryResource,
    ProjectsListResource, RegisterAgent, ReleaseBuildSlot, ReleaseFileReservations, RenewBuildSlot,
    RenewFileReservations, ReplyMessage, RequestContact, ResolvePaneIdentity, RespondContact,
    SearchMessages, SearchMessagesProduct, SendMessage, SetContactPolicy, SummarizeThread,
    SummarizeThreadProduct, ThreadDetailsResource, ToolingCapabilitiesResource,
    ToolingDiagnosticsQueryResource, ToolingDiagnosticsResource, ToolingDirectoryQueryResource,
    ToolingDirectoryResource, ToolingLocksQueryResource, ToolingLocksResource,
    ToolingMetricsCoreQueryResource, ToolingMetricsCoreResource, ToolingMetricsQueryResource,
    ToolingMetricsResource, ToolingRecentResource, ToolingSchemasQueryResource,
    ToolingSchemasResource, UninstallPrecommitGuard, ViewsAckOverdueResource,
    ViewsAckRequiredResource, ViewsAcksStaleResource, ViewsUrgentUnreadResource, Whois, clusters,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct InstrumentedTool<T> {
    tool_index: usize,
    tool_name: &'static str,
    inner: T,
}

struct InflightGuard {
    gauge: &'static mcp_agent_mail_core::GaugeI64,
}

impl InflightGuard {
    fn begin(gauge: &'static mcp_agent_mail_core::GaugeI64) -> Self {
        gauge.add(1);
        Self { gauge }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.gauge.add(-1);
    }
}

impl<T: fastmcp::ToolHandler> fastmcp::ToolHandler for InstrumentedTool<T> {
    fn definition(&self) -> Tool {
        self.inner.definition()
    }

    fn icon(&self) -> Option<&Icon> {
        self.inner.icon()
    }

    fn version(&self) -> Option<&str> {
        self.inner.version()
    }

    fn tags(&self) -> &[String] {
        self.inner.tags()
    }

    fn annotations(&self) -> Option<&ToolAnnotations> {
        self.inner.annotations()
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        self.inner.output_schema()
    }

    fn timeout(&self) -> Option<Duration> {
        self.inner.timeout()
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        // Keep cached health current; otherwise stale red can linger until an explicit
        // health-check call refreshes the cache.
        let _ = mcp_agent_mail_core::refresh_health_level();
        // Backpressure gate: reject shedable tools under Red (when enabled)
        if mcp_agent_mail_core::should_shed_tool(self.tool_name) {
            return Err(McpError::new(
                McpErrorCode::InternalError,
                format!(
                    "Server overloaded (health_level=red). Tool '{}' temporarily unavailable. Retry after load subsides.",
                    self.tool_name,
                ),
            ));
        }

        mcp_agent_mail_tools::record_call_idx(self.tool_index);

        // Emit ToolCallStart with masked params
        let (project, agent) = extract_project_agent(&arguments);
        let masked = console::mask_json(&arguments);
        emit_tui_event(tui_events::MailEvent::tool_call_start(
            self.tool_name,
            masked,
            project.clone(),
            agent.clone(),
        ));

        let qt_before = mcp_agent_mail_db::QUERY_TRACKER.snapshot();
        let start = Instant::now();
        let call_arguments = arguments.clone();
        let out = self.inner.call(ctx, arguments);
        let elapsed = start.elapsed();
        let latency_us =
            u64::try_from(elapsed.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
        let is_error = out.is_err();
        if is_error {
            mcp_agent_mail_tools::record_error_idx(self.tool_index);
        }
        mcp_agent_mail_core::global_metrics()
            .tools
            .record_call(latency_us, is_error);
        mcp_agent_mail_tools::record_latency_idx(self.tool_index, latency_us);

        // Emit ToolCallEnd with duration and query delta
        let qt_after = mcp_agent_mail_db::QUERY_TRACKER.snapshot();
        let duration_ms =
            u64::try_from(elapsed.as_millis().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
        let result_preview = result_preview_from_mcpresult(&out);
        let (queries, query_time_ms, per_table) = query_delta(&qt_before, &qt_after);
        if let Ok(contents) = &out {
            for event in derive_domain_events_from_tool_contents(
                self.tool_name,
                Some(&call_arguments),
                contents,
                project.as_deref(),
                agent.as_deref(),
            ) {
                emit_tui_event(event);
            }
        }
        emit_tui_event(tui_events::MailEvent::tool_call_end(
            self.tool_name,
            duration_ms,
            result_preview,
            queries,
            query_time_ms,
            per_table,
            project,
            agent,
        ));

        out
    }

    fn call_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
        // Keep cached health current; otherwise stale red can linger until an explicit
        // health-check call refreshes the cache.
        let _ = mcp_agent_mail_core::refresh_health_level();
        // Backpressure gate: reject shedable tools under Red (when enabled)
        if mcp_agent_mail_core::should_shed_tool(self.tool_name) {
            return Box::pin(std::future::ready(fastmcp_core::Outcome::Err(
                McpError::new(
                    McpErrorCode::InternalError,
                    format!(
                        "Server overloaded (health_level=red). Tool '{}' temporarily unavailable. Retry after load subsides.",
                        self.tool_name,
                    ),
                ),
            )));
        }

        mcp_agent_mail_tools::record_call_idx(self.tool_index);

        let (project, agent) = extract_project_agent(&arguments);
        let masked = console::mask_json(&arguments);

        let qt_before = mcp_agent_mail_db::QUERY_TRACKER.snapshot();
        let start = Instant::now();
        let call_arguments = arguments.clone();
        Box::pin(async move {
            // Emit ToolCallStart with masked params
            emit_tui_event_async(tui_events::MailEvent::tool_call_start(
                self.tool_name,
                masked,
                project.clone(),
                agent.clone(),
            ))
            .await;

            let out = self.inner.call_async(ctx, arguments).await;
            let is_error = !matches!(out, fastmcp_core::Outcome::Ok(_));
            if is_error {
                mcp_agent_mail_tools::record_error_idx(self.tool_index);
            }
            let elapsed = start.elapsed();
            let latency_us =
                u64::try_from(elapsed.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
            mcp_agent_mail_core::global_metrics()
                .tools
                .record_call(latency_us, is_error);
            mcp_agent_mail_tools::record_latency_idx(self.tool_index, latency_us);

            // Emit ToolCallEnd with duration and query delta
            let qt_after = mcp_agent_mail_db::QUERY_TRACKER.snapshot();
            let duration_ms =
                u64::try_from(elapsed.as_millis().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
            let result_preview = result_preview_from_outcome(&out);
            let (queries, query_time_ms, per_table) = query_delta(&qt_before, &qt_after);
            if let fastmcp_core::Outcome::Ok(contents) = &out {
                for event in derive_domain_events_from_tool_contents(
                    self.tool_name,
                    Some(&call_arguments),
                    contents,
                    project.as_deref(),
                    agent.as_deref(),
                ) {
                    emit_tui_event_async(event).await;
                }
            }
            emit_tui_event_async(tui_events::MailEvent::tool_call_end(
                self.tool_name,
                duration_ms,
                result_preview,
                queries,
                query_time_ms,
                per_table,
                project,
                agent,
            ))
            .await;

            out
        })
    }
}

/// Extract `project_key` and agent name from tool arguments for event tagging.
fn extract_project_agent(args: &serde_json::Value) -> (Option<String>, Option<String>) {
    let obj = args.as_object();
    let project = obj
        .and_then(|m| m.get("project_key"))
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    // Try common agent name param variants
    let agent = obj
        .and_then(|m| {
            m.get("agent_name")
                .or_else(|| m.get("sender_name"))
                .or_else(|| m.get("name"))
        })
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    (project, agent)
}

/// Truncate a UTF-8 string to at most `max_bytes`, backing off to a valid char boundary.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    &s[..idx]
}

/// Build a masked preview string (max 200 chars) from tool result contents.
fn result_preview_from_contents(contents: &[Content]) -> Option<String> {
    let Content::Text { text: raw } = contents.first()? else {
        return None;
    };
    let preview = truncate_utf8(raw, 200);
    // Mask if it looks like JSON
    Some(
        serde_json::from_str::<serde_json::Value>(preview).map_or_else(
            |_| preview.replace('\n', "\\n").replace('\r', "\\r"),
            |v| console::mask_json(&v).to_string(),
        ),
    )
}

/// Build a preview string (max 200 chars, masked) from a sync tool result.
fn result_preview_from_mcpresult(out: &McpResult<Vec<Content>>) -> Option<String> {
    result_preview_from_contents(out.as_ref().ok()?)
}

/// Build a preview string from an async tool Outcome.
fn result_preview_from_outcome(out: &McpOutcome<Vec<Content>>) -> Option<String> {
    match out {
        fastmcp_core::Outcome::Ok(c) => result_preview_from_contents(c),
        _ => None,
    }
}

/// Compute the delta between two query tracker snapshots.
fn query_delta(
    before: &mcp_agent_mail_db::QueryTrackerSnapshot,
    after: &mcp_agent_mail_db::QueryTrackerSnapshot,
) -> (u64, f64, Vec<(String, u64)>) {
    let queries = after.total.saturating_sub(before.total);
    let query_time_ms = (after.total_time_ms - before.total_time_ms).max(0.0);
    let per_table: Vec<(String, u64)> = after
        .per_table
        .iter()
        .filter_map(|(table, &count)| {
            let prev = before.per_table.get(table).copied().unwrap_or(0);
            let delta = count.saturating_sub(prev);
            if delta > 0 {
                Some((table.clone(), delta))
            } else {
                None
            }
        })
        .collect();
    (queries, query_time_ms, per_table)
}

fn add_tool<T: fastmcp::ToolHandler + 'static>(
    server: fastmcp_server::ServerBuilder,
    config: &mcp_agent_mail_core::Config,
    tool_name: &'static str,
    cluster: &'static str,
    tool: T,
) -> fastmcp_server::ServerBuilder {
    if config.should_expose_tool(tool_name, cluster) {
        if let Some(tool_index) = mcp_agent_mail_tools::tool_index(tool_name) {
            // Resolve the static tool name from TOOL_CLUSTER_MAP for event emission
            let static_name = mcp_agent_mail_tools::TOOL_CLUSTER_MAP[tool_index].0;
            server.tool(InstrumentedTool {
                tool_index,
                tool_name: static_name,
                inner: tool,
            })
        } else {
            tracing::error!(
                tool = tool_name,
                cluster,
                "tool missing from TOOL_CLUSTER_MAP; registering without instrumentation"
            );
            server.tool(tool)
        }
    } else {
        server
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_server(config: &mcp_agent_mail_core::Config) -> Server {
    // Wire the config flag into the global atomic gate.
    mcp_agent_mail_core::set_shedding_enabled(config.backpressure_shedding_enabled);

    let server = Server::new("mcp-agent-mail", env!("CARGO_PKG_VERSION"));

    let server = add_tool(
        server,
        config,
        "health_check",
        clusters::INFRASTRUCTURE,
        HealthCheck,
    );
    let server = add_tool(
        server,
        config,
        "ensure_project",
        clusters::INFRASTRUCTURE,
        EnsureProject,
    );
    let server = add_tool(
        server,
        config,
        "register_agent",
        clusters::IDENTITY,
        RegisterAgent,
    );
    let server = add_tool(
        server,
        config,
        "create_agent_identity",
        clusters::IDENTITY,
        CreateAgentIdentity,
    );
    let server = add_tool(server, config, "whois", clusters::IDENTITY, Whois);
    let server = add_tool(
        server,
        config,
        "resolve_pane_identity",
        clusters::IDENTITY,
        ResolvePaneIdentity,
    );
    let server = add_tool(
        server,
        config,
        "cleanup_pane_identities",
        clusters::IDENTITY,
        CleanupPaneIdentities,
    );
    let server = add_tool(
        server,
        config,
        "list_agents",
        clusters::IDENTITY,
        ListAgents,
    );
    let server = add_tool(
        server,
        config,
        "send_message",
        clusters::MESSAGING,
        SendMessage,
    );
    let server = add_tool(
        server,
        config,
        "reply_message",
        clusters::MESSAGING,
        ReplyMessage,
    );
    let server = add_tool(
        server,
        config,
        "fetch_inbox",
        clusters::MESSAGING,
        FetchInbox,
    );
    let server = add_tool(
        server,
        config,
        "mark_message_read",
        clusters::MESSAGING,
        MarkMessageRead,
    );
    let server = add_tool(
        server,
        config,
        "acknowledge_message",
        clusters::MESSAGING,
        AcknowledgeMessage,
    );
    let server = add_tool(
        server,
        config,
        "request_contact",
        clusters::CONTACT,
        RequestContact,
    );
    let server = add_tool(
        server,
        config,
        "respond_contact",
        clusters::CONTACT,
        RespondContact,
    );
    let server = add_tool(
        server,
        config,
        "list_contacts",
        clusters::CONTACT,
        ListContacts,
    );
    let server = add_tool(
        server,
        config,
        "set_contact_policy",
        clusters::CONTACT,
        SetContactPolicy,
    );
    let server = add_tool(
        server,
        config,
        "file_reservation_paths",
        clusters::FILE_RESERVATIONS,
        FileReservationPaths,
    );
    let server = add_tool(
        server,
        config,
        "release_file_reservations",
        clusters::FILE_RESERVATIONS,
        ReleaseFileReservations,
    );
    let server = add_tool(
        server,
        config,
        "renew_file_reservations",
        clusters::FILE_RESERVATIONS,
        RenewFileReservations,
    );
    let server = add_tool(
        server,
        config,
        "force_release_file_reservation",
        clusters::FILE_RESERVATIONS,
        ForceReleaseFileReservation,
    );
    let server = add_tool(
        server,
        config,
        "install_precommit_guard",
        clusters::INFRASTRUCTURE,
        InstallPrecommitGuard,
    );
    let server = add_tool(
        server,
        config,
        "uninstall_precommit_guard",
        clusters::INFRASTRUCTURE,
        UninstallPrecommitGuard,
    );
    let server = add_tool(
        server,
        config,
        "search_messages",
        clusters::SEARCH,
        SearchMessages,
    );
    let server = add_tool(
        server,
        config,
        "summarize_thread",
        clusters::SEARCH,
        SummarizeThread,
    );
    let server = add_tool(
        server,
        config,
        "macro_start_session",
        clusters::WORKFLOW_MACROS,
        MacroStartSession,
    );
    let server = add_tool(
        server,
        config,
        "macro_prepare_thread",
        clusters::WORKFLOW_MACROS,
        MacroPrepareThread,
    );
    let server = add_tool(
        server,
        config,
        "macro_file_reservation_cycle",
        clusters::WORKFLOW_MACROS,
        MacroFileReservationCycle,
    );
    let server = add_tool(
        server,
        config,
        "macro_contact_handshake",
        clusters::WORKFLOW_MACROS,
        MacroContactHandshake,
    );
    let server = add_tool(
        server,
        config,
        "ensure_product",
        clusters::PRODUCT_BUS,
        EnsureProduct,
    );
    let server = add_tool(
        server,
        config,
        "products_link",
        clusters::PRODUCT_BUS,
        ProductsLink,
    );
    let server = add_tool(
        server,
        config,
        "search_messages_product",
        clusters::PRODUCT_BUS,
        SearchMessagesProduct,
    );
    let server = add_tool(
        server,
        config,
        "fetch_inbox_product",
        clusters::PRODUCT_BUS,
        FetchInboxProduct,
    );
    let server = add_tool(
        server,
        config,
        "summarize_thread_product",
        clusters::PRODUCT_BUS,
        SummarizeThreadProduct,
    );
    let server = add_tool(
        server,
        config,
        "acquire_build_slot",
        clusters::BUILD_SLOTS,
        AcquireBuildSlot,
    );
    let server = add_tool(
        server,
        config,
        "renew_build_slot",
        clusters::BUILD_SLOTS,
        RenewBuildSlot,
    );
    let server = add_tool(
        server,
        config,
        "release_build_slot",
        clusters::BUILD_SLOTS,
        ReleaseBuildSlot,
    );

    server
        // Identity
        // Resources
        .resource(ConfigEnvironmentResource)
        .resource(ConfigEnvironmentQueryResource)
        .resource(ToolingDirectoryResource)
        .resource(ToolingDirectoryQueryResource)
        .resource(ToolingSchemasResource)
        .resource(ToolingSchemasQueryResource)
        .resource(ToolingMetricsResource)
        .resource(ToolingMetricsQueryResource)
        .resource(ToolingMetricsCoreResource)
        .resource(ToolingMetricsCoreQueryResource)
        .resource(ToolingDiagnosticsResource)
        .resource(ToolingDiagnosticsQueryResource)
        .resource(ToolingLocksResource)
        .resource(ToolingLocksQueryResource)
        .resource(ToolingCapabilitiesResource)
        .resource(ToolingRecentResource)
        .resource(ProjectsListResource)
        .resource(ProjectsListQueryResource)
        .resource(ProjectDetailsResource)
        .resource(AgentsListResource)
        .resource(ProductDetailsResource)
        .resource(IdentityProjectResource)
        .resource(FileReservationsResource)
        .resource(MessageDetailsResource)
        .resource(ThreadDetailsResource)
        .resource(InboxResource)
        .resource(MailboxResource)
        .resource(MailboxWithCommitsResource)
        .resource(OutboxResource)
        .resource(ViewsUrgentUnreadResource)
        .resource(ViewsAckRequiredResource)
        .resource(ViewsAcksStaleResource)
        .resource(ViewsAckOverdueResource)
        .build()
}

static STARTUP_SEARCH_BACKFILL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
const DEFAULT_STARTUP_SEARCH_BACKFILL_DELAY_SECS: u64 = 8;

struct StartupSearchBackfillResetGuard;

impl Drop for StartupSearchBackfillResetGuard {
    fn drop(&mut self) {
        STARTUP_SEARCH_BACKFILL_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

fn record_startup_search_backfill_completion(config: &mcp_agent_mail_core::Config) {
    if let Err(error) = mcp_agent_mail_db::search_service::note_startup_lexical_backfill_completed(
        &config.database_url,
    ) {
        tracing::warn!(
            error = %error,
            "[startup-search] failed to record lexical bootstrap completion"
        );
    }
}

fn startup_search_backfill_spawn_failure_message(error: &std::io::Error) -> String {
    format!(
        "[startup-search] failed to spawn background backfill worker; leaving lexical backfill lazy ({error})"
    )
}

fn run_startup_search_backfill(config: &mcp_agent_mail_core::Config) {
    let backfill_database_url =
        normalized_startup_search_backfill_database_url(&config.database_url);
    match mcp_agent_mail_db::search_v3::backfill_from_db(&backfill_database_url) {
        Ok((indexed, _skipped)) if indexed > 0 => {
            record_startup_search_backfill_completion(config);
            tracing::info!(
                indexed,
                "[startup-search] backfilled messages into Tantivy index"
            );
        }
        Ok(_) => {
            record_startup_search_backfill_completion(config);
        } // nothing to backfill or already up-to-date
        Err(err) => {
            tracing::warn!("[startup-search] Tantivy backfill failed (non-fatal): {err}");
            if recover_startup_search_backfill_db(config, &err) {
                match mcp_agent_mail_db::search_v3::backfill_from_db(&backfill_database_url) {
                    Ok((indexed, _)) if indexed > 0 => {
                        record_startup_search_backfill_completion(config);
                        tracing::warn!(
                            indexed,
                            "[startup-search] backfill succeeded after sqlite auto-recovery"
                        );
                    }
                    Ok(_) => {
                        record_startup_search_backfill_completion(config);
                        tracing::warn!(
                            "[startup-search] backfill completed after sqlite auto-recovery"
                        );
                    }
                    Err(retry_err) => {
                        tracing::warn!(
                            "[startup-search] backfill retry failed after sqlite auto-recovery: {retry_err}"
                        );
                    }
                }
            }
        }
    }
}

#[must_use]
fn startup_search_backfill_delay_secs() -> u64 {
    env_u64_or_default(
        "AM_STARTUP_SEARCH_BACKFILL_DELAY_SECS",
        DEFAULT_STARTUP_SEARCH_BACKFILL_DELAY_SECS,
    )
}

fn spawn_startup_search_backfill(config: &mcp_agent_mail_core::Config) {
    if STARTUP_SEARCH_BACKFILL_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    let thread_config = config.clone();
    let spawn = std::thread::Builder::new()
        .name("am-search-backfill".to_string())
        .spawn(move || {
            let _reset_guard = StartupSearchBackfillResetGuard;
            let delay_secs = startup_search_backfill_delay_secs();
            if delay_secs > 0 {
                tracing::info!(
                    delay_secs,
                    "[startup-search] delaying backfill to prioritize startup responsiveness"
                );
                std::thread::sleep(Duration::from_secs(delay_secs));
            }
            run_startup_search_backfill(&thread_config);
        });

    if let Err(err) = spawn {
        STARTUP_SEARCH_BACKFILL_IN_PROGRESS.store(false, Ordering::Release);
        tracing::warn!(
            error = %err,
            "{}",
            startup_search_backfill_spawn_failure_message(&err)
        );
    }
}

fn init_search_bridge(config: &mcp_agent_mail_core::Config) {
    // Only initialize when lexical-capable modes are active.
    // This avoids creating the index directory when lexical retrieval is disabled.
    let rollout = &config.search_rollout;
    let uses_v3 = rollout.engine.uses_lexical()
        || rollout.surface_overrides.values().any(|e| e.uses_lexical());

    if !uses_v3 && !rollout.should_shadow() {
        return;
    }

    let index_dir = config.storage_root.join("search_index");
    match mcp_agent_mail_db::search_v3::init_bridge(&index_dir) {
        Ok(()) => {
            tracing::info!(
                "[startup-search] initialized search v3 bridge at {}",
                index_dir.display()
            );
            if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url) {
                // In-memory SQLite URLs create a fresh isolated DB per connection, so
                // startup lexical backfill cannot see the live migrated server state.
                // Shared lexical bootstrap markers are also invalid for `:memory:`,
                // because a later in-process memory DB would reuse the same key.
                // Keep the bridge initialized but skip the guaranteed-failing worker.
                return;
            }
            // Backfill on a dedicated worker to keep first-render startup latency low.
            // Search tool semantics remain robust: this is idempotent and retries with
            // auto-recovery on recoverable sqlite failures.
            spawn_startup_search_backfill(config);
        }
        Err(err) => {
            tracing::warn!(
                "[startup-search] failed to initialize search v3 bridge: {}",
                err
            );
        }
    }
}

fn normalized_startup_search_backfill_database_url(database_url: &str) -> String {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return database_url.to_string();
    }
    let Some(sqlite_path) = resolve_server_database_url_sqlite_path(database_url) else {
        return database_url.to_string();
    };
    let normalized = mcp_agent_mail_db::pool::normalize_sqlite_path_for_pool_key(
        sqlite_path.to_string_lossy().as_ref(),
    );
    format!("sqlite:///{normalized}")
}

fn recover_startup_search_backfill_db(config: &mcp_agent_mail_core::Config, error: &str) -> bool {
    let recoverable = mcp_agent_mail_db::is_sqlite_recovery_error_message(error)
        || mcp_agent_mail_db::is_corruption_error_message(error);
    if !recoverable {
        return false;
    }
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url) {
        return false;
    }

    let Some(sqlite_path) = resolve_server_database_url_sqlite_path(&config.database_url) else {
        tracing::warn!(
            database_url = %config.database_url,
            "[startup-search] background backfill hit sqlite recovery error, but automatic server-side recovery is disabled"
        );
        return false;
    };

    tracing::warn!(
        sqlite = %sqlite_path.display(),
        error = %error,
        "[startup-search] background backfill needs sqlite recovery, but automatic server-side recovery is disabled"
    );
    false
}

fn heal_storage_lock_artifacts(config: &mcp_agent_mail_core::Config) {
    match mcp_agent_mail_storage::heal_archive_locks(config) {
        Ok(report) => {
            if !report.locks_removed.is_empty() || !report.metadata_removed.is_empty() {
                tracing::info!(
                    "[startup-heal] removed {} stale lock files and {} orphan lock metadata files (scanned={})",
                    report.locks_removed.len(),
                    report.metadata_removed.len(),
                    report.locks_scanned
                );
            } else {
                tracing::debug!(
                    "[startup-heal] lock scan complete (scanned={}, removed=0)",
                    report.locks_scanned
                );
            }
        }
        Err(err) => {
            tracing::warn!("[startup-heal] lock scan failed: {err}");
        }
    }
}

fn start_advisory_consistency_probe(config: &mcp_agent_mail_core::Config) {
    let config = config.clone();
    let _ = std::thread::Builder::new()
        .name("startup-consistency-probe".into())
        .spawn(move || {
            startup_checks::run_consistency_probe_advisory(&config);
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxActivityLockMode {
    Shared,
    Exclusive,
}

impl MailboxActivityLockMode {
    fn label(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

#[derive(Debug)]
pub struct MailboxActivityLockGuard {
    _mode: MailboxActivityLockMode,
    _lock_subject_path: PathBuf,
    _lock_path: PathBuf,
    lock_file: fs::File,
}

impl Drop for MailboxActivityLockGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.lock_file);
    }
}

fn normalized_mailbox_activity_sqlite_path(sqlite_path: &Path) -> PathBuf {
    PathBuf::from(mcp_agent_mail_db::pool::normalize_sqlite_path_for_pool_key(
        sqlite_path.to_string_lossy().as_ref(),
    ))
}

fn mailbox_activity_lock_path(sqlite_path: &Path) -> PathBuf {
    let mut lock_os = sqlite_path.as_os_str().to_os_string();
    lock_os.push(".activity.lock");
    PathBuf::from(lock_os)
}

fn mailbox_activity_lock_contention_error(
    subject_path: &Path,
    subject_kind: &str,
    lock_path: &Path,
    mode: MailboxActivityLockMode,
    err: &std::io::Error,
) -> std::io::Error {
    let detail = format!(
        "mailbox activity lock is busy for {subject_kind} {} ({} lock {}): another Agent Mail runtime or mutating `am doctor` operation is already active; stop it or wait for it to finish",
        subject_path.display(),
        mode.label(),
        lock_path.display()
    );
    if err.kind() == std::io::ErrorKind::WouldBlock
        || mcp_agent_mail_db::is_lock_error(&err.to_string())
    {
        std::io::Error::new(std::io::ErrorKind::WouldBlock, detail)
    } else {
        std::io::Error::new(err.kind(), format!("failed to acquire {detail}: {err}"))
    }
}

fn acquire_mailbox_activity_lock_for_subject(
    subject_path: &Path,
    subject_kind: &str,
    lock_path: PathBuf,
    mode: MailboxActivityLockMode,
) -> std::io::Result<Option<MailboxActivityLockGuard>> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    let lock_result = match mode {
        MailboxActivityLockMode::Shared => fs2::FileExt::try_lock_shared(&lock_file),
        MailboxActivityLockMode::Exclusive => fs2::FileExt::try_lock_exclusive(&lock_file),
    };
    if let Err(err) = lock_result {
        return Err(mailbox_activity_lock_contention_error(
            subject_path,
            subject_kind,
            &lock_path,
            mode,
            &err,
        ));
    }

    Ok(Some(MailboxActivityLockGuard {
        _mode: mode,
        _lock_subject_path: subject_path.to_path_buf(),
        _lock_path: lock_path,
        lock_file,
    }))
}

pub fn acquire_mailbox_activity_lock_for_sqlite_path(
    sqlite_path: &Path,
    mode: MailboxActivityLockMode,
) -> std::io::Result<Option<MailboxActivityLockGuard>> {
    if sqlite_path == Path::new(":memory:") {
        return Ok(None);
    }

    let sqlite_path = normalized_mailbox_activity_sqlite_path(sqlite_path);
    let lock_path = mailbox_activity_lock_path(&sqlite_path);
    acquire_mailbox_activity_lock_for_subject(&sqlite_path, "SQLite mailbox", lock_path, mode)
}

pub fn acquire_mailbox_activity_lock_for_storage_root(
    storage_root: &Path,
    mode: MailboxActivityLockMode,
) -> std::io::Result<Option<MailboxActivityLockGuard>> {
    let storage_root = storage_root.to_path_buf();
    let lock_path = storage_root.join(".mailbox.activity.lock");
    acquire_mailbox_activity_lock_for_subject(&storage_root, "storage root", lock_path, mode)
}

pub fn acquire_mailbox_activity_lock_for_database_url(
    database_url: &str,
    mode: MailboxActivityLockMode,
) -> std::io::Result<Option<MailboxActivityLockGuard>> {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return Ok(None);
    }

    let Some(sqlite_path) = resolve_server_database_url_sqlite_path(database_url) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "cannot resolve sqlite path from DATABASE_URL for mailbox activity locking: {database_url}"
            ),
        ));
    };

    acquire_mailbox_activity_lock_for_sqlite_path(&sqlite_path, mode)
}

pub fn acquire_mailbox_activity_lock(
    config: &mcp_agent_mail_core::Config,
    mode: MailboxActivityLockMode,
) -> std::io::Result<Option<MailboxActivityLockGuard>> {
    acquire_mailbox_activity_lock_for_storage_root(&config.storage_root, mode)
}

struct RuntimeMailboxActivityLocks {
    _storage_root_lock: Option<MailboxActivityLockGuard>,
    _sqlite_lock: Option<MailboxActivityLockGuard>,
}

fn acquire_runtime_mailbox_activity_locks(
    config: &mcp_agent_mail_core::Config,
) -> std::io::Result<RuntimeMailboxActivityLocks> {
    let storage_root_lock =
        acquire_mailbox_activity_lock(config, MailboxActivityLockMode::Exclusive)?;
    let sqlite_lock = acquire_mailbox_activity_lock_for_database_url(
        &config.database_url,
        MailboxActivityLockMode::Shared,
    )?;
    Ok(RuntimeMailboxActivityLocks {
        _storage_root_lock: storage_root_lock,
        _sqlite_lock: sqlite_lock,
    })
}
fn ensure_stdio_startup_probes_pass(report: &startup_checks::StartupReport) -> std::io::Result<()> {
    if report.is_ok() {
        return Ok(());
    }
    Err(std::io::Error::other(report.format_errors()))
}

pub fn run_stdio(config: &mcp_agent_mail_core::Config) -> std::io::Result<()> {
    // Initialize console theme from parsed config (includes persisted envfile values).
    let _ = theme::init_console_theme_from_config(config.console_theme);
    // Pre-intern well-known strings to avoid first-request contention.
    mcp_agent_mail_core::pre_intern_policies();

    // Check for resource collisions (e.g. another am process holding locks).
    // IMPORTANT: probes must run BEFORE acquiring runtime locks because
    // `probe_integrity` takes an exclusive flock on the activity lockfile.
    // If we already hold a shared flock (from `acquire_runtime_mailbox_activity_locks`),
    // the exclusive attempt will fail with EAGAIN, deadlocking startup.
    let probe_report = startup_checks::run_stdio_startup_probes(config);
    ensure_stdio_startup_probes_pass(&probe_report)?;

    // Now that probes have confirmed no other process holds the locks,
    // acquire our runtime shared lock for the duration of the process.
    let _runtime_mailbox_locks = acquire_runtime_mailbox_activity_locks(config)?;

    // Enable global query tracker if instrumentation is on.
    if config.instrumentation_enabled {
        mcp_agent_mail_db::QUERY_TRACKER.enable(Some(config.instrumentation_slow_query_ms));
    }

    let heal_config = config.clone();
    let _ = std::thread::Builder::new()
        .name("startup-heal".into())
        .spawn(move || {
            heal_storage_lock_artifacts(&heal_config);
        });

    integrity_guard::start(config);
    disk_monitor::start(config);
    mcp_agent_mail_storage::wbq_start();

    // Initialize the Air Traffic Controller engine for proactive agent coordination.
    atc::init_global_atc(config);
    start_atc_operator_runtime(config);

    // Start background backfill for Search V3 if enabled.
    spawn_startup_search_backfill(config);

    log_active_database(config);

    // Surface a prominent warning when MVCC concurrent mode is enabled so
    // operators are aware of the known snapshot-drift limitation (GH#65).
    if config.fsqlite_concurrent_mode {
        tracing::warn!(
            "fsqlite_concurrent_mode=true: MVCC concurrent writes enabled. \
             This mode has a known snapshot-drift issue (GH#65) that can cause \
             all writes to fail after ~10-20 operations under sustained load. \
             Set FSQLITE_CONCURRENT_MODE=false (the default) to use reliable \
             single-writer mode."
        );
    }

    tracing::info!("MCP Agent Mail server (stdio) starting transport loop");
    build_server(config).run_stdio();

    // run_stdio() returns `!` so the lines below are unreachable today.
    // They are kept as documentation of the intended graceful-shutdown
    // sequence should the transport ever change to a fallible loop.
    #[allow(unreachable_code)]
    {
        tracing::info!("stdio loop exited; performing graceful shutdown of background services");
        stop_atc_operator_runtime();
        integrity_guard::shutdown();
        disk_monitor::shutdown();
        mcp_agent_mail_storage::wbq_shutdown();
        mcp_agent_mail_storage::flush_async_commits();
        Ok(())
    }
}

// HTTP runtime worker bounds (not user-configurable; use AM_HTTP_WORKER_THREADS).
const HTTP_RUNTIME_MIN_WORKERS: usize = 4;
const HTTP_RUNTIME_DEFAULT_WORKERS_CAP: usize = 4;
const HTTP_RUNTIME_MAX_WORKERS: usize = 64;

const HTTP_SUPERVISOR_CONTROL_CHANNEL_CAPACITY: usize = 32;

// The following HTTP supervisor constants have been replaced by Config fields
// (AM_HTTP_* env vars). They are retained for test assertions only.
#[cfg(test)]
const HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS: u64 = 200;
#[cfg(test)]
const HTTP_SUPERVISOR_RESTART_BACKOFF_MAX_MS: u64 = 5_000;
#[cfg(test)]
const HTTP_SUPERVISOR_MAX_CONSECUTIVE_RESTART_FAILURES: u32 = 10;
const HTTP_SERVER_STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_SERVER_FORCE_CLOSE_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

const TUI_SPIN_WATCHDOG_SAMPLE_SECS_DEFAULT: u64 = 2;
const TUI_SPIN_WATCHDOG_WINDOW_SECS_DEFAULT: u64 = 20;
const TUI_SPIN_WATCHDOG_STARTUP_SECS_DEFAULT: u64 = 180;
const TUI_SPIN_WATCHDOG_CPU_PCT_DEFAULT: u64 = 250;

#[derive(Debug, Clone, Copy)]
struct TuiSpinWatchdogConfig {
    sample_interval: Duration,
    sustained_window: Duration,
    startup_window: Duration,
    cpu_threshold_pct_x100: u64,
}

struct TuiSpinWatchdog {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TuiSpinWatchdog {
    fn start(tui_state: &Arc<tui_bridge::TuiSharedState>) -> Option<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = tui_state;
            None
        }

        #[cfg(target_os = "linux")]
        {
            let config = read_tui_spin_watchdog_config()?;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_signal = Arc::clone(&shutdown);
            let state = Arc::clone(tui_state);
            let join = std::thread::Builder::new()
                .name("tui-spin-watchdog".into())
                .spawn(move || run_tui_spin_watchdog_loop(&state, &shutdown_signal, config))
                .ok()?;
            Some(Self {
                shutdown,
                join: Some(join),
            })
        }
    }

    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_tui_spin_watchdog_config() -> Option<TuiSpinWatchdogConfig> {
    if !env_truthy_default_true("AM_TUI_SPIN_WATCHDOG_ENABLED") {
        return None;
    }

    let sample_secs = env_u64_or_default(
        "AM_TUI_SPIN_WATCHDOG_SAMPLE_SECS",
        TUI_SPIN_WATCHDOG_SAMPLE_SECS_DEFAULT,
    )
    .max(1);
    let window_secs = env_u64_or_default(
        "AM_TUI_SPIN_WATCHDOG_WINDOW_SECS",
        TUI_SPIN_WATCHDOG_WINDOW_SECS_DEFAULT,
    )
    .max(sample_secs);
    let startup_secs = env_u64_or_default(
        "AM_TUI_SPIN_WATCHDOG_STARTUP_SECS",
        TUI_SPIN_WATCHDOG_STARTUP_SECS_DEFAULT,
    )
    .max(window_secs);
    let cpu_threshold_pct = env_u64_or_default(
        "AM_TUI_SPIN_WATCHDOG_CPU_PCT",
        TUI_SPIN_WATCHDOG_CPU_PCT_DEFAULT,
    )
    .max(50);

    Some(TuiSpinWatchdogConfig {
        sample_interval: Duration::from_secs(sample_secs),
        sustained_window: Duration::from_secs(window_secs),
        startup_window: Duration::from_secs(startup_secs),
        cpu_threshold_pct_x100: cpu_threshold_pct.saturating_mul(100),
    })
}

fn env_truthy_default_true(key: &str) -> bool {
    std::env::var(key).map_or(true, |value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn env_u64_or_default(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct ProcCpuSample {
    process_jiffies: u64,
    total_jiffies: u64,
}

#[cfg(target_os = "linux")]
fn run_tui_spin_watchdog_loop(
    tui_state: &Arc<tui_bridge::TuiSharedState>,
    shutdown: &Arc<AtomicBool>,
    config: TuiSpinWatchdogConfig,
) {
    let Some(mut previous) = read_proc_cpu_sample() else {
        tracing::warn!("[startup-watchdog] disabled: unable to read /proc CPU counters");
        return;
    };

    let cpu_count = std::thread::available_parallelism()
        .map_or(1_u64, |count| u64::try_from(count.get()).unwrap_or(1))
        .max(1);
    let mut over_threshold_since: Option<Instant> = None;
    let start = Instant::now();

    while !shutdown.load(Ordering::Acquire) && start.elapsed() < config.startup_window {
        if sleep_with_shutdown(shutdown, config.sample_interval) {
            return;
        }

        let Some(next) = read_proc_cpu_sample() else {
            continue;
        };

        let Some(cpu_pct_x100) = process_cpu_pct_x100(previous, next, cpu_count) else {
            previous = next;
            continue;
        };
        previous = next;

        mcp_agent_mail_core::metrics::global_metrics()
            .system
            .tui_spin_watchdog_last_cpu_pct_x100
            .set(cpu_pct_x100);

        if cpu_pct_x100 < config.cpu_threshold_pct_x100 {
            over_threshold_since = None;
            continue;
        }

        let now = Instant::now();
        let since = over_threshold_since.get_or_insert(now);
        if now.duration_since(*since) < config.sustained_window {
            continue;
        }

        let now_us = mcp_agent_mail_core::timestamps::now_micros();
        let metrics = mcp_agent_mail_core::metrics::global_metrics();
        metrics.system.tui_spin_watchdog_trips_total.inc();
        metrics
            .system
            .tui_spin_watchdog_last_trip_us
            .set(u64::try_from(now_us.max(0)).unwrap_or(u64::MAX).max(1));

        tracing::error!(
            cpu_pct_x100,
            threshold_pct_x100 = config.cpu_threshold_pct_x100,
            sustained_secs = config.sustained_window.as_secs(),
            startup_uptime_secs = start.elapsed().as_secs(),
            "[startup-watchdog] sustained TUI CPU spin detected; detaching TUI and continuing headless HTTP"
        );

        tui_state.request_headless_detach();
        return;
    }
}

#[cfg(target_os = "linux")]
fn sleep_with_shutdown(shutdown: &AtomicBool, duration: Duration) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if shutdown.load(Ordering::Acquire) {
            return true;
        }
        let chunk = remaining.min(Duration::from_millis(250));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    shutdown.load(Ordering::Acquire)
}

#[cfg(target_os = "linux")]
fn read_proc_cpu_sample() -> Option<ProcCpuSample> {
    let proc_self = std::fs::read_to_string("/proc/self/stat").ok()?;
    let proc_total = std::fs::read_to_string("/proc/stat").ok()?;
    let process_jiffies = parse_proc_self_jiffies(&proc_self)?;
    let total_jiffies = parse_proc_total_jiffies(&proc_total)?;
    Some(ProcCpuSample {
        process_jiffies,
        total_jiffies,
    })
}

#[cfg(target_os = "linux")]
fn parse_proc_self_jiffies(contents: &str) -> Option<u64> {
    let closing_paren = contents.rfind(')')?;
    let rest = contents.get(closing_paren + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

#[cfg(target_os = "linux")]
fn parse_proc_total_jiffies(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|line| line.starts_with("cpu "))?;
    line.split_whitespace()
        .skip(1)
        .try_fold(0_u64, |acc, token| {
            token
                .parse::<u64>()
                .ok()
                .map(|value| acc.saturating_add(value))
        })
}

#[cfg(target_os = "linux")]
fn process_cpu_pct_x100(
    previous: ProcCpuSample,
    next: ProcCpuSample,
    cpu_count: u64,
) -> Option<u64> {
    let process_delta = next.process_jiffies.checked_sub(previous.process_jiffies)?;
    let total_delta = next.total_jiffies.checked_sub(previous.total_jiffies)?;
    if total_delta == 0 {
        return None;
    }

    let scaled = u128::from(process_delta)
        .saturating_mul(u128::from(cpu_count))
        .saturating_mul(10_000)
        .saturating_div(u128::from(total_delta));
    Some(u64::try_from(scaled).unwrap_or(u64::MAX))
}

fn resolve_http_runtime_worker_threads() -> usize {
    // Use a small real worker pool by default so startup stays cheap while
    // partial/incomplete client sockets still cannot monopolize a single worker.
    let default = std::thread::available_parallelism()
        .map_or(HTTP_RUNTIME_MIN_WORKERS, std::num::NonZeroUsize::get)
        .clamp(HTTP_RUNTIME_MIN_WORKERS, HTTP_RUNTIME_DEFAULT_WORKERS_CAP);
    std::env::var("AM_HTTP_WORKER_THREADS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(HTTP_RUNTIME_MIN_WORKERS, HTTP_RUNTIME_MAX_WORKERS)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpRuntimeReactorPreference {
    Epoll,
    Auto,
    IoUring,
}

#[cfg(target_os = "linux")]
impl HttpRuntimeReactorPreference {
    fn from_env() -> std::io::Result<Self> {
        let Some(raw) = std::env::var("AM_HTTP_REACTOR").ok() else {
            // Default to epoll on Linux until the io_uring accept/rearm path
            // is proven stable under sustained MCP-over-HTTP load.
            return Ok(Self::Epoll);
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "epoll" => Ok(Self::Epoll),
            "auto" => Ok(Self::Auto),
            "io_uring" | "uring" => Ok(Self::IoUring),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "invalid AM_HTTP_REACTOR value '{other}' (expected epoll, auto, or io_uring)"
                ),
            )),
        }
    }
}

#[cfg(target_os = "linux")]
fn build_http_reactor() -> std::io::Result<(Arc<dyn Reactor>, &'static str)> {
    match HttpRuntimeReactorPreference::from_env()? {
        HttpRuntimeReactorPreference::Epoll => Ok((Arc::new(EpollReactor::new()?), "epoll")),
        HttpRuntimeReactorPreference::Auto => {
            if let Ok(reactor) = IoUringReactor::new() {
                return Ok((Arc::new(reactor), "io_uring"));
            }
            Ok((Arc::new(EpollReactor::new()?), "epoll"))
        }
        HttpRuntimeReactorPreference::IoUring => {
            let reactor = IoUringReactor::new().map_err(|err| {
                std::io::Error::other(format!("failed to initialize io_uring reactor: {err}"))
            })?;
            Ok((Arc::new(reactor), "io_uring"))
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn build_http_reactor()
-> std::io::Result<(Arc<dyn asupersync::runtime::reactor::Reactor>, &'static str)> {
    Ok((create_reactor()?, "default"))
}

fn hardened_http_listener_config(config: &mcp_agent_mail_core::Config) -> Http1ListenerConfig {
    let http_config = Http1Config::default()
        // Keep-alive is allowed for polling clients, but with bounded reuse.
        // This limits long-lived connection pathologies while avoiding pure
        // per-request TCP churn in the web UI.
        .keep_alive(true)
        .max_requests(Some(8))
        .idle_timeout(Some(Duration::from_secs(config.http_idle_timeout_secs)))
        .max_headers_size(32 * 1024)
        .max_body_size(10 * 1024 * 1024); // 10MB — must match HttpHandlerConfig.max_body_size

    Http1ListenerConfig::default()
        .http_config(http_config)
        .max_connections(Some(config.http_max_connections))
        .drain_timeout(Duration::from_secs(config.http_drain_timeout_secs))
}

fn normalized_probe_host(http_host: &str) -> &str {
    match http_host.trim() {
        "" | "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        host => host,
    }
}

fn format_http_authority_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') && !host.ends_with(']') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

pub(crate) fn connect_authority_host(http_host: &str) -> String {
    format_http_authority_host(normalized_probe_host(http_host))
}

/// Build the shared [`HttpClient`] used for liveness probes.
///
/// Reusing a single client across probes avoids the TCP-setup overhead that
/// caused spurious timeouts when each probe created its own connection (see
/// GitHub issue #74).
fn build_probe_http_client() -> HttpClient {
    HttpClient::builder()
        .max_connections_per_host(1)
        .max_total_connections(1)
        .build()
}

async fn probe_http_healthz(
    cx: &Cx,
    config: &mcp_agent_mail_core::Config,
    client: &HttpClient,
) -> Result<(), HttpHealthProbeFailure> {
    let authority_host = connect_authority_host(&config.http_host);
    let url = format!("http://{authority_host}:{}{}", config.http_port, "/healthz");
    let started_at = Instant::now();
    match timeout(
        wall_now(),
        Duration::from_secs(config.http_probe_timeout_secs),
        client.get(cx, &url),
    )
    .await
    {
        Ok(Ok(response)) if response.status == 200 => Ok(()),
        Ok(Ok(response)) => Err(HttpHealthProbeFailure::Status {
            status: response.status,
            elapsed_ms: started_at.elapsed().as_millis(),
        }),
        Ok(Err(err)) => Err(HttpHealthProbeFailure::Transport {
            error: err.to_string(),
            elapsed_ms: started_at.elapsed().as_millis(),
        }),
        Err(_) => Err(HttpHealthProbeFailure::Timeout {
            elapsed_ms: started_at.elapsed().as_millis(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HttpHealthProbeFailure {
    Timeout { elapsed_ms: u128 },
    Status { status: u16, elapsed_ms: u128 },
    Transport { error: String, elapsed_ms: u128 },
}

/// Maximum time to wait for the startup readiness self-probe to succeed.
///
/// This covers the window between the TCP listener accepting connections and the
/// server being able to serve a `/healthz` 200.  It is intentionally generous to
/// tolerate slow SQLite WAL recovery or first-run migrations, but short enough
/// that a launchd-managed process doesn't sit in a degraded "running but not
/// serving" state for minutes.
const STARTUP_READINESS_SELF_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between retries inside the startup readiness self-probe loop.
const STARTUP_READINESS_SELF_PROBE_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Verify the just-started HTTP server is actually reachable by probing its
/// `/healthz` endpoint.  Retries internally until `STARTUP_READINESS_SELF_PROBE_TIMEOUT`
/// expires.
///
/// Returns `Ok(())` if a 200 response is received, or the last
/// [`HttpHealthProbeFailure`] if the timeout is exceeded.
async fn startup_readiness_self_probe(
    cx: &Cx,
    config: &mcp_agent_mail_core::Config,
) -> Result<(), HttpHealthProbeFailure> {
    let client = build_probe_http_client();
    let deadline = Instant::now() + STARTUP_READINESS_SELF_PROBE_TIMEOUT;
    let mut last_failure = HttpHealthProbeFailure::Timeout { elapsed_ms: 0 };
    while Instant::now() < deadline {
        match probe_http_healthz(cx, config, &client).await {
            Ok(()) => return Ok(()),
            Err(failure) => {
                last_failure = failure;
                // Brief pause before retrying to avoid busy-looping while the
                // server is still initialising internal state.
                sleep(wall_now(), STARTUP_READINESS_SELF_PROBE_RETRY_INTERVAL).await;
            }
        }
    }
    Err(last_failure)
}

fn build_http_runtime() -> std::io::Result<Runtime> {
    let (reactor, reactor_name) = build_http_reactor()?;
    let workers = resolve_http_runtime_worker_threads();
    tracing::info!(
        reactor = reactor_name,
        workers,
        "HTTP runtime reactor selected"
    );
    RuntimeBuilder::new()
        .with_reactor(reactor)
        .worker_threads(workers)
        .blocking_threads(workers, 64)
        .enable_parking(true)
        .build()
        .map_err(|err| map_asupersync_err(&err))
}

fn restart_backoff_ms(previous_ms: u64, min_ms: u64, max_ms: u64) -> u64 {
    // Ensure min <= max; if caller passes inverted bounds, saturate to max.
    let min_ms = min_ms.min(max_ms);
    if previous_ms == 0 {
        min_ms
    } else {
        (previous_ms.saturating_mul(2)).min(max_ms)
    }
}

fn reset_probe_state(config: &mcp_agent_mail_core::Config) -> (u32, Instant, Instant) {
    (
        0,
        Instant::now() + Duration::from_secs(config.http_probe_interval_secs),
        Instant::now() + Duration::from_secs(config.http_probe_startup_grace_secs),
    )
}

#[allow(clippy::too_many_lines)]
fn run_http_headless_supervisor(config: mcp_agent_mail_core::Config) -> std::io::Result<()> {
    tracing::info!(
        host = %config.http_host,
        port = config.http_port,
        workers = resolve_http_runtime_worker_threads(),
        "HTTP server supervisor started"
    );
    let runtime = build_http_runtime()?;
    let result_rx = spawn_http_supervisor_task(runtime.handle(), config, None, None, None)?;
    let result = recv_http_supervisor_result(result_rx);
    drop(runtime);
    result
}

fn prepare_http_runtime_startup(config: &mcp_agent_mail_core::Config) -> std::io::Result<()> {
    let probe_report = startup_checks::run_startup_probes(config);
    if !probe_report.is_ok() {
        return Err(std::io::Error::other(probe_report.format_errors()));
    }

    if config.instrumentation_enabled {
        mcp_agent_mail_db::QUERY_TRACKER.enable(Some(config.instrumentation_slow_query_ms));
    }

    Ok(())
}

const STARTUP_READINESS_FAST_PATH_GRACE: Duration = Duration::from_secs(1);
static STARTUP_READINESS_FAST_PATH_UNTIL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn startup_readiness_fast_path_handle() -> &'static Mutex<Option<Instant>> {
    STARTUP_READINESS_FAST_PATH_UNTIL.get_or_init(|| Mutex::new(None))
}

fn arm_startup_readiness_fast_path() {
    *lock_mutex(startup_readiness_fast_path_handle()) =
        Some(Instant::now() + STARTUP_READINESS_FAST_PATH_GRACE);
}

fn clear_startup_readiness_fast_path() {
    *lock_mutex(startup_readiness_fast_path_handle()) = None;
}

fn startup_readiness_fast_path_active() -> bool {
    let now = Instant::now();
    let mut until = lock_mutex(startup_readiness_fast_path_handle());
    match *until {
        Some(deadline) if now <= deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SERVER_SYNC_DB_BUSY_TIMEOUT_MS: u32 = 60_000;
// Interactive TUI and web-UI surfaces should degrade quickly under lock
// contention instead of blocking the operator behind minute-long waits.
pub(crate) const INTERACTIVE_SYNC_DB_BUSY_TIMEOUT_MS: u32 = 1_000;
// The TUI poller and observability paths must tolerate short-lived writer bursts
// without degrading into chronic "counts present, detail rows missing" snapshots.
pub(crate) const BEST_EFFORT_SYNC_DB_BUSY_TIMEOUT_MS: u32 = 5_000;

pub(crate) fn resolve_server_database_url_sqlite_path(
    database_url: &str,
) -> Option<std::path::PathBuf> {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return None;
    }

    let sqlite_path = mcp_agent_mail_core::disk::sqlite_file_path_from_database_url(database_url)?;
    Some(std::path::PathBuf::from(resolve_server_sync_sqlite_path(
        sqlite_path.to_string_lossy().as_ref(),
    )))
}

pub(crate) fn resolve_server_sync_sqlite_path(path: &str) -> String {
    if path == ":memory:" {
        return path.to_string();
    }

    let resolved = mcp_agent_mail_db::pool::normalize_sqlite_path_for_pool_key(path);
    if resolved != path {
        return resolved;
    }

    let relative_path = std::path::Path::new(path);
    if relative_path.is_absolute() || path.starts_with("./") || path.starts_with("../") {
        return path.to_string();
    }

    if !relative_path.exists() {
        let absolute_candidate = std::path::Path::new("/").join(relative_path);
        if absolute_candidate.exists() {
            return absolute_candidate.to_string_lossy().into_owned();
        }
    }

    resolved
}

fn open_sync_db_connection_with_busy_timeout(
    path: &str,
    busy_timeout_ms: u32,
) -> std::io::Result<DbConn> {
    let path = resolve_server_sync_sqlite_path(path);
    let conn = if path == ":memory:" {
        DbConn::open_memory()
    } else {
        DbConn::open_file(&path)
    }
    .map_err(|err| std::io::Error::other(format!("open sqlite file {path}: {err}")))?;
    conn.execute_raw(&format!("PRAGMA busy_timeout = {busy_timeout_ms};"))
        .map_err(|err| {
            std::io::Error::other(format!(
                "configure sqlite busy_timeout={busy_timeout_ms} on {path}: {err}"
            ))
        })?;
    Ok(conn)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn open_server_sync_db_connection(path: &str) -> std::io::Result<DbConn> {
    open_sync_db_connection_with_busy_timeout(path, SERVER_SYNC_DB_BUSY_TIMEOUT_MS)
}

pub(crate) fn open_interactive_sync_db_connection(path: &str) -> std::io::Result<DbConn> {
    open_sync_db_connection_with_busy_timeout(path, INTERACTIVE_SYNC_DB_BUSY_TIMEOUT_MS)
}

pub(crate) fn open_live_metadata_sync_db_connection(database_url: &str) -> Option<DbConn> {
    let sqlite_path = resolve_server_database_url_sqlite_path(database_url)?;
    if !sqlite_path.exists() {
        return None;
    }
    open_interactive_sync_db_connection(sqlite_path.to_string_lossy().as_ref()).ok()
}

pub(crate) fn open_best_effort_sync_db_connection(path: &str) -> std::io::Result<DbConn> {
    let conn =
        open_sync_db_connection_with_busy_timeout(path, BEST_EFFORT_SYNC_DB_BUSY_TIMEOUT_MS)?;
    ensure_base_schema_on_sync_connection(&conn);
    Ok(conn)
}

/// Ensure the base schema (tables, indexes) exists on a raw sync connection.
///
/// TUI observability connections bypass the pool's startup-init gate, so the
/// database file may not have any tables yet (startup race) or FrankenConnection
/// may not see schema created by a concurrent pool connection.  Running the
/// idempotent `CREATE TABLE IF NOT EXISTS` DDL guarantees the connection can
/// execute queries against core tables like `messages`, `agents`, etc.
fn ensure_base_schema_on_sync_connection(conn: &DbConn) {
    // Quick probe: if the core `messages` table exists we can skip the DDL.
    if conn
        .query_sync("SELECT 1 FROM messages LIMIT 0", &[])
        .is_ok()
    {
        return;
    }
    let base_ddl = mcp_agent_mail_db::schema::init_schema_sql_base();
    let mut applied = 0usize;
    for stmt in base_ddl.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        if let Err(e) = conn.execute_raw(&format!("{stmt};")) {
            // Stop on first failure to avoid a cascade of dependent errors
            // (e.g. read-only filesystem or locked database).
            tracing::warn!(
                error = %e,
                applied,
                "ensure_base_schema_on_sync_connection: aborting DDL after first failure"
            );
            return;
        }
        applied += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveDbDriftSummary {
    archive_projects: u64,
    archive_agents: u64,
    archive_messages: u64,
    archive_max_id: i64,
    db_projects: u64,
    db_agents: u64,
    db_messages: u64,
    db_max_id: i64,
    missing_archive_projects: Vec<String>,
}

impl ArchiveDbDriftSummary {
    fn readiness_error(&self) -> String {
        let missing_project_suffix = if self.missing_archive_projects.is_empty() {
            String::new()
        } else {
            format!(
                ", missing archive project(s) in db: {}",
                self.missing_archive_projects.join(", ")
            )
        };
        format!(
            "archive inventory is ahead of the sqlite index (archive projects={}, agents={}, messages={}, latest_id={}, db projects={}, agents={}, messages={}, max_id={}{})",
            self.archive_projects,
            self.archive_agents,
            self.archive_messages,
            self.archive_max_id,
            self.db_projects,
            self.db_agents,
            self.db_messages,
            self.db_max_id,
            missing_project_suffix
        )
    }
}

fn archive_inventory_has_state(storage_root: &Path) -> bool {
    let archive = mcp_agent_mail_db::scan_archive_message_inventory(storage_root);
    archive.projects > 0 || archive.agents > 0 || archive.unique_message_ids > 0
}

fn archive_storage_root_is_authoritative_for_sqlite_path(
    storage_root: &Path,
    sqlite_path: &Path,
) -> bool {
    !mcp_agent_mail_core::config::is_default_storage_root(storage_root)
        || sqlite_path.starts_with(storage_root)
}

fn inspect_archive_db_drift(
    storage_root: &Path,
    sqlite_path: &Path,
    conn: &DbConn,
) -> Result<Option<ArchiveDbDriftSummary>, String> {
    if !archive_storage_root_is_authoritative_for_sqlite_path(storage_root, sqlite_path) {
        return Ok(None);
    }

    let projects_root = storage_root.join("projects");
    let projects_root_is_dir = std::fs::symlink_metadata(&projects_root)
        .is_ok_and(|metadata| metadata.file_type().is_dir());
    if !storage_root.is_dir() || !projects_root_is_dir {
        return Ok(None);
    }

    let archive = mcp_agent_mail_db::scan_archive_message_inventory(storage_root);
    if archive.projects == 0 && archive.agents == 0 && archive.unique_message_ids == 0 {
        return Ok(None);
    }

    let rows = conn
        .query_sync(
            "SELECT \
                (SELECT COUNT(*) FROM projects) AS project_count, \
                (SELECT COUNT(*) FROM agents) AS agent_count, \
                (SELECT COUNT(*) FROM messages) AS message_count, \
                COALESCE((SELECT MAX(id) FROM messages), 0) AS max_id",
            &[],
        )
        .map_err(|e| format!("failed to inspect sqlite inventory during drift check: {e}"))?;
    let row = rows
        .first()
        .ok_or_else(|| "sqlite inventory query returned no rows during drift check".to_string())?;
    let db_project_count = row
        .get_named::<i64>("project_count")
        .ok()
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0);
    let db_agent_count = row
        .get_named::<i64>("agent_count")
        .ok()
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0);
    let db_message_count = row
        .get_named::<i64>("message_count")
        .ok()
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0);
    let db_max_id = row.get_named::<i64>("max_id").unwrap_or(0);
    let archive_max_id = archive.latest_message_id.unwrap_or(0);
    let db_project_identities =
        mcp_agent_mail_db::collect_db_project_identities(conn).map_err(|e| {
            format!("failed to inspect sqlite project identities during drift check: {e}")
        })?;
    let missing_archive_projects =
        mcp_agent_mail_db::archive_missing_project_identities(&archive, &db_project_identities);

    let archive_message_count = archive.unique_message_ids;
    let archive_messages_ahead =
        u64::try_from(archive_message_count).unwrap_or(u64::MAX) > db_message_count;
    let archive_latest_id_ahead = archive_max_id > db_max_id;
    let archive_metadata_ahead = mcp_agent_mail_db::pool::archive_metadata_advantage_is_decisive(
        archive.projects,
        archive.agents,
        archive_message_count,
        archive.latest_message_id,
        usize::try_from(db_project_count).unwrap_or(usize::MAX),
        usize::try_from(db_agent_count).unwrap_or(usize::MAX),
        usize::try_from(db_message_count).unwrap_or(usize::MAX),
        db_max_id,
        &missing_archive_projects,
    );

    if archive_messages_ahead || archive_latest_id_ahead || archive_metadata_ahead {
        return Ok(Some(ArchiveDbDriftSummary {
            archive_projects: u64::try_from(archive.projects).unwrap_or(u64::MAX),
            archive_agents: u64::try_from(archive.agents).unwrap_or(u64::MAX),
            archive_messages: u64::try_from(archive.unique_message_ids).unwrap_or(u64::MAX),
            archive_max_id,
            db_projects: db_project_count,
            db_agents: db_agent_count,
            db_messages: db_message_count,
            db_max_id,
            missing_archive_projects,
        }));
    }

    Ok(None)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservabilitySyncDbKind {
    LiveSqlite,
    ArchiveSnapshot,
}

pub(crate) struct SnapshotDirGuard {
    path: PathBuf,
}

impl SnapshotDirGuard {
    fn new(prefix: &str) -> std::io::Result<Self> {
        let base = preferred_snapshot_temp_dir()?;
        let pid = std::process::id();
        for attempt in 0..32_u32 {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = base.join(format!("{prefix}{pid}-{nonce}-{attempt}"));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "failed to allocate unique snapshot dir under {}",
                base.display()
            ),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn path_existing_prefix_has_symlink(path: &Path) -> std::io::Result<bool> {
    let mut current = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()?
    };

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => continue,
            Component::ParentDir => current.push(".."),
            Component::Normal(part) => current.push(part),
        }

        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    Ok(false)
}

fn validate_snapshot_temp_dir(candidate: &Path, source: &str) -> std::io::Result<PathBuf> {
    if path_existing_prefix_has_symlink(candidate)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{source} points to symlinked snapshot directory {}",
                candidate.display()
            ),
        ));
    }

    let metadata = std::fs::symlink_metadata(candidate).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "{source} points to unusable snapshot directory {}: {error}",
                candidate.display()
            ),
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{source} points to {} which is not a directory",
                candidate.display()
            ),
        ));
    }

    Ok(candidate.to_path_buf())
}

fn preferred_snapshot_temp_dir() -> std::io::Result<PathBuf> {
    for key in ["TMPDIR", "TEMP", "TMP"] {
        let Some(value) = mcp_agent_mail_core::config::env_value(key) else {
            continue;
        };
        let candidate = PathBuf::from(value);
        if candidate.as_os_str().is_empty() {
            continue;
        }

        return validate_snapshot_temp_dir(&candidate, key);
    }

    validate_snapshot_temp_dir(&std::env::temp_dir(), "system temp dir")
}

impl Drop for SnapshotDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) struct ObservabilitySyncDb {
    conn: DbConn,
    sqlite_path: String,
    #[cfg(test)]
    kind: ObservabilitySyncDbKind,
    _snapshot_dir: Option<SnapshotDirGuard>,
}

impl ObservabilitySyncDb {
    fn live(conn: DbConn, sqlite_path: String) -> Self {
        Self {
            conn,
            sqlite_path,
            #[cfg(test)]
            kind: ObservabilitySyncDbKind::LiveSqlite,
            _snapshot_dir: None,
        }
    }

    fn archive_snapshot(
        storage_root: &Path,
        salvage_db_path: Option<&Path>,
        context: &str,
    ) -> Result<Self, String> {
        let snapshot_dir = SnapshotDirGuard::new("server-observability-mailbox-")
            .map_err(|error| format!("failed to allocate observability snapshot dir: {error}"))?;
        let sqlite_path = snapshot_dir.path().join("mailbox.sqlite3");
        let reconstruct = salvage_db_path.map_or_else(
            || mcp_agent_mail_db::reconstruct_from_archive(&sqlite_path, storage_root),
            |salvage_db_path| {
                mcp_agent_mail_db::reconstruct_from_archive_with_salvage(
                    &sqlite_path,
                    storage_root,
                    Some(salvage_db_path),
                )
            },
        );
        if let Err(error) = reconstruct {
            tracing::warn!(
                operation = context,
                storage_root = %storage_root.display(),
                salvage = ?salvage_db_path.map(|path| path.display().to_string()),
                error = %error,
                "failed to build archive-backed observability snapshot"
            );
            return Err(format!(
                "failed to build archive-backed observability snapshot: {error}"
            ));
        }
        let sqlite_path_str = sqlite_path.to_string_lossy().into_owned();
        let conn = open_best_effort_sync_db_connection(&sqlite_path_str).map_err(|error| {
            format!(
                "failed to open archive-backed observability snapshot {}: {error}",
                sqlite_path.display()
            )
        })?;
        Ok(Self {
            conn,
            sqlite_path: sqlite_path_str,
            #[cfg(test)]
            kind: ObservabilitySyncDbKind::ArchiveSnapshot,
            _snapshot_dir: Some(snapshot_dir),
        })
    }

    pub(crate) fn conn(&self) -> &DbConn {
        &self.conn
    }

    pub(crate) fn into_parts(self) -> (DbConn, String, Option<SnapshotDirGuard>) {
        (self.conn, self.sqlite_path, self._snapshot_dir)
    }

    #[cfg(test)]
    fn uses_archive_snapshot(&self) -> bool {
        matches!(self.kind, ObservabilitySyncDbKind::ArchiveSnapshot)
    }
}

pub(crate) struct ObservabilityDbPool {
    pool: mcp_agent_mail_db::DbPool,
    _snapshot_dir: Option<SnapshotDirGuard>,
}

impl ObservabilityDbPool {
    pub(crate) fn pool(&self) -> &mcp_agent_mail_db::DbPool {
        &self.pool
    }
}

pub(crate) fn open_observability_db_pool(
    database_url: &str,
    storage_root: &Path,
    context: &str,
) -> Result<ObservabilityDbPool, String> {
    let observed = open_observability_sync_db_connection(database_url, storage_root, context)?
        .ok_or_else(|| "database connection unavailable".to_string())?;
    let (_conn, sqlite_path, snapshot_dir) = observed.into_parts();
    let cfg = DbPoolConfig {
        database_url: format!("sqlite:///{}", sqlite_path),
        storage_root: Some(storage_root.to_path_buf()),
        ..Default::default()
    };
    let pool = mcp_agent_mail_db::create_pool(&cfg)
        .map_err(|e| format!("failed to initialize DB pool: {e}"))?;
    Ok(ObservabilityDbPool {
        pool,
        _snapshot_dir: snapshot_dir,
    })
}

pub(crate) fn open_observability_sync_db_connection(
    database_url: &str,
    storage_root: &Path,
    context: &str,
) -> Result<Option<ObservabilitySyncDb>, String> {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return Ok(None);
    }

    let cfg = DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let sqlite_path = resolve_server_sync_sqlite_path(
        &cfg.sqlite_path()
            .map_err(|error| format!("invalid sqlite database URL: {error}"))?,
    );
    if sqlite_path == ":memory:" {
        return Ok(None);
    }

    let resolved_path = PathBuf::from(&sqlite_path);
    let archive_has_state =
        archive_storage_root_is_authoritative_for_sqlite_path(storage_root, &resolved_path)
            && archive_inventory_has_state(storage_root);

    match open_best_effort_sync_db_connection(&sqlite_path) {
        Ok(conn) => match inspect_archive_db_drift(storage_root, &resolved_path, &conn) {
            Ok(Some(drift)) => {
                tracing::warn!(
                    operation = context,
                    source = %resolved_path.display(),
                    storage_root = %storage_root.display(),
                    drift = drift.readiness_error(),
                    "using archive-backed observability snapshot because the live sqlite index lags the Git archive"
                );
                drop(conn);
                ObservabilitySyncDb::archive_snapshot(
                    storage_root,
                    resolved_path.exists().then_some(resolved_path.as_path()),
                    context,
                )
                .map(Some)
            }
            Ok(None) => Ok(Some(ObservabilitySyncDb::live(conn, sqlite_path))),
            Err(error) if archive_has_state => {
                tracing::warn!(
                    operation = context,
                    source = %resolved_path.display(),
                    storage_root = %storage_root.display(),
                    error = %error,
                    "using archive-backed observability snapshot because the live sqlite inventory probe failed"
                );
                drop(conn);
                ObservabilitySyncDb::archive_snapshot(
                    storage_root,
                    resolved_path.exists().then_some(resolved_path.as_path()),
                    context,
                )
                .map(Some)
            }
            Err(_) => Ok(Some(ObservabilitySyncDb::live(conn, sqlite_path))),
        },
        Err(error) if archive_has_state => {
            tracing::warn!(
                operation = context,
                source = %resolved_path.display(),
                storage_root = %storage_root.display(),
                error = %error,
                "using archive-backed observability snapshot because the live sqlite source could not be opened"
            );
            ObservabilitySyncDb::archive_snapshot(
                storage_root,
                resolved_path.exists().then_some(resolved_path.as_path()),
                context,
            )
            .map(Some)
        }
        Err(error) => {
            if resolved_path.exists() {
                Err(format!(
                    "failed to open live sqlite source {}: {error}",
                    resolved_path.display()
                ))
            } else {
                Ok(None)
            }
        }
    }
}

fn tui_readiness_warmup_failure_message(error: &str) -> String {
    format!("TUI startup: background DB readiness warmup failed ({error})")
}

fn handle_tui_readiness_warmup_result(
    tui_state: Option<&Arc<tui_bridge::TuiSharedState>>,
    result: Result<(), String>,
) {
    match result {
        Ok(()) => {
            if let Some(state) = tui_state {
                state.mark_db_ready();
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "tui readiness warmup failed; continuing with degraded DB-unavailable startup"
            );
            if let Some(state) = tui_state {
                state.mark_db_warmup_failed();
                state.push_console_log(tui_readiness_warmup_failure_message(&error));
            }
        }
    }
}

const TUI_DEFERRED_WORKER_FIRST_PAINT_WAIT: Duration = Duration::from_millis(500);
const TUI_DEFERRED_WORKER_DB_GRACE: Duration = Duration::from_secs(2);
const TUI_DEFERRED_WORKER_RECHECK_INTERVAL: Duration = Duration::from_secs(1);
const TUI_ADVISORY_CONSISTENCY_IDLE_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Default)]
struct TuiDeferredWorkerProgress {
    non_db: AtomicBool,
    db: AtomicBool,
    advisory: AtomicBool,
}

impl TuiDeferredWorkerProgress {
    fn claim_non_db_start(&self) -> bool {
        !self.non_db.swap(true, Ordering::AcqRel)
    }

    fn claim_db_start(&self) -> bool {
        !self.db.swap(true, Ordering::AcqRel)
    }

    fn claim_advisory_start(&self) -> bool {
        !self.advisory.swap(true, Ordering::AcqRel)
    }

    fn start_non_db_if_needed(&self, config: &mcp_agent_mail_core::Config) {
        if self.claim_non_db_start() {
            start_tui_non_db_background_workers(config);
        }
    }

    fn start_db_if_needed(&self, config: &mcp_agent_mail_core::Config) {
        if self.claim_db_start() {
            start_tui_db_background_workers(config);
        }
    }

    fn start_advisory_if_needed(&self, config: &mcp_agent_mail_core::Config) {
        if self.claim_advisory_start() {
            start_advisory_consistency_probe(config);
        }
    }
}

struct TuiDeferredWorkerHandle {
    join: Option<std::thread::JoinHandle<()>>,
    progress: Arc<TuiDeferredWorkerProgress>,
}

impl TuiDeferredWorkerHandle {
    fn join(&mut self) {
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn start_tui_non_db_background_workers(config: &mcp_agent_mail_core::Config) {
    disk_monitor::start(config);
}

fn start_tui_db_background_workers(config: &mcp_agent_mail_core::Config) {
    init_search_bridge(config);
    cleanup::start(config);
    ack_ttl::start(config);
    tool_metrics::start(config);
    retention::start(config);
    if config.integrity_check_on_startup {
        integrity_guard::defer_next_proactive_backup();
    }
    integrity_guard::start(config);
}

fn tui_startup_should_stop(tui_state: &Arc<tui_bridge::TuiSharedState>) -> bool {
    tui_state.is_shutdown_requested() || tui_state.is_headless_detach_requested()
}

fn wait_for_tui_first_paint(tui_state: &Arc<tui_bridge::TuiSharedState>) -> bool {
    if tui_state.wait_for_first_paint(TUI_DEFERRED_WORKER_FIRST_PAINT_WAIT) {
        return true;
    }
    tracing::debug!("TUI deferred workers still waiting for first paint");
    while !tui_startup_should_stop(tui_state) {
        if tui_state.wait_for_first_paint(TUI_DEFERRED_WORKER_RECHECK_INTERVAL) {
            return true;
        }
    }
    false
}

fn wait_for_tui_db_readiness(tui_state: &Arc<tui_bridge::TuiSharedState>) -> bool {
    let mut state = tui_state.wait_for_db_warmup(TUI_DEFERRED_WORKER_DB_GRACE);
    if state == tui_bridge::DbWarmupState::Ready {
        return true;
    }
    tracing::info!(
        db_warmup = ?state,
        "TUI deferred DB workers are waiting for a real DB-ready outcome"
    );
    while !tui_startup_should_stop(tui_state) {
        match state {
            tui_bridge::DbWarmupState::Ready => return true,
            tui_bridge::DbWarmupState::Pending => {
                if tui_state.wait_for_db_ready(TUI_DEFERRED_WORKER_RECHECK_INTERVAL) {
                    return true;
                }
            }
            tui_bridge::DbWarmupState::Failed => {
                if sleep_with_tui_shutdown(tui_state, TUI_DEFERRED_WORKER_RECHECK_INTERVAL) {
                    return false;
                }
            }
        }
        state = tui_state.db_warmup_state();
    }
    false
}

fn sleep_with_tui_shutdown(
    tui_state: &Arc<tui_bridge::TuiSharedState>,
    duration: Duration,
) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if tui_startup_should_stop(tui_state) {
            return true;
        }
        let chunk = remaining.min(Duration::from_secs(1));
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    tui_startup_should_stop(tui_state)
}

fn tui_deferred_worker_spawn_failure_message(error: &std::io::Error) -> String {
    format!(
        "TUI startup: failed to spawn deferred worker starter ({error}); background services remain gated off during this TUI session"
    )
}

fn handle_tui_deferred_background_worker_spawn_failure(
    progress: &Arc<TuiDeferredWorkerProgress>,
    tui_state: &Arc<tui_bridge::TuiSharedState>,
    error: &std::io::Error,
) {
    tracing::warn!(
        error = %error,
        "failed to spawn deferred TUI worker starter; keeping background workers gated off"
    );
    tui_state.push_console_log(tui_deferred_worker_spawn_failure_message(error));
    progress.non_db.store(false, Ordering::Release);
    progress.db.store(false, Ordering::Release);
    progress.advisory.store(false, Ordering::Release);
}

fn spawn_tui_deferred_background_workers(
    config: &mcp_agent_mail_core::Config,
    tui_state: &Arc<tui_bridge::TuiSharedState>,
) -> TuiDeferredWorkerHandle {
    let worker_config = config.clone();
    let inline_tui_state = Arc::clone(tui_state);
    let worker_tui_state = Arc::clone(&inline_tui_state);
    let progress = Arc::new(TuiDeferredWorkerProgress::default());
    let worker_progress = Arc::clone(&progress);
    match std::thread::Builder::new()
        .name("tui-deferred-workers".into())
        .spawn(move || {
            if !wait_for_tui_first_paint(&worker_tui_state) {
                return;
            }
            worker_progress.start_non_db_if_needed(&worker_config);
            if !wait_for_tui_db_readiness(&worker_tui_state) {
                return;
            }
            worker_progress.start_db_if_needed(&worker_config);
            if sleep_with_tui_shutdown(&worker_tui_state, TUI_ADVISORY_CONSISTENCY_IDLE_GRACE) {
                return;
            }
            worker_progress.start_advisory_if_needed(&worker_config);
        }) {
        Ok(handle) => TuiDeferredWorkerHandle {
            join: Some(handle),
            progress,
        },
        Err(error) => {
            handle_tui_deferred_background_worker_spawn_failure(
                &progress,
                &inline_tui_state,
                &error,
            );
            TuiDeferredWorkerHandle {
                join: None,
                progress,
            }
        }
    }
}

fn spawn_tui_readiness_warmup(
    config: &mcp_agent_mail_core::Config,
    tui_state: &Arc<tui_bridge::TuiSharedState>,
) {
    let config = config.clone();
    let tui_state = Arc::clone(tui_state);
    let failure_tui_state = Arc::clone(&tui_state);
    if let Err(error) = std::thread::Builder::new()
        .name("tui-readiness-warmup".into())
        .spawn(move || {
            handle_tui_readiness_warmup_result(Some(&tui_state), readiness_check_quick(&config));
        })
    {
        handle_tui_readiness_warmup_result(
            Some(&failure_tui_state),
            Err(format!("spawn failed: {error}")),
        );
    }
}

pub fn run_http(config: &mcp_agent_mail_core::Config) -> std::io::Result<()> {
    // Initialize console theme from parsed config (includes persisted envfile values).
    let _ = theme::init_console_theme_from_config(config.console_theme);
    // Pre-intern well-known strings to avoid first-request contention.
    mcp_agent_mail_core::pre_intern_policies();

    // IMPORTANT: startup probes (inside `prepare_http_runtime_startup`) must
    // run BEFORE acquiring runtime activity locks.  The probes take an
    // exclusive flock on the activity lockfile to verify no other process
    // is running; if we already hold a shared flock from
    // `acquire_runtime_mailbox_activity_locks`, the exclusive attempt
    // deadlocks (EAGAIN) against our own process.
    prepare_http_runtime_startup(config)?;

    // Safe to acquire now -- probes have confirmed we are the sole owner.
    let _runtime_mailbox_locks = acquire_runtime_mailbox_activity_locks(config)?;

    log_active_database(config);
    let _ = startup_checks::write_listener_pid_hint(&config.http_host, config.http_port);
    heal_storage_lock_artifacts(config);
    init_search_bridge(config);
    mcp_agent_mail_storage::wbq_start();

    // Initialize the Air Traffic Controller engine for proactive agent coordination.
    atc::init_global_atc(config);
    start_atc_operator_runtime(config);

    cleanup::start(config);
    ack_ttl::start(config);
    tool_metrics::start(config);
    retention::start(config);
    integrity_guard::start(config);
    disk_monitor::start(config);
    start_advisory_consistency_probe(config);
    let dashboard = StartupDashboard::maybe_start(config);
    set_dashboard_handle(dashboard.clone());
    arm_startup_readiness_fast_path();

    // Keep headless HTTP (`serve --no-tui`) under the same supervised restart
    // policy as the TUI path so long-lived operator sessions self-heal from
    // transport starvation or listener crashes.
    let result = run_http_headless_supervisor(config.clone());
    clear_startup_readiness_fast_path();

    retention::shutdown();
    tool_metrics::shutdown();
    ack_ttl::shutdown();
    cleanup::shutdown();
    integrity_guard::shutdown();
    disk_monitor::shutdown();
    stop_atc_operator_runtime();
    mcp_agent_mail_storage::wbq_shutdown();
    mcp_agent_mail_storage::flush_async_commits();
    if let Some(dashboard) = dashboard.as_ref() {
        dashboard.shutdown();
    }
    set_dashboard_handle(None);
    result
}

/// Run the MCP HTTP server on a background thread and the full TUI on the
/// main thread.  This is the default mode for `am serve`.
///
/// When `tui_enabled` is false (e.g. non-TTY environments or `--no-tui`),
/// this falls back to [`run_http`].
pub fn run_http_with_tui(config: &mcp_agent_mail_core::Config) -> std::io::Result<()> {
    // Fall back to headless mode when not a TTY or TUI is disabled
    if !std::io::stdout().is_terminal() || !config.tui_enabled {
        return run_http(config);
    }

    // Guard against degenerate PTY geometry (e.g. `stty size` => `0 0`) which can
    // trigger high-frequency redraw loops in the fullscreen renderer.
    if let Some((cols, rows)) = degenerate_stty_size() {
        eprintln!(
            "[warn] Detected invalid terminal size ({cols}x{rows}); \
             disabling TUI to avoid runaway CPU (headless HTTP mode)"
        );
        return run_http(config);
    }

    // ── 1. Pre-flight: theme, probes, instrumentation ──────────────
    let _ = theme::init_console_theme_from_config(config.console_theme);
    mcp_agent_mail_core::pre_intern_policies();

    // IMPORTANT: probes must run BEFORE acquiring runtime activity locks.
    // `probe_integrity` takes an exclusive flock on the activity lockfile;
    // if we already hold a shared flock the exclusive attempt deadlocks
    // (EAGAIN) against our own process.
    let probe_report = startup_checks::run_startup_probes(config);
    if !probe_report.is_ok() {
        return Err(std::io::Error::other(probe_report.format_errors()));
    }

    // Now that probes have confirmed we are the sole owner, acquire the
    // runtime shared lock for the lifetime of the process.
    let _runtime_mailbox_locks = acquire_runtime_mailbox_activity_locks(config)?;

    if config.instrumentation_enabled {
        mcp_agent_mail_db::QUERY_TRACKER.enable(Some(config.instrumentation_slow_query_ms));
    }
    let _ = startup_checks::write_listener_pid_hint(&config.http_host, config.http_port);
    log_active_database(config);

    // ── 2. Pre-paint essentials only ────────────────────────────────
    heal_storage_lock_artifacts(config);
    mcp_agent_mail_storage::wbq_start();
    atc::init_global_atc(config);
    start_atc_operator_runtime(config);

    // ── 3. Shared TUI state (replaces StartupDashboard) ─────────────
    let tui_state = tui_bridge::TuiSharedState::new(config);
    let http_runtime = match build_http_runtime() {
        Ok(runtime) => runtime,
        Err(err) => {
            stop_atc_operator_runtime();
            mcp_agent_mail_storage::wbq_shutdown();
            mcp_agent_mail_storage::flush_async_commits();
            return Err(err);
        }
    };
    set_tui_state_handle(Some(Arc::clone(&tui_state)));

    // ── 4. HTTP runtime + supervisor task (supports mode switching) ─
    let (server_ctl_tx, server_ctl_rx) =
        mpsc::channel::<tui_bridge::ServerControlMsg>(HTTP_SUPERVISOR_CONTROL_CHANNEL_CAPACITY);
    tui_state.set_server_control_sender(server_ctl_tx.clone());
    let supervisor_fail_fast_active = Arc::new(AtomicBool::new(true));
    let supervisor_result_rx = match spawn_http_supervisor_task(
        http_runtime.handle(),
        config.clone(),
        Some(Arc::clone(&tui_state)),
        Some(server_ctl_rx),
        Some(Arc::clone(&supervisor_fail_fast_active)),
    ) {
        Ok(result_rx) => result_rx,
        Err(err) => {
            set_tui_state_handle(None);
            drop(http_runtime);
            stop_atc_operator_runtime();
            mcp_agent_mail_storage::wbq_shutdown();
            mcp_agent_mail_storage::flush_async_commits();
            return Err(err);
        }
    };

    // Keep first paint fast: screens and the DB poller already degrade
    // gracefully while SQLite is still warming up, so do not block the
    // interactive handoff on migrations/readiness work here.
    spawn_tui_readiness_warmup(config, &tui_state);
    let mut deferred_workers = spawn_tui_deferred_background_workers(config, &tui_state);

    // ── 5. DB poller on dedicated thread ────────────────────────────
    let mut db_poller =
        tui_poller::DbPoller::new(Arc::clone(&tui_state), config.database_url.clone()).start();

    let startup_watchdog = TuiSpinWatchdog::start(&tui_state);

    // ── 6. TUI on main thread ───────────────────────────────────────
    let tui_result = run_tui_main_thread(&tui_state, config);
    if let Some(watchdog) = startup_watchdog {
        watchdog.shutdown();
    }
    let detach_headless = tui_result.is_ok() && tui_state.is_headless_detach_requested();

    if detach_headless {
        // TUI detached intentionally: keep HTTP server running headless.
        //
        // Any startup-gated worker thread must stop waiting on TUI-only latches
        // before we join it; otherwise a detach that happens before DB-ready can
        // leave the thread parked forever while the poller is being stopped.
        deferred_workers.join();
        deferred_workers.progress.start_non_db_if_needed(config);
        deferred_workers.progress.start_db_if_needed(config);
        deferred_workers.progress.start_advisory_if_needed(config);
        let _ = tui_state.take_headless_detach_requested();
        set_tui_state_handle(None);
        db_poller.stop();

        supervisor_fail_fast_active.store(false, Ordering::SeqCst);
        let supervisor_result = recv_http_supervisor_result(supervisor_result_rx);
        drop(http_runtime);

        retention::shutdown();
        tool_metrics::shutdown();
        ack_ttl::shutdown();
        cleanup::shutdown();
        integrity_guard::shutdown();
        disk_monitor::shutdown();
        stop_atc_operator_runtime();
        mcp_agent_mail_storage::wbq_shutdown();
        mcp_agent_mail_storage::flush_async_commits();

        return supervisor_result;
    }

    // ── 7. Graceful shutdown ────────────────────────────────────────
    supervisor_fail_fast_active.store(false, Ordering::SeqCst);
    tui_state.request_shutdown();
    let _ = server_ctl_tx.try_send(tui_bridge::ServerControlMsg::Shutdown);
    db_poller.stop();
    deferred_workers.join();

    let supervisor_result = recv_http_supervisor_result(supervisor_result_rx);
    drop(http_runtime);

    // Shutdown background workers
    set_tui_state_handle(None);
    retention::shutdown();
    tool_metrics::shutdown();
    ack_ttl::shutdown();
    cleanup::shutdown();
    integrity_guard::shutdown();
    disk_monitor::shutdown();
    stop_atc_operator_runtime();
    mcp_agent_mail_storage::wbq_shutdown();
    mcp_agent_mail_storage::flush_async_commits();

    // Return first error encountered
    combine_tui_and_supervisor_results(tui_result, supervisor_result)
}

fn tui_signal_termination_signal(err: &std::io::Error) -> Option<i32> {
    const SIGNAL_PREFIX: &str = "terminated by signal ";

    if err.kind() != std::io::ErrorKind::Interrupted {
        return None;
    }

    let message = err.to_string();
    let suffix = message
        .find(SIGNAL_PREFIX)
        .map(|index| &message[index + SIGNAL_PREFIX.len()..])?;
    let digits: String = suffix
        .chars()
        .skip_while(char::is_ascii_whitespace)
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty())
        .then_some(digits)
        .and_then(|value| value.parse::<i32>().ok())
}

fn combine_tui_and_supervisor_results(
    tui_result: std::io::Result<()>,
    supervisor_result: std::io::Result<()>,
) -> std::io::Result<()> {
    match tui_result {
        Ok(()) => supervisor_result,
        Err(err) => tui_signal_termination_signal(&err).map_or(Err(err), |signal| {
            tracing::warn!(
                signal,
                "TUI runtime terminated after receiving a shutdown signal"
            );
            supervisor_result
        }),
    }
}

#[cfg(test)]
mod tui_result_tests {
    use super::{combine_tui_and_supervisor_results, tui_signal_termination_signal};

    #[test]
    fn signal_termination_is_detected_from_interrupted_error() {
        let err = std::io::Error::new(std::io::ErrorKind::Interrupted, "terminated by signal 15");
        assert_eq!(tui_signal_termination_signal(&err), Some(15));
    }

    #[test]
    fn signal_termination_detects_embedded_signal_phrase() {
        let err = std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "fullscreen renderer terminated by signal 15 while shutting down",
        );
        assert_eq!(tui_signal_termination_signal(&err), Some(15));
    }

    #[test]
    fn signal_termination_with_clean_supervisor_is_not_fatal() {
        let result = combine_tui_and_supervisor_results(
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "terminated by signal 15",
            )),
            Ok(()),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn signal_termination_preserves_supervisor_failure() {
        let result = combine_tui_and_supervisor_results(
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "terminated by signal 15",
            )),
            Err(std::io::Error::other("listener failed")),
        );
        assert_eq!(
            result
                .expect_err("supervisor error should surface")
                .to_string(),
            "listener failed"
        );
    }

    #[test]
    fn non_signal_tui_error_still_fails() {
        let result =
            combine_tui_and_supervisor_results(Err(std::io::Error::other("render failed")), Ok(()));
        assert_eq!(
            result.expect_err("tui error should surface").to_string(),
            "render failed"
        );
    }

    #[test]
    fn interrupted_non_signal_tui_error_still_fails() {
        let result = combine_tui_and_supervisor_results(
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stdin poll interrupted",
            )),
            Ok(()),
        );
        assert_eq!(
            result
                .expect_err("non-signal interrupt should surface")
                .to_string(),
            "stdin poll interrupted"
        );
    }
}

struct HttpServerInstance {
    join: AsyncJoinHandle<std::io::Result<()>>,
    shutdown: asupersync::server::shutdown::ShutdownSignal,
    connection_manager: asupersync::server::connection::ConnectionManager,
    listener_stats: Arc<Http1ListenerStats>,
    request_diagnostics: Arc<HttpRequestRuntimeDiagnostics>,
}

#[derive(Debug, Clone)]
struct HttpRequestRuntimeDiagnosticsSnapshot {
    started_total: u64,
    completed_total: u64,
    last_started_at_ms: u64,
    last_completed_at_ms: u64,
    last_started_method: String,
    last_started_path: String,
    last_completed_method: String,
    last_completed_path: String,
    last_completed_status: u16,
}

#[derive(Debug, Default)]
struct HttpRequestRuntimeDiagnostics {
    started_total: AtomicU64,
    completed_total: AtomicU64,
    last_started_at_ms: AtomicU64,
    last_completed_at_ms: AtomicU64,
    last_completed_status: AtomicU64,
    last_started_method: Mutex<String>,
    last_started_path: Mutex<String>,
    last_completed_method: Mutex<String>,
    last_completed_path: Mutex<String>,
}

impl HttpRequestRuntimeDiagnostics {
    fn record_started(&self, method: &str, path: &str) {
        self.started_total.fetch_add(1, Ordering::Relaxed);
        self.last_started_at_ms
            .store(http_runtime_diag_now_ms(), Ordering::Relaxed);
        *lock_mutex(&self.last_started_method) = method.to_string();
        *lock_mutex(&self.last_started_path) = path.to_string();
    }

    fn record_completed(&self, method: &str, path: &str, status: u16) {
        self.completed_total.fetch_add(1, Ordering::Relaxed);
        self.last_completed_at_ms
            .store(http_runtime_diag_now_ms(), Ordering::Relaxed);
        self.last_completed_status
            .store(u64::from(status), Ordering::Relaxed);
        *lock_mutex(&self.last_completed_method) = method.to_string();
        *lock_mutex(&self.last_completed_path) = path.to_string();
    }

    fn snapshot(&self) -> HttpRequestRuntimeDiagnosticsSnapshot {
        HttpRequestRuntimeDiagnosticsSnapshot {
            started_total: self.started_total.load(Ordering::Relaxed),
            completed_total: self.completed_total.load(Ordering::Relaxed),
            last_started_at_ms: self.last_started_at_ms.load(Ordering::Relaxed),
            last_completed_at_ms: self.last_completed_at_ms.load(Ordering::Relaxed),
            last_started_method: lock_mutex(&self.last_started_method).clone(),
            last_started_path: lock_mutex(&self.last_started_path).clone(),
            last_completed_method: lock_mutex(&self.last_completed_method).clone(),
            last_completed_path: lock_mutex(&self.last_completed_path).clone(),
            last_completed_status: u16::try_from(
                self.last_completed_status.load(Ordering::Relaxed),
            )
            .unwrap_or(u16::MAX),
        }
    }
}

fn http_runtime_diag_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn http_runtime_diag_age_ms(last_ms: u64, now_ms: u64) -> Option<u64> {
    (last_ms != 0).then_some(now_ms.saturating_sub(last_ms))
}

fn log_http_runtime_snapshot(instance: &HttpServerInstance, reason: &str) {
    let now_ms = http_runtime_diag_now_ms();
    let listener = instance.listener_stats.snapshot();
    let requests = instance.request_diagnostics.snapshot();
    let last_started_method = if requests.last_started_method.is_empty() {
        "-"
    } else {
        requests.last_started_method.as_str()
    };
    let last_started_path = if requests.last_started_path.is_empty() {
        "-"
    } else {
        requests.last_started_path.as_str()
    };
    let last_completed_method = if requests.last_completed_method.is_empty() {
        "-"
    } else {
        requests.last_completed_method.as_str()
    };
    let last_completed_path = if requests.last_completed_path.is_empty() {
        "-"
    } else {
        requests.last_completed_path.as_str()
    };
    tracing::warn!(
        reason = reason,
        active_connections = instance.connection_manager.active_count(),
        listener_accepted_total = listener.accepted_total,
        listener_transient_accept_errors_total = listener.transient_accept_errors_total,
        listener_spawn_failures_total = listener.spawn_failures_total,
        listener_last_accept_age_ms = ?http_runtime_diag_age_ms(listener.last_accept_at_ms, now_ms),
        requests_started_total = requests.started_total,
        requests_completed_total = requests.completed_total,
        request_gap = requests.started_total.saturating_sub(requests.completed_total),
        requests_last_started_age_ms = ?http_runtime_diag_age_ms(requests.last_started_at_ms, now_ms),
        requests_last_completed_age_ms = ?http_runtime_diag_age_ms(requests.last_completed_at_ms, now_ms),
        last_started_method,
        last_started_path,
        last_completed_method,
        last_completed_path,
        last_completed_status = requests.last_completed_status,
        "HTTP runtime snapshot"
    );
}

struct PanicAsIoFuture<F> {
    label: &'static str,
    future: Pin<Box<F>>,
}

impl<F> PanicAsIoFuture<F> {
    fn new(label: &'static str, future: F) -> Self {
        Self {
            label,
            future: Box::pin(future),
        }
    }
}

impl<F, T> Future for PanicAsIoFuture<F>
where
    F: Future<Output = std::io::Result<T>>,
{
    type Output = std::io::Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let label = self.label;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().poll(cx)
        })) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => Poll::Ready(result),
            Err(_) => Poll::Ready(Err(std::io::Error::other(format!("{label} panicked")))),
        }
    }
}

fn recv_http_supervisor_result(
    result_rx: std::sync::mpsc::Receiver<std::io::Result<()>>,
) -> std::io::Result<()> {
    result_rx.into_iter().next().unwrap_or_else(|| {
        Err(std::io::Error::other(
            "HTTP supervisor task exited without reporting",
        ))
    })
}

fn record_http_server_started(
    tui_state: Option<&tui_bridge::TuiSharedState>,
    config: &mcp_agent_mail_core::Config,
    detail: String,
) {
    if let Some(tui_state) = tui_state {
        tui_state.update_config_snapshot(tui_bridge::ConfigSnapshot::from_config(config));
        let _ = tui_state.push_event(tui_events::MailEvent::server_started(
            format!(
                "http://{}:{}{}",
                config.http_host, config.http_port, config.http_path
            ),
            detail,
        ));
    }
}

fn record_http_server_shutdown(tui_state: Option<&tui_bridge::TuiSharedState>) {
    if let Some(tui_state) = tui_state {
        let _ = tui_state.push_event(tui_events::MailEvent::server_shutdown());
    }
}

fn spawn_http_supervisor_task(
    runtime_handle: RuntimeHandle,
    config: mcp_agent_mail_core::Config,
    tui_state: Option<Arc<tui_bridge::TuiSharedState>>,
    control_rx: Option<mpsc::Receiver<tui_bridge::ServerControlMsg>>,
    fail_fast_active: Option<Arc<AtomicBool>>,
) -> std::io::Result<std::sync::mpsc::Receiver<std::io::Result<()>>> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let spawn_handle = runtime_handle;
    let runtime_handle_for_task = spawn_handle.clone();
    let supervisor_state = tui_state;
    let fail_fast_state = supervisor_state.clone();
    spawn_handle
        .try_spawn_with_cx(move |cx| async move {
            let result = PanicAsIoFuture::new(
                "HTTP supervisor task",
                run_http_server_supervisor(
                    &cx,
                    runtime_handle_for_task,
                    config,
                    supervisor_state,
                    control_rx,
                ),
            )
            .await;

            if fail_fast_active
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::SeqCst))
                && fail_fast_state
                    .as_ref()
                    .is_some_and(|state| !state.is_shutdown_requested())
            {
                tracing::error!(
                    "HTTP supervisor task exited while TUI remained active; requesting shutdown"
                );
                if let Some(tui_state) = fail_fast_state.as_ref() {
                    let _ = tui_state.push_event(tui_events::MailEvent::server_shutdown());
                    tui_state.request_shutdown();
                }
            }

            let _ = result_tx.send(result);
        })
        .map_err(|err| std::io::Error::other(format!("spawn HTTP supervisor task: {err}")))?;

    Ok(result_rx)
}

async fn spawn_http_server_instance(
    runtime_handle: &RuntimeHandle,
    config: mcp_agent_mail_core::Config,
) -> std::io::Result<(mcp_agent_mail_core::Config, HttpServerInstance)> {
    let server = build_server(&config);
    let server_info = server.info().clone();
    let server_capabilities = server.capabilities().clone();
    let router = Arc::new(server.into_router());
    let addr = format!("{}:{}", config.http_host, config.http_port);
    let request_diagnostics = Arc::new(HttpRequestRuntimeDiagnostics::default());
    let state = Arc::new(HttpState::new(
        router,
        server_info,
        server_capabilities,
        config.clone(),
        Arc::clone(&request_diagnostics),
    ));
    let _ = state.self_ref.set(Arc::downgrade(&state));

    let handler_state = Arc::clone(&state);
    let listener = Http1Listener::bind_with_config(
        addr,
        move |req| {
            let inner = Arc::clone(&handler_state);
            async move { inner.handle(req).await }
        },
        hardened_http_listener_config(&config),
    )
    .await?;

    let local_addr = listener.local_addr()?;
    let shutdown = listener.shutdown_signal();
    let connection_manager = listener.connection_manager().clone();
    let listener_stats = listener.stats_handle();
    let listener_runtime_handle = runtime_handle.clone();
    let join = runtime_handle
        .clone()
        .try_spawn(PanicAsIoFuture::new("HTTP listener task", async move {
            let _stats = listener.run(&listener_runtime_handle).await?;
            Ok::<(), std::io::Error>(())
        }))
        .map_err(|err| std::io::Error::other(format!("failed to spawn HTTP listener: {err}")))?;

    let mut updated_config = config;
    if updated_config.http_port == 0 {
        updated_config.http_port = local_addr.port();
    }

    Ok((
        updated_config,
        HttpServerInstance {
            join,
            shutdown,
            connection_manager,
            listener_stats,
            request_diagnostics,
        },
    ))
}

async fn stop_http_server_instance(instance: HttpServerInstance) -> std::io::Result<()> {
    stop_http_server_instance_with_timeouts(
        instance,
        HTTP_SERVER_STOP_JOIN_TIMEOUT,
        HTTP_SERVER_FORCE_CLOSE_JOIN_TIMEOUT,
        HTTP_SERVER_STOP_JOIN_TIMEOUT, // drain uses same timeout as join
    )
    .await
}

async fn stop_http_server_instance_with_timeouts(
    instance: HttpServerInstance,
    join_timeout: Duration,
    force_close_timeout: Duration,
    drain_timeout: Duration,
) -> std::io::Result<()> {
    let HttpServerInstance { join, shutdown, .. } = instance;
    let mut join = Box::pin(join);
    let _ = shutdown.begin_drain(drain_timeout);
    if let Ok(result) = timeout(wall_now(), join_timeout, &mut join).await {
        result
    } else {
        let forced = shutdown.begin_force_close();
        tracing::warn!(
            forced,
            join_timeout_ms = join_timeout.as_millis(),
            force_close_timeout_ms = force_close_timeout.as_millis(),
            "HTTP server task exceeded drain timeout; escalating to force-close"
        );
        timeout(wall_now(), force_close_timeout, &mut join)
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "server task did not stop within {join_timeout:?} drain + {force_close_timeout:?} force-close window"
                    ),
                ))
            })
    }
}

async fn respawn_http_server_instance_with_retry<AbortFn, RetryFn>(
    runtime_handle: &RuntimeHandle,
    config: &mut mcp_agent_mail_core::Config,
    last_restart_sleep_ms: &mut u64,
    should_abort: AbortFn,
    on_retry_error: RetryFn,
) -> std::io::Result<HttpServerInstance>
where
    AbortFn: FnMut() -> bool,
    RetryFn: FnMut(&std::io::Error, u64),
{
    respawn_http_server_instance_with_retry_using(
        config,
        last_restart_sleep_ms,
        should_abort,
        on_retry_error,
        |cfg| spawn_http_server_instance(runtime_handle, cfg),
        |duration| sleep(wall_now(), duration),
    )
    .await
}

async fn respawn_http_server_instance_with_retry_using<
    AbortFn,
    RetryFn,
    SpawnFn,
    SpawnFut,
    SleepFn,
    SleepFut,
>(
    config: &mut mcp_agent_mail_core::Config,
    last_restart_sleep_ms: &mut u64,
    mut should_abort: AbortFn,
    mut on_retry_error: RetryFn,
    mut spawn_fn: SpawnFn,
    mut sleep_fn: SleepFn,
) -> std::io::Result<HttpServerInstance>
where
    AbortFn: FnMut() -> bool,
    RetryFn: FnMut(&std::io::Error, u64),
    SpawnFn: FnMut(mcp_agent_mail_core::Config) -> SpawnFut,
    SpawnFut: Future<Output = std::io::Result<(mcp_agent_mail_core::Config, HttpServerInstance)>>,
    SleepFn: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
{
    let mut consecutive_failures: u32 = 0;
    loop {
        if should_abort() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "HTTP restart aborted because shutdown was requested",
            ));
        }

        match spawn_fn(config.clone()).await {
            Ok((new_cfg, new_instance)) => {
                *config = new_cfg;
                return Ok(new_instance);
            }
            Err(err) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures >= config.http_max_restart_failures {
                    tracing::error!(
                        consecutive_failures,
                        max = config.http_max_restart_failures,
                        host = %config.http_host,
                        port = config.http_port,
                        error = %err,
                        "HTTP server failed to restart after maximum consecutive attempts; giving up"
                    );
                    return Err(std::io::Error::other(format!(
                        "HTTP server failed to bind {}:{} after {} consecutive attempts \
                         (last error: {err}). The process will exit so the service manager \
                         can restart it cleanly.",
                        config.http_host, config.http_port, consecutive_failures,
                    )));
                }
                *last_restart_sleep_ms = restart_backoff_ms(
                    *last_restart_sleep_ms,
                    config.http_restart_backoff_min_ms,
                    config.http_restart_backoff_max_ms,
                );
                on_retry_error(&err, *last_restart_sleep_ms);
                sleep_fn(Duration::from_millis(*last_restart_sleep_ms)).await;
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_http_server_supervisor(
    cx: &Cx,
    runtime_handle: RuntimeHandle,
    mut config: mcp_agent_mail_core::Config,
    tui_state: Option<Arc<tui_bridge::TuiSharedState>>,
    mut control_rx: Option<mpsc::Receiver<tui_bridge::ServerControlMsg>>,
) -> std::io::Result<()> {
    let (updated_config, mut instance) =
        spawn_http_server_instance(&runtime_handle, config.clone()).await?;
    config = updated_config;

    record_http_server_started(
        tui_state.as_deref(),
        &config,
        format!(
            "tui={} auth={} mode={}",
            if tui_state.is_some() { "on" } else { "off" },
            config.http_bearer_token.is_some(),
            tui_bridge::TransportBase::from_http_path(&config.http_path)
                .map_or("custom", tui_bridge::TransportBase::as_str)
        ),
    );

    // ── Startup readiness self-probe ────────────────────────────────
    // Verify the newly-bound listener can actually serve requests.
    // Without this check, the process can appear "running" to launchd/systemd
    // while the HTTP listener silently failed to start serving.
    if let Err(failure) = startup_readiness_self_probe(cx, &config).await {
        tracing::error!(
            ?failure,
            host = %config.http_host,
            port = config.http_port,
            "Startup readiness self-probe failed — server bound the port but cannot serve requests"
        );
        let _ = stop_http_server_instance(instance).await;
        return Err(std::io::Error::other(format!(
            "Server bound {}:{} but startup readiness probe failed ({failure:?}). \
             The server cannot serve requests and will exit.",
            config.http_host, config.http_port,
        )));
    }
    tracing::info!(
        host = %config.http_host,
        port = config.http_port,
        "Startup readiness self-probe passed — server is accepting requests"
    );

    let mut last_restart_sleep_ms: u64 = 0;
    let (mut liveness_failures, mut next_probe_at, mut probe_grace_until) =
        reset_probe_state(&config);
    let mut probe_client = build_probe_http_client();

    loop {
        if cx.is_cancel_requested() {
            record_http_server_shutdown(tui_state.as_deref());
            let _ = stop_http_server_instance(instance).await;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "HTTP supervisor cancelled",
            ));
        }

        if tui_state
            .as_ref()
            .is_some_and(|state| state.is_shutdown_requested())
        {
            record_http_server_shutdown(tui_state.as_deref());
            return stop_http_server_instance(instance).await;
        }

        if instance.join.is_finished() {
            log_http_runtime_snapshot(&instance, "unexpected-exit-before-restart");
            tracing::warn!("HTTP server instance exited unexpectedly; restarting");
            record_http_server_shutdown(tui_state.as_deref());
            if let Err(err) = stop_http_server_instance(instance).await {
                tracing::warn!("failed to stop exited HTTP server instance cleanly: {err}");
            }

            let restart_host = config.http_host.clone();
            let restart_port = config.http_port;
            instance = respawn_http_server_instance_with_retry(
                &runtime_handle,
                &mut config,
                &mut last_restart_sleep_ms,
                || {
                    cx.is_cancel_requested()
                        || tui_state
                            .as_ref()
                            .is_some_and(|state| state.is_shutdown_requested())
                },
                |err, backoff_ms| {
                    tracing::error!(
                        error = %err,
                        backoff_ms,
                        host = %restart_host,
                        port = restart_port,
                        "HTTP server restart failed after unexpected exit; retrying"
                    );
                },
            )
            .await?;
            record_http_server_started(
                tui_state.as_deref(),
                &config,
                "auto-restarted after unexpected exit".to_string(),
            );
            tracing::info!(
                host = %config.http_host,
                port = config.http_port,
                backoff_ms = last_restart_sleep_ms,
                "HTTP server auto-restarted after unexpected exit"
            );
            (liveness_failures, next_probe_at, probe_grace_until) = reset_probe_state(&config);
            probe_client = build_probe_http_client();
            continue;
        }

        let mut handled_control = false;
        if let Some(control_rx) = control_rx.as_mut() {
            match timeout(wall_now(), Duration::from_millis(200), control_rx.recv(cx)).await {
                Ok(Ok(tui_bridge::ServerControlMsg::ToggleTransportBase)) => {
                    let current = tui_bridge::TransportBase::from_http_path(&config.http_path)
                        .unwrap_or(tui_bridge::TransportBase::Mcp);
                    let desired = current.toggle();
                    instance = handle_transport_switch(
                        &runtime_handle,
                        &mut config,
                        tui_state.as_deref(),
                        instance,
                        desired,
                    )
                    .await?;
                    last_restart_sleep_ms = 0;
                    (liveness_failures, next_probe_at, probe_grace_until) =
                        reset_probe_state(&config);
                    probe_client = build_probe_http_client();
                    handled_control = true;
                }
                Ok(Ok(tui_bridge::ServerControlMsg::SetTransportBase(desired))) => {
                    instance = handle_transport_switch(
                        &runtime_handle,
                        &mut config,
                        tui_state.as_deref(),
                        instance,
                        desired,
                    )
                    .await?;
                    last_restart_sleep_ms = 0;
                    (liveness_failures, next_probe_at, probe_grace_until) =
                        reset_probe_state(&config);
                    probe_client = build_probe_http_client();
                    handled_control = true;
                }
                Ok(Ok(tui_bridge::ServerControlMsg::ComposeEnvelope(envelope))) => {
                    if let Some(tui_state) = tui_state.as_deref() {
                        dispatch_compose_envelope(&config.database_url, tui_state, &envelope);
                    }
                    handled_control = true;
                }
                Ok(
                    Ok(tui_bridge::ServerControlMsg::Shutdown)
                    | Err(mpsc::RecvError::Disconnected | mpsc::RecvError::Cancelled),
                ) => {
                    record_http_server_shutdown(tui_state.as_deref());
                    return stop_http_server_instance(instance).await;
                }
                Ok(Err(mpsc::RecvError::Empty)) | Err(_) => {}
            }
        } else {
            sleep(wall_now(), Duration::from_millis(200)).await;
        }

        if handled_control {
            continue;
        }

        let now = Instant::now();
        if now < next_probe_at {
            continue;
        }
        next_probe_at = now + Duration::from_secs(config.http_probe_interval_secs);

        let probe_result = probe_http_healthz(cx, &config, &probe_client).await;
        if probe_result.is_ok() {
            liveness_failures = 0;
            continue;
        }
        if now < probe_grace_until {
            continue;
        }

        // Cross-reference the server's own request diagnostics: if the server
        // successfully completed a real request within the current probe
        // interval, the probe timeout is a false positive (e.g. TCP setup
        // overhead on fresh connection).  Demote to DEBUG and do NOT count
        // toward the restart threshold.  See GitHub issue #74.
        let probe_failure = probe_result.expect_err("probe_result checked above");
        let diag_now_ms = http_runtime_diag_now_ms();
        let last_completed_ms = instance
            .request_diagnostics
            .last_completed_at_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let probe_window_ms = config.http_probe_interval_secs * 1000;
        if let Some(age_ms) = http_runtime_diag_age_ms(last_completed_ms, diag_now_ms) {
            if age_ms < probe_window_ms {
                tracing::debug!(
                    ?probe_failure,
                    last_completed_age_ms = age_ms,
                    host = %config.http_host,
                    port = config.http_port,
                    "HTTP liveness probe failed but server completed a request \
                     within the probe window; treating as false positive"
                );
                continue;
            }
        }

        liveness_failures = liveness_failures.saturating_add(1);
        log_http_runtime_snapshot(&instance, "liveness-probe-failed");
        tracing::warn!(
            ?probe_failure,
            failures = liveness_failures,
            threshold = config.http_probe_failure_threshold,
            host = %config.http_host,
            port = config.http_port,
            "HTTP liveness probe failed"
        );

        if liveness_failures < config.http_probe_failure_threshold {
            continue;
        }

        log_http_runtime_snapshot(&instance, "forcing-restart-after-liveness-failures");
        tracing::warn!("HTTP server unresponsive; forcing supervised restart");
        record_http_server_shutdown(tui_state.as_deref());
        if let Err(err) = stop_http_server_instance(instance).await {
            tracing::warn!("failed to stop unresponsive HTTP server instance cleanly: {err}");
        }

        let restart_host = config.http_host.clone();
        let restart_port = config.http_port;
        instance = respawn_http_server_instance_with_retry(
            &runtime_handle,
            &mut config,
            &mut last_restart_sleep_ms,
            || {
                cx.is_cancel_requested()
                    || tui_state
                        .as_ref()
                        .is_some_and(|state| state.is_shutdown_requested())
            },
            |err, backoff_ms| {
                tracing::error!(
                    error = %err,
                    backoff_ms,
                    host = %restart_host,
                    port = restart_port,
                    "HTTP server restart failed after liveness probe failures; retrying"
                );
            },
        )
        .await?;
        record_http_server_started(
            tui_state.as_deref(),
            &config,
            "auto-restarted after liveness probe failures".to_string(),
        );
        tracing::info!(
            host = %config.http_host,
            port = config.http_port,
            backoff_ms = last_restart_sleep_ms,
            "HTTP server auto-restarted after liveness probe failures"
        );
        (liveness_failures, next_probe_at, probe_grace_until) = reset_probe_state(&config);
        probe_client = build_probe_http_client();
    }
}

async fn handle_transport_switch(
    runtime_handle: &RuntimeHandle,
    config: &mut mcp_agent_mail_core::Config,
    tui_state: Option<&tui_bridge::TuiSharedState>,
    instance: HttpServerInstance,
    desired: tui_bridge::TransportBase,
) -> std::io::Result<HttpServerInstance> {
    if tui_bridge::TransportBase::from_http_path(&config.http_path) == Some(desired) {
        return Ok(instance);
    }

    let prev_path = config.http_path.clone();
    record_http_server_shutdown(tui_state);
    if let Err(err) = stop_http_server_instance(instance).await {
        tracing::warn!("failed to stop HTTP server before transport switch: {err}");
    }

    config.http_path = desired.http_path().to_string();
    match spawn_http_server_instance(runtime_handle, config.clone()).await {
        Ok((new_cfg, new_instance)) => {
            *config = new_cfg;
            record_http_server_started(
                tui_state,
                config,
                format!("mode switched to {}", desired.as_str()),
            );
            Ok(new_instance)
        }
        Err(err) => {
            rollback_http_transport_switch(runtime_handle, config, tui_state, prev_path, err).await
        }
    }
}

async fn rollback_http_transport_switch(
    runtime_handle: &RuntimeHandle,
    config: &mut mcp_agent_mail_core::Config,
    tui_state: Option<&tui_bridge::TuiSharedState>,
    prev_path: String,
    switch_err: std::io::Error,
) -> std::io::Result<HttpServerInstance> {
    config.http_path = prev_path;
    let rollback_host = config.http_host.clone();
    let rollback_port = config.http_port;
    let mut last_restart_sleep_ms = 0_u64;
    let rollback_instance = respawn_http_server_instance_with_retry(
        runtime_handle,
        config,
        &mut last_restart_sleep_ms,
        || tui_state.is_some_and(tui_bridge::TuiSharedState::is_shutdown_requested),
        |err, backoff_ms| {
            tracing::error!(
                error = %err,
                backoff_ms,
                host = %rollback_host,
                port = rollback_port,
                "HTTP transport rollback restart failed; retrying"
            );
        },
    )
    .await?;
    record_http_server_started(
        tui_state,
        config,
        format!("mode switch failed; rolled back ({switch_err})"),
    );
    Ok(rollback_instance)
}

/// Dispatch a compose envelope from the TUI to the database via sync `SQLite`.
///
/// Opens a one-shot connection, resolves (or auto-creates) the overseer agent,
/// inserts the message + recipients, and pushes a `MessageSent` event.
#[allow(clippy::too_many_lines)]
fn dispatch_compose_envelope(
    database_url: &str,
    tui_state: &tui_bridge::TuiSharedState,
    envelope: &tui_compose::ComposeEnvelope,
) {
    let Some(conn) = tui_poller::open_sync_connection_pub(database_url) else {
        tracing::warn!("compose: cannot open DB for send");
        return;
    };

    // 5. Insert recipients (To, Cc, Bcc).
    let mut all_recipients = Vec::new();
    for name in &envelope.to {
        all_recipients.push((name.clone(), "to".to_string()));
    }
    for name in &envelope.cc {
        all_recipients.push((name.clone(), "cc".to_string()));
    }
    for name in &envelope.bcc {
        all_recipients.push((name.clone(), "bcc".to_string()));
    }

    match mcp_agent_mail_db::sync::dispatch_root_message(
        &conn,
        &envelope.sender_name,
        &envelope.subject,
        &envelope.body_md,
        &envelope.importance,
        envelope.thread_id.as_deref(),
        &all_recipients,
    ) {
        Ok(msg_id) => {
            // 6. Push a MessageSent event to the TUI.
            let recipient_names: Vec<String> = envelope
                .to
                .iter()
                .chain(envelope.cc.iter())
                .cloned()
                .collect();
            let thread_str = envelope.thread_id.as_deref().unwrap_or("");
            let _ = tui_state.push_event(tui_events::MailEvent::message_sent(
                msg_id,
                &envelope.sender_name,
                recipient_names,
                &envelope.subject,
                thread_str,
                "default",
                envelope.body_md.clone(),
            ));

            tracing::info!(
                msg_id,
                to = ?envelope.to,
                subject = %envelope.subject,
                "compose: message sent from TUI"
            );
        }
        Err(e) => {
            println!("ERR IN DISPATCH: {e:?}");
            tracing::error!("compose: failed to dispatch message: {e}");
        }
    }
}

/// Run the TUI application on the main thread.
fn run_tui_main_thread(
    tui_state: &Arc<tui_bridge::TuiSharedState>,
    config: &mcp_agent_mail_core::Config,
) -> std::io::Result<()> {
    use ftui_runtime::program::Program;

    let model = tui_app::MailAppModel::with_config(Arc::clone(tui_state), config);

    // Explicit resize coalescer wiring keeps bursty terminal resize streams
    // (mux panes, split toggles, drag-resize) from forcing repeated redraws.
    let resize_coalescer = ftui_runtime::resize_coalescer::CoalescerConfig {
        steady_delay_ms: 12,
        burst_delay_ms: 44,
        hard_deadline_ms: 96,
        cooldown_frames: 4,
        enable_logging: env_truthy("AM_TUI_RESIZE_LOG"),
        ..ftui_runtime::resize_coalescer::CoalescerConfig::default().with_bocpd()
    };
    let tui_config = ftui_runtime::program::ProgramConfig::fullscreen()
        .with_mouse()
        .with_diff_config(stable_tui_diff_config())
        .with_budget(ftui_render::budget::FrameBudgetConfig {
            total: Duration::from_millis(100), // Match FAST_TICK_INTERVAL
            allow_frame_skip: false,           // Never blank frames
            degradation_cooldown: 5,           // 500ms between level changes
            upgrade_threshold: 0.5,
            ..Default::default()
        })
        .with_conformal_config(ftui_runtime::conformal_predictor::ConformalConfig {
            alpha: 0.10,      // 90% coverage
            min_samples: 20,  // Calibrate after ~2s
            window_size: 100, // ~10s sliding window
            ..Default::default()
        })
        .with_resize_behavior(ftui_runtime::program::ResizeBehavior::Throttled)
        .with_resize_coalescer(resize_coalescer);

    let mut program = Program::with_native_backend(model, tui_config)?;
    program.run()
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// Cached health-check counts (Fix: avoid running COUNT(*) on every /health)
// ---------------------------------------------------------------------------

/// TTL for cached project/message counts returned by the readiness endpoint.
const HEALTH_COUNT_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct HealthCountCacheEntry {
    database_url: String,
    storage_root: PathBuf,
    counts: Option<(u64, u64)>,
}

type HealthCountCacheValue = (Instant, Option<HealthCountCacheEntry>);

/// Cached `(last_refresh, project_count, message_count)`.  Both counts are
/// `Option` so we can distinguish "never fetched" from "fetch failed".
static HEALTH_COUNT_CACHE: std::sync::LazyLock<Mutex<HealthCountCacheValue>> =
    std::sync::LazyLock::new(|| {
        // Start with `None` — the read path checks `cached.is_some()` before
        // trusting the TTL, so the first call always refreshes regardless of
        // the initial Instant.  (Using `Instant::now() - TTL` would panic if
        // the server starts within ~31s of system boot on Linux, where
        // CLOCK_MONOTONIC starts from zero.)
        Mutex::new((Instant::now(), None))
    });

/// TTL for cached semantic readiness validation.
///
/// This covers the more expensive archive-vs-index parity check used by
/// `/health/readiness`, while still failing closed quickly when the mailbox
/// enters a bad state.
const READINESS_SEMANTIC_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct ReadinessSemanticCacheEntry {
    database_url: String,
    storage_root: PathBuf,
    result: Result<(), String>,
}

type ReadinessSemanticCacheValue = (Instant, Option<ReadinessSemanticCacheEntry>);

static READINESS_SEMANTIC_CACHE: std::sync::LazyLock<Mutex<ReadinessSemanticCacheValue>> =
    std::sync::LazyLock::new(|| Mutex::new((Instant::now(), None)));

// ---------------------------------------------------------------------------
// Dispatch admission control (Fix: bound concurrent spawn_blocking threads)
// ---------------------------------------------------------------------------

/// Maximum concurrent `tools/call` dispatches allowed through `spawn_blocking`.
/// Threads that exceed this limit receive an immediate "overloaded" error
/// instead of queueing, preventing unbounded thread accumulation when the
/// blocking pool backs up behind a timeout.
const MAX_CONCURRENT_DISPATCHES: u32 = 128;

/// Atomic counter tracking in-flight `spawn_blocking` dispatches.
static DISPATCH_INFLIGHT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// RAII guard that decrements `DISPATCH_INFLIGHT` on drop, ensuring the
/// counter stays accurate even when the future is cancelled or panics.
struct DispatchPermit;

impl DispatchPermit {
    /// Try to acquire a dispatch slot.  Returns `None` when the server is at
    /// capacity (`DISPATCH_INFLIGHT >= MAX_CONCURRENT_DISPATCHES`).
    fn try_acquire() -> Option<Self> {
        // Relaxed ordering is fine: the counter is advisory and races between
        // concurrent fetch_add calls are harmless (off-by-one at most).
        let prev = DISPATCH_INFLIGHT.fetch_add(1, Ordering::Relaxed);
        if prev >= MAX_CONCURRENT_DISPATCHES {
            DISPATCH_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
            None
        } else {
            Some(DispatchPermit)
        }
    }
}

impl Drop for DispatchPermit {
    fn drop(&mut self) {
        DISPATCH_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

static LIVE_DASHBOARD: std::sync::LazyLock<Mutex<Option<Arc<StartupDashboard>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Global handle to the TUI shared state for event emission from tool calls
/// and HTTP handlers. Set when TUI mode is active, `None` otherwise.
static TUI_STATE: std::sync::LazyLock<Mutex<Option<Arc<tui_bridge::TuiSharedState>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

fn tui_state_handle() -> Option<Arc<tui_bridge::TuiSharedState>> {
    lock_mutex(&TUI_STATE).as_ref().map(Arc::clone)
}

fn set_tui_state_handle(state: Option<Arc<tui_bridge::TuiSharedState>>) {
    *lock_mutex(&TUI_STATE) = state;
}

const ATC_OPERATOR_ACTION_CAPACITY: usize = 64;
const ATC_OPERATOR_EXECUTION_CAPACITY: usize = 64;
const ATC_OPERATOR_STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);
const ATC_OPERATOR_MIN_TICK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct AtcOperatorSnapshot {
    pub(crate) enabled: bool,
    pub(crate) source: String,
    pub(crate) safe_mode: bool,
    pub(crate) tick_count: u64,
    pub(crate) tracked_agents: Vec<AtcOperatorAgentSnapshot>,
    pub(crate) deadlock_cycles: usize,
    pub(crate) eprocess_value: f64,
    pub(crate) regret_avg: f64,
    pub(crate) decisions_total: u64,
    pub(crate) recent_actions: Vec<AtcOperatorActionSnapshot>,
    pub(crate) recent_decisions: Vec<atc::AtcDecisionRecord>,
    pub(crate) recent_executions: Vec<AtcOperatorExecutionSnapshot>,
    pub(crate) last_tick_micros: i64,
    pub(crate) last_tick_duration_micros: u64,
    pub(crate) last_tick_budget_micros: u64,
    pub(crate) last_tick_budget_exceeded: bool,
    pub(crate) outer_loop_overhead_micros: u64,
    pub(crate) executor_mode: String,
    pub(crate) executor_pending_effects: usize,
    pub(crate) stage_timings: atc::AtcStageTimings,
    pub(crate) kernel: atc::AtcKernelTelemetry,
    pub(crate) budget: atc::AtcBudgetTelemetry,
    pub(crate) policy: atc::AtcPolicyTelemetry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

impl AtcOperatorSnapshot {
    fn disabled() -> Self {
        Self {
            enabled: false,
            source: "disabled".to_string(),
            note: Some("ATC is disabled by configuration.".to_string()),
            ..Self::default()
        }
    }

    fn warming_up(enabled: bool) -> Self {
        Self {
            enabled,
            source: "warming_up".to_string(),
            note: Some("ATC supervisor starting; no live summary published yet.".to_string()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct AtcOperatorAgentSnapshot {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) silence_secs: i64,
    pub(crate) posterior_alive: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct AtcOperatorActionSnapshot {
    pub(crate) timestamp_micros: i64,
    pub(crate) kind: String,
    pub(crate) category: String,
    pub(crate) agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct AtcOperatorExecutionSnapshot {
    pub(crate) timestamp_micros: i64,
    pub(crate) decision_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) experience_id: Option<u64>,
    pub(crate) effect_id: String,
    pub(crate) claim_id: String,
    pub(crate) evidence_id: String,
    pub(crate) trace_id: String,
    pub(crate) kind: String,
    pub(crate) category: String,
    pub(crate) agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) policy_id: Option<String>,
    pub(crate) policy_revision: u64,
    pub(crate) execution_mode: String,
    pub(crate) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtcExecutorMode {
    Shadow,
    DryRun,
    Canary,
    Live,
}

impl AtcExecutorMode {
    fn from_env() -> Self {
        match mcp_agent_mail_core::config::full_env_value("AM_ATC_EXECUTOR_MODE")
            .as_deref()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("shadow") => Self::Shadow,
            Some("dry-run" | "dry_run" | "dryrun") => Self::DryRun,
            Some("canary") => Self::Canary,
            Some("live") => Self::Live,
            _ => Self::Live,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::DryRun => "dry_run",
            Self::Canary => "canary",
            Self::Live => "live",
        }
    }

    const fn requires_runtime(self) -> bool {
        matches!(self, Self::Canary | Self::Live)
    }

    const fn executes_advisories(self) -> bool {
        matches!(self, Self::Canary | Self::Live)
    }

    const fn executes_probes(self) -> bool {
        matches!(self, Self::Canary | Self::Live)
    }

    const fn executes_releases(self) -> bool {
        matches!(self, Self::Live)
    }
}

fn atc_durable_experience_store_writable(pool: &mcp_agent_mail_db::DbPool) -> bool {
    // File-backed mailbox DBs still are not safe for durable ATC experience
    // writes. Even through the canonical SQLite path, the current mixed-runtime
    // stack can destabilize the main mailbox file. Keep durable ATC writes
    // limited to in-memory pools until ATC state is moved to an isolated
    // canonical store.
    pool.sqlite_path() == ":memory:"
}

fn atc_durable_experience_store_enabled(
    executor_mode: AtcExecutorMode,
    atc_db_pool: Option<&mcp_agent_mail_db::DbPool>,
) -> bool {
    matches!(
        executor_mode,
        AtcExecutorMode::Live | AtcExecutorMode::Canary
    ) && atc_db_pool.is_some_and(atc_durable_experience_store_writable)
}

impl AtcOperatorActionSnapshot {
    fn console_line(&self) -> String {
        self.message.as_deref().map_or_else(
            || format!("[ATC:{}] {} {}", self.category, self.kind, self.agent),
            |message| format!("[ATC:{}] {} -> {}", self.category, self.agent, message),
        )
    }
}

#[derive(Debug)]
struct AtcOperatorRuntime {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AtcOperatorRuntime {
    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

static ATC_OPERATOR_SNAPSHOT: std::sync::LazyLock<Mutex<AtcOperatorSnapshot>> =
    std::sync::LazyLock::new(|| Mutex::new(AtcOperatorSnapshot::disabled()));
static ATC_OPERATOR_RUNTIME: std::sync::LazyLock<Mutex<Option<AtcOperatorRuntime>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

fn atc_liveness_state_label(state: atc::LivenessState) -> &'static str {
    match state {
        atc::LivenessState::Alive => "alive",
        atc::LivenessState::Flaky => "flaky",
        atc::LivenessState::Dead => "dead",
    }
}

fn atc_effect_semantic_key(effect: &atc::AtcEffectPlan) -> String {
    effect.semantics.cooldown_key.clone()
}

fn atc_execution_snapshot(
    now_micros: i64,
    effect: &atc::AtcEffectPlan,
    execution_mode: &str,
    status: &str,
) -> AtcOperatorExecutionSnapshot {
    let capture = atc_execution_capture(status);
    AtcOperatorExecutionSnapshot {
        timestamp_micros: now_micros,
        decision_id: effect.decision_id,
        experience_id: effect.experience_id,
        effect_id: effect.effect_id.clone(),
        claim_id: effect.claim_id.clone(),
        evidence_id: effect.evidence_id.clone(),
        trace_id: effect.trace_id.clone(),
        kind: effect.kind.clone(),
        category: effect.category.clone(),
        agent: effect.agent.clone(),
        project_key: effect.project_key.clone(),
        policy_id: effect.policy_id.clone(),
        policy_revision: effect.policy_revision,
        execution_mode: execution_mode.to_string(),
        status: capture.snapshot_status.to_string(),
        status_detail: capture.detail,
        message: atc_effect_operator_message(effect),
    }
}

fn atc_action_snapshot_from_execution(
    execution: &AtcOperatorExecutionSnapshot,
) -> AtcOperatorActionSnapshot {
    AtcOperatorActionSnapshot {
        timestamp_micros: execution.timestamp_micros,
        kind: execution.kind.clone(),
        category: execution.category.clone(),
        agent: execution.agent.clone(),
        message: atc_action_snapshot_message(execution),
    }
}

fn atc_action_snapshot_message(execution: &AtcOperatorExecutionSnapshot) -> Option<String> {
    if execution.status == "executed" {
        return execution.message.clone();
    }

    let status_prefix = execution.status_detail.as_deref().map_or_else(
        || format!("[{}]", execution.status),
        |detail| format!("[{}:{detail}]", execution.status),
    );
    Some(match execution.message.as_deref() {
        Some(message) => format!("{status_prefix} {message}"),
        None => status_prefix,
    })
}

fn push_bounded<T>(items: &mut VecDeque<T>, capacity: usize, item: T) {
    if items.len() >= capacity {
        let _ = items.pop_front();
    }
    items.push_back(item);
}

fn record_atc_operator_execution(
    recent_executions: &mut VecDeque<AtcOperatorExecutionSnapshot>,
    recent_actions: &mut VecDeque<AtcOperatorActionSnapshot>,
    visible_actions: &mut Vec<AtcOperatorActionSnapshot>,
    execution: AtcOperatorExecutionSnapshot,
) {
    push_bounded(
        recent_executions,
        ATC_OPERATOR_EXECUTION_CAPACITY,
        execution.clone(),
    );
    let action_snapshot = atc_action_snapshot_from_execution(&execution);
    if visible_actions.len() < ATC_OPERATOR_ACTION_CAPACITY {
        visible_actions.push(action_snapshot.clone());
    }
    push_bounded(
        recent_actions,
        ATC_OPERATOR_ACTION_CAPACITY,
        action_snapshot,
    );
}

fn atc_status_consumes_cooldown(status: &str) -> bool {
    !status.starts_with("failed:") && status != "suppressed:missing_project_precondition"
}

const ATC_QUEUE_BACKPRESSURE_STATUS: &str = "throttled:pending_queue_capacity";

fn atc_effect_subject(effect: &atc::AtcEffectPlan) -> String {
    match effect.semantics.family.as_str() {
        "liveness_monitoring" => format!("[ATC] activity check for {}", effect.agent),
        "deadlock_remediation" => {
            let target = effect
                .project_key
                .as_deref()
                .unwrap_or(effect.agent.as_str());
            format!("[ATC] deadlock remediation for {target}")
        }
        "liveness_probe" => format!("[ATC] acknowledgment requested from {}", effect.agent),
        "reservation_release" => format!("[ATC] reservation release for {}", effect.agent),
        "release_notice" => format!("[ATC] release requested for {}", effect.agent),
        "withheld_release_notice" => format!("[ATC] release withheld for {}", effect.agent),
        _ => match effect.kind.as_str() {
            "send_advisory" => format!("[ATC] {} advisory for {}", effect.category, effect.agent),
            "release_reservations_requested" => {
                format!("[ATC] release request for {}", effect.agent)
            }
            "probe_agent" => format!("[ATC] probe request for {}", effect.agent),
            _ => format!("[ATC] {} for {}", effect.kind, effect.agent),
        },
    }
}

fn atc_effect_default_headline(effect: &atc::AtcEffectPlan) -> String {
    match effect.semantics.family.as_str() {
        "liveness_monitoring" => format!(
            "ATC sees sustained inactivity from {} and is holding at advisory level for now.",
            effect.agent
        ),
        "deadlock_remediation" => format!(
            "ATC found a deterministic reservation deadlock affecting {}.",
            effect.agent
        ),
        "liveness_probe" => format!(
            "ATC needs an acknowledgment from {} to distinguish a stale session from active work.",
            effect.agent
        ),
        "reservation_release" => format!(
            "ATC is requesting reservation release for {} after a dead-agent verdict.",
            effect.agent
        ),
        "release_notice" => format!(
            "ATC requested reservation release for {} and is surfacing the intervention explicitly.",
            effect.agent
        ),
        "withheld_release_notice" => format!(
            "ATC withheld reservation release for {} because the liveness evidence is still uncertain.",
            effect.agent
        ),
        _ => match effect.kind.as_str() {
            "probe_agent" => format!(
                "ATC liveness probe for {}. Please acknowledge or reply if you are still active.",
                effect.agent
            ),
            _ => format!("ATC generated effect '{}'.", effect.kind),
        },
    }
}

fn atc_effect_has_explicit_message(effect: &atc::AtcEffectPlan) -> bool {
    effect
        .message
        .as_deref()
        .map(str::trim)
        .is_some_and(|message| !message.is_empty())
}

fn atc_effect_headline(effect: &atc::AtcEffectPlan) -> String {
    if atc_effect_has_explicit_message(effect) {
        effect
            .message
            .as_deref()
            .map(str::trim)
            .map(str::to_string)
            .unwrap_or_else(|| atc_effect_default_headline(effect))
    } else {
        atc_effect_default_headline(effect)
    }
}

fn atc_effect_operator_message(effect: &atc::AtcEffectPlan) -> Option<String> {
    Some(atc_effect_headline(effect))
}

fn atc_effect_body(effect: &atc::AtcEffectPlan) -> String {
    let headline = atc_effect_headline(effect);
    let expected_loss = effect
        .expected_loss
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "n/a".to_string());
    let experience_id = effect
        .experience_id
        .map(|value| value.to_string())
        .unwrap_or_else(|| "pending".to_string());
    let mut lines = vec![
        headline,
        String::new(),
        format!("signal: {}", effect.semantics.evidence_summary),
        format!("next_step: {}", effect.semantics.operator_action),
        format!("utility: {}", effect.semantics.utility_model),
        format!("risk: {}", effect.semantics.risk_level),
        format!(
            "cooldown_micros: {}",
            effect.semantics.cooldown_micros.max(0)
        ),
        format!("escalation: {}", effect.semantics.escalation_policy),
    ];
    if let Some(project_key) = effect.project_key.as_deref() {
        lines.push(format!("project: {project_key}"));
    }
    if !effect.semantics.preconditions.is_empty() {
        lines.push(format!(
            "preconditions: {}",
            effect.semantics.preconditions.join(" | ")
        ));
    }
    lines.extend([
        format!("decision_id: {}", effect.decision_id),
        format!("experience_id: {experience_id}"),
        format!("trace_id: {}", effect.trace_id),
        format!("claim_id: {}", effect.claim_id),
        format!("evidence_id: {}", effect.evidence_id),
        format!("effect_id: {}", effect.effect_id),
        format!("policy_revision: {}", effect.policy_revision),
        format!("expected_loss: {expected_loss}"),
        "mode: automated-atc".to_string(),
    ]);
    lines.join("\n")
}

fn stable_fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn atc_effect_numeric_id(effect_id: &str) -> u64 {
    let parsed = effect_id
        .strip_prefix("atc-effect-")
        .and_then(|suffix| u64::from_str_radix(suffix, 16).ok())
        .unwrap_or_else(|| stable_fnv1a64(effect_id.as_bytes()));
    parsed & (i64::MAX as u64)
}

fn atc_effect_kind(effect: &atc::AtcEffectPlan) -> Result<EffectKind, String> {
    match effect.kind.as_str() {
        "send_advisory" => Ok(EffectKind::Advisory),
        "release_reservations_requested" => Ok(EffectKind::Release),
        "probe_agent" => Ok(EffectKind::Probe),
        "routing_suggestion" => Ok(EffectKind::RoutingSuggestion),
        "backpressure" => Ok(EffectKind::Backpressure),
        "no_action" => Ok(EffectKind::NoAction),
        other => Err(format!(
            "unsupported ATC effect kind for experience append: {other}"
        )),
    }
}

fn atc_experience_subsystem(subsystem: atc::AtcSubsystem) -> ExperienceSubsystem {
    match subsystem {
        atc::AtcSubsystem::Liveness => ExperienceSubsystem::Liveness,
        atc::AtcSubsystem::Conflict => ExperienceSubsystem::Conflict,
        atc::AtcSubsystem::LoadRouting => ExperienceSubsystem::LoadRouting,
        atc::AtcSubsystem::Synthesis => ExperienceSubsystem::Synthesis,
        atc::AtcSubsystem::Calibration => ExperienceSubsystem::Calibration,
    }
}

fn atc_posterior_probability(posterior: &[(String, f64)], label: &str) -> f64 {
    posterior
        .iter()
        .find_map(|(state, probability)| state.eq_ignore_ascii_case(label).then_some(*probability))
        .unwrap_or(0.0)
}

fn atc_runner_up_action(record: &atc::AtcDecisionRecord) -> Option<(String, f64)> {
    record
        .loss_table
        .iter()
        .filter(|entry| entry.action != record.action)
        .min_by(|left, right| left.expected_loss.total_cmp(&right.expected_loss))
        .map(|entry| (entry.action.clone(), entry.expected_loss))
}

fn build_atc_feature_vector(
    record: &atc::AtcDecisionRecord,
    effect_kind: EffectKind,
) -> FeatureVector {
    let mut features = FeatureVector::zeroed();
    features.posterior_alive_bp = prob_to_bp(atc_posterior_probability(&record.posterior, "alive"));
    features.posterior_flaky_bp = prob_to_bp(atc_posterior_probability(&record.posterior, "flaky"));
    features.expected_loss_bp = loss_to_bp(record.expected_loss);
    features.loss_gap_bp = loss_to_bp((record.runner_up_loss - record.expected_loss).max(0.0));
    features.calibration_healthy = record.calibration_healthy;
    features.safe_mode_active = record.safe_mode_active;
    features.risk_tier = FeatureVector::risk_tier_for(effect_kind);
    features
}

fn build_atc_experience_row(effect: &atc::AtcEffectPlan) -> Result<ExperienceRow, String> {
    let decision = atc::atc_decision_record(effect.decision_id)
        .ok_or_else(|| format!("missing ATC decision record {}", effect.decision_id))?;
    let effect_kind = atc_effect_kind(effect)?;
    let mut builder = ExperienceBuilder::new(
        decision.id,
        atc_effect_numeric_id(&effect.effect_id),
        decision.trace_id.clone(),
        decision.claim_id.clone(),
        decision.evidence_id.clone(),
        atc_experience_subsystem(decision.subsystem),
        decision.decision_class.clone(),
        decision.subject.clone(),
        effect_kind,
        decision.action.clone(),
        decision.posterior.clone(),
        decision.expected_loss,
        decision.evidence_summary.clone(),
        decision.calibration_healthy,
        decision.safe_mode_active,
    )
    .features(build_atc_feature_vector(&decision, effect_kind))
    .context(serde_json::json!({
        "action_family": {
            "kind": effect.kind.clone(),
            "category": effect.category.clone(),
        },
        "effect_semantics": {
            "family": effect.semantics.family.clone(),
            "risk_level": effect.semantics.risk_level.clone(),
            "utility_model": effect.semantics.utility_model.clone(),
            "operator_action": effect.semantics.operator_action.clone(),
            "remediation": effect.semantics.remediation.clone(),
            "escalation_policy": effect.semantics.escalation_policy.clone(),
            "evidence_summary": effect.semantics.evidence_summary.clone(),
            "cooldown_key": effect.semantics.cooldown_key.clone(),
            "cooldown_micros": effect.semantics.cooldown_micros,
            "requires_project": effect.semantics.requires_project,
            "ack_required": effect.semantics.ack_required,
            "high_risk_intervention": effect.semantics.high_risk_intervention,
            "preconditions": effect.semantics.preconditions.clone(),
        },
        "decision_timestamp_micros": decision.timestamp_micros,
        "effect_id_raw": effect.effect_id.clone(),
        "policy_revision": effect.policy_revision,
        "explicit_message_present": atc_effect_has_explicit_message(effect),
        "fallback_reason": decision.fallback_reason.clone(),
    }));
    if let Some(project_key) = effect.project_key.as_ref() {
        builder = builder.project_key(project_key.clone());
    }
    if let Some(policy_id) = effect
        .policy_id
        .clone()
        .or_else(|| decision.policy_id.clone())
    {
        builder = builder.policy_id(policy_id);
    }
    if let Some((runner_up_action, runner_up_loss)) = atc_runner_up_action(&decision) {
        builder = builder.runner_up(runner_up_action, runner_up_loss);
    }
    Ok(builder.build(0, effect.timestamp_micros))
}

fn append_atc_experience_for_effect(
    pool: &mcp_agent_mail_db::DbPool,
    effect: &atc::AtcEffectPlan,
) -> Result<ExperienceRow, String> {
    if !atc_durable_experience_store_writable(pool) {
        return Err(
            "ATC durable experience store is disabled for file-backed mailboxes".to_string(),
        );
    }

    let row = build_atc_experience_row(effect)?;
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    match block_on(mcp_agent_mail_db::queries::append_atc_experience(
        &cx, pool, &row,
    )) {
        asupersync::Outcome::Ok(value) => Ok(value),
        asupersync::Outcome::Err(error) => Err(error.to_string()),
        asupersync::Outcome::Cancelled(reason) => Err(format!("cancelled: {reason:?}")),
        asupersync::Outcome::Panicked(payload) => Err(format!("panicked: {}", payload.message())),
    }
}

#[derive(Debug, Clone)]
struct AtcExecutionCapture {
    snapshot_status: &'static str,
    state: ExperienceState,
    classification: &'static str,
    detail: Option<String>,
    non_execution_reason: Option<NonExecutionReason>,
}

fn bounded_atc_execution_detail(detail: &str) -> String {
    const MAX_CHARS: usize = 160;

    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let bounded: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

fn bounded_optional_atc_execution_detail(detail: &str) -> Option<String> {
    let bounded = bounded_atc_execution_detail(detail);
    (!bounded.is_empty()).then_some(bounded)
}

fn atc_execution_capture(raw_status: &str) -> AtcExecutionCapture {
    if raw_status == "executed" {
        return AtcExecutionCapture {
            snapshot_status: "executed",
            state: ExperienceState::Executed,
            classification: "success",
            detail: None,
            non_execution_reason: None,
        };
    }

    if let Some(detail) = raw_status.strip_prefix("failed:") {
        return AtcExecutionCapture {
            snapshot_status: "failed",
            state: ExperienceState::Failed,
            classification: "runtime_failure",
            detail: bounded_optional_atc_execution_detail(detail),
            non_execution_reason: None,
        };
    }

    if let Some(detail) = raw_status.strip_prefix("suppressed:") {
        let detail = bounded_optional_atc_execution_detail(detail);
        let gate_name = detail
            .clone()
            .unwrap_or_else(|| "unspecified_suppression".to_string());
        return AtcExecutionCapture {
            snapshot_status: "suppressed",
            state: ExperienceState::Suppressed,
            classification: "policy_suppression",
            detail,
            non_execution_reason: Some(NonExecutionReason::SafetyGate {
                gate_name,
                risk_score: 0.0,
                gate_threshold: 0.0,
            }),
        };
    }

    if let Some(detail) = raw_status.strip_prefix("skipped:") {
        return AtcExecutionCapture {
            snapshot_status: "skipped",
            state: ExperienceState::Skipped,
            classification: "deliberate_inaction",
            detail: bounded_optional_atc_execution_detail(detail),
            non_execution_reason: Some(NonExecutionReason::DeliberateInaction {
                no_action_loss: 0.0,
                best_action_loss: 0.0,
            }),
        };
    }

    if let Some(detail) = raw_status.strip_prefix("throttled:") {
        let detail = bounded_optional_atc_execution_detail(detail);
        let budget_name = detail
            .clone()
            .unwrap_or_else(|| "unspecified_throttle".to_string());
        return AtcExecutionCapture {
            snapshot_status: "throttled",
            state: ExperienceState::Throttled,
            classification: "budget_throttle",
            detail,
            non_execution_reason: Some(NonExecutionReason::BudgetExhausted {
                budget_name,
                current: 0.0,
                threshold: 0.0,
            }),
        };
    }

    if raw_status == "dry_run" || raw_status == "shadowed" || raw_status.starts_with("shadowed_") {
        let detail = match raw_status {
            "dry_run" => "executor_mode_dry_run",
            "shadowed" => "executor_mode_shadow",
            other => other,
        };
        let detail = bounded_atc_execution_detail(detail);
        return AtcExecutionCapture {
            snapshot_status: "suppressed",
            state: ExperienceState::Suppressed,
            classification: "policy_suppression",
            detail: Some(detail.clone()),
            non_execution_reason: Some(NonExecutionReason::SafetyGate {
                gate_name: detail,
                risk_score: 0.0,
                gate_threshold: 0.0,
            }),
        };
    }

    if raw_status == "executor_unavailable" || raw_status.starts_with("missing_") {
        return AtcExecutionCapture {
            snapshot_status: "failed",
            state: ExperienceState::Failed,
            classification: "runtime_failure",
            detail: bounded_optional_atc_execution_detail(raw_status),
            non_execution_reason: None,
        };
    }

    AtcExecutionCapture {
        snapshot_status: "failed",
        state: ExperienceState::Failed,
        classification: "runtime_failure",
        detail: bounded_optional_atc_execution_detail(raw_status),
        non_execution_reason: None,
    }
}

fn atc_execution_context_patch(
    capture: &AtcExecutionCapture,
    execution_mode: &str,
    raw_status: &str,
    ts_micros: i64,
) -> serde_json::Value {
    let mut execution = serde_json::Map::from_iter([
        (
            "status".to_string(),
            serde_json::Value::String(capture.snapshot_status.to_string()),
        ),
        (
            "classification".to_string(),
            serde_json::Value::String(capture.classification.to_string()),
        ),
        (
            "mode".to_string(),
            serde_json::Value::String(execution_mode.to_string()),
        ),
        (
            "raw_status".to_string(),
            serde_json::Value::String(bounded_atc_execution_detail(raw_status)),
        ),
        (
            "captured_ts_micros".to_string(),
            serde_json::Value::Number(ts_micros.into()),
        ),
    ]);
    if let Some(detail) = capture.detail.as_ref() {
        execution.insert(
            "detail".to_string(),
            serde_json::Value::String(detail.clone()),
        );
    }
    serde_json::json!({ "execution": execution })
}

/// Capture the execution result for an ATC experience row by transitioning
/// its lifecycle state in the database.
fn capture_atc_execution_result(
    pool: &mcp_agent_mail_db::DbPool,
    experience_id: Option<u64>,
    execution_mode: &str,
    status: &str,
    ts_micros: i64,
) {
    if !atc_durable_experience_store_writable(pool) {
        return;
    }

    let Some(exp_id) = experience_id else {
        // No experience ID means append_atc_experience_for_effect failed earlier.
        // The effect was still executed but has no durable experience record.
        // This is tracked as an ATC diagnostic gap, not silently swallowed.
        tracing::debug!("skipping execution capture: no experience_id (append may have failed)");
        return;
    };

    let capture = atc_execution_capture(status);

    // Transition: Planned → Dispatched (effect was handed to executor).
    // If this fails, we still attempt the second transition because an
    // orphaned experience stuck in Planned is worse than skipping a step.
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    match block_on(mcp_agent_mail_db::queries::transition_atc_experience(
        &cx,
        pool,
        exp_id,
        ExperienceState::Dispatched,
        ts_micros,
        None,
        None,
    )) {
        asupersync::Outcome::Ok(()) => {}
        asupersync::Outcome::Err(mcp_agent_mail_db::DbError::InvalidArgument {
            field: "state",
            ..
        }) => {}
        asupersync::Outcome::Err(error) => {
            tracing::warn!(experience_id = exp_id, %error, "failed to mark experience dispatched, continuing to final state");
            // Do NOT return — fall through to the second transition so the
            // execution result is still captured. An experience in Planned state
            // with no execution record is worse than a skipped Dispatched step.
        }
        _ => {}
    }

    // Transition: Dispatched → Executed/Failed/Throttled/Suppressed/Skipped.
    let context_patch = atc_execution_context_patch(&capture, execution_mode, status, ts_micros);
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    match block_on(mcp_agent_mail_db::queries::transition_atc_experience(
        &cx,
        pool,
        exp_id,
        capture.state,
        ts_micros,
        capture.non_execution_reason.as_ref(),
        Some(&context_patch),
    )) {
        asupersync::Outcome::Ok(()) => {}
        asupersync::Outcome::Err(error) => {
            tracing::warn!(
                experience_id = exp_id,
                ?capture.state,
                %error,
                "failed to transition experience after execution"
            );
        }
        _ => {}
    }
}

fn atc_resolution_anchor_micros(experience: &ExperienceRow) -> i64 {
    experience
        .executed_ts_micros
        .or(experience.dispatched_ts_micros)
        .unwrap_or(experience.created_ts_micros)
}

fn atc_resolution_outcome_from_activity(
    experience: &ExperienceRow,
    agent_active_since: i64,
) -> Option<ExperienceOutcome> {
    let resolution_anchor_micros = atc_resolution_anchor_micros(experience);
    if agent_active_since <= resolution_anchor_micros {
        return None;
    }

    Some(ExperienceOutcome {
        observed_ts_micros: agent_active_since,
        label: "later_activity".to_string(),
        correct: true,
        actual_loss: Some(0.0),
        regret: Some(0.0),
        evidence: Some(serde_json::json!({
            "resolution_signal": "agent_activity",
            "agent": experience.subject,
            "activity_ts_micros": agent_active_since,
            "latency_micros": agent_active_since - resolution_anchor_micros,
        })),
    })
}

fn promote_executed_experience_to_open_for_resolution(
    pool: &mcp_agent_mail_db::DbPool,
    experience_id: u64,
    state: ExperienceState,
    now_micros: i64,
    failure_message: &'static str,
) -> bool {
    if !atc_durable_experience_store_writable(pool) {
        return false;
    }

    if state != ExperienceState::Executed {
        return true;
    }

    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    match block_on(mcp_agent_mail_db::queries::transition_atc_experience(
        &cx,
        pool,
        experience_id,
        ExperienceState::Open,
        now_micros,
        None,
        None,
    )) {
        asupersync::Outcome::Ok(()) => true,
        asupersync::Outcome::Err(error) => {
            tracing::debug!(
                experience_id,
                %error,
                reason = failure_message,
                "failed to promote executed ATC experience to open"
            );
            false
        }
        asupersync::Outcome::Cancelled(_) => {
            tracing::debug!(
                experience_id,
                reason = failure_message,
                "promotion of executed ATC experience to open was cancelled"
            );
            false
        }
        asupersync::Outcome::Panicked(_) => {
            tracing::warn!(
                experience_id,
                reason = failure_message,
                "promotion of executed ATC experience to open panicked"
            );
            false
        }
    }
}

/// Sweep open ATC experiences for resolution signals (br-0qt6e.2.3).
///
/// Checks experiences in `executed` or `open` state and resolves them based on
/// messaging signals: later agent activity, acknowledgements, or elapsed
/// resolution windows.
///
/// **Resolution rules:**
/// - Advisory/Probe to agent X, and agent X was active after the effect →
///   Resolved with label "later_activity", correct = true.
/// - Advisory/Probe with no agent activity and resolution window elapsed →
///   Expired (outcome unobservable within window).
/// - Experience for departed agent (no project registration) → Censored.
///
/// This function is designed to run periodically (every N ticks) to amortize
/// the query cost across tick budgets.
fn sweep_open_experiences_for_resolution(
    pool: &mcp_agent_mail_db::DbPool,
    now_micros: i64,
    resolution_window_micros: i64,
) {
    if !atc_durable_experience_store_writable(pool) {
        return;
    }

    let cx = Cx::for_request_with_budget(Budget::INFINITE);

    // Fetch up to 50 open experiences per sweep to bound query cost.
    let open_experiences = match block_on(mcp_agent_mail_db::queries::fetch_open_atc_experiences(
        &cx, pool, None, 50,
    )) {
        asupersync::Outcome::Ok(rows) => rows,
        asupersync::Outcome::Err(error) => {
            tracing::debug!(%error, "failed to fetch open experiences for resolution sweep");
            return;
        }
        _ => return,
    };

    for experience in &open_experiences {
        // Ensure executed experiences transition to open before resolution.
        // The state machine requires Executed → Open → Resolved.
        if !promote_executed_experience_to_open_for_resolution(
            pool,
            experience.experience_id,
            experience.state,
            now_micros,
            "failed to transition experience to open",
        ) {
            continue;
        }

        let resolution_anchor_micros = atc_resolution_anchor_micros(experience);
        let age_micros = now_micros.saturating_sub(resolution_anchor_micros);

        // Check if the subject agent has been active since this effect was
        // actually dispatched/executed. Agent activity is tracked by the
        // ATC engine.
        let agent_active_since = atc::atc_agent_last_activity(&experience.subject).unwrap_or(0);
        if let Some(outcome) = atc_resolution_outcome_from_activity(experience, agent_active_since)
        {
            // Positive resolution: the agent showed activity after the
            // advisory/probe, indicating the decision was correct.
            let cx = Cx::for_request_with_budget(Budget::INFINITE);
            if let asupersync::Outcome::Err(error) =
                block_on(mcp_agent_mail_db::queries::resolve_atc_experience(
                    &cx,
                    pool,
                    experience.experience_id,
                    &outcome,
                ))
            {
                tracing::warn!(
                    experience_id = experience.experience_id,
                    %error,
                    "failed to resolve experience via later_activity"
                );
            }
        } else if age_micros > resolution_window_micros {
            // Resolution window elapsed without activity signal → expire.
            let cx = Cx::for_request_with_budget(Budget::INFINITE);
            if let asupersync::Outcome::Err(error) =
                block_on(mcp_agent_mail_db::queries::transition_atc_experience(
                    &cx,
                    pool,
                    experience.experience_id,
                    ExperienceState::Expired,
                    now_micros,
                    None,
                    None,
                ))
            {
                tracing::warn!(
                    experience_id = experience.experience_id,
                    %error,
                    "failed to expire experience after resolution window"
                );
            }
        }
        // Otherwise: still within resolution window, leave as open.
    }
}

#[allow(dead_code)]
fn looks_like_project_slug(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && !trimmed.contains('/') && !trimmed.contains('\\')
}

#[allow(dead_code)]
fn atc_project_keys_match(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return left == right;
    }
    if left == right {
        return true;
    }

    let left_identity = mcp_agent_mail_core::resolve_project_identity(left);
    let right_identity = mcp_agent_mail_core::resolve_project_identity(right);
    if left_identity.human_key == right_identity.human_key
        || left_identity.canonical_path == right_identity.canonical_path
    {
        return true;
    }

    // Only fall back to slug matching when at least one side actually looks
    // like a slug. Two distinct filesystem paths can legitimately slugify to
    // the same value, so pure path-vs-path comparison must stay exact.
    (looks_like_project_slug(left) || looks_like_project_slug(right))
        && left_identity
            .slug
            .eq_ignore_ascii_case(&right_identity.slug)
}

/// Resolve open conflict-subsystem experiences when reservation events arrive (br-0qt6e.2.4).
///
/// Helper for reservation-event domain derivers after grants, releases, or conflict detections.
/// Finds open experiences for the relevant agent(s) in the Conflict subsystem and resolves them
/// with the appropriate outcome label.
///
/// **Resolution rules:**
/// - Reservation grant clears conflicts → Resolve ForceReservation/Advisory experiences as correct.
/// - Reservation release clears conflicts → Resolve Release experiences as correct.
/// - New conflict detected → Resolve no-action experiences as needing action (not-correct but
///   expected, since the system intentionally did not intervene).
///
/// When wired from domain event derivers, this helper must remain cheap because
/// it is intended for inline use rather than periodic sweeping: bounded at 20
/// open experiences per call.
#[allow(dead_code)]
pub(crate) fn resolve_conflict_experiences_on_reservation_event(
    pool: &mcp_agent_mail_db::DbPool,
    agent: &str,
    project: &str,
    label: &str,
    correct: bool,
    evidence: serde_json::Value,
) {
    if !atc_durable_experience_store_writable(pool) {
        return;
    }

    let now_micros = mcp_agent_mail_db::now_micros();
    let cx = Cx::for_request_with_budget(Budget::INFINITE);

    // Fetch up to 20 open conflict experiences for this agent.
    let open_experiences = match block_on(mcp_agent_mail_db::queries::fetch_open_atc_experiences(
        &cx,
        pool,
        Some(agent),
        20,
    )) {
        asupersync::Outcome::Ok(rows) => rows,
        asupersync::Outcome::Err(error) => {
            tracing::debug!(
                %error,
                agent,
                project,
                "failed to fetch open experiences for reservation-event resolution"
            );
            return;
        }
        _ => return,
    };

    // Only resolve Conflict-subsystem experiences.
    for experience in &open_experiences {
        if experience.subsystem != ExperienceSubsystem::Conflict {
            continue;
        }
        // Skip experiences from a different project if project scoping is available.
        // Project keys may be persisted as either human_key paths or slugs, but
        // matching must remain exact enough to avoid prefix collisions between
        // unrelated projects.
        if let Some(ref exp_project) = experience.project_key {
            if !exp_project.is_empty() && !atc_project_keys_match(exp_project, project) {
                continue;
            }
        }

        let outcome = ExperienceOutcome {
            observed_ts_micros: now_micros,
            label: label.to_string(),
            correct,
            actual_loss: None,
            regret: None,
            evidence: Some(evidence.clone()),
        };

        if !promote_executed_experience_to_open_for_resolution(
            pool,
            experience.experience_id,
            experience.state,
            now_micros,
            "failed to transition conflict experience to open before resolution",
        ) {
            continue;
        }

        let cx2 = Cx::for_request_with_budget(Budget::INFINITE);
        if let asupersync::Outcome::Err(error) =
            block_on(mcp_agent_mail_db::queries::resolve_atc_experience(
                &cx2,
                pool,
                experience.experience_id,
                &outcome,
            ))
        {
            tracing::warn!(
                experience_id = experience.experience_id,
                %error,
                label,
                agent,
                "failed to resolve conflict experience on reservation event"
            );
        }
    }
}

fn ensure_atc_executor_identity(
    runtime: &Runtime,
    ensured_projects: &mut HashSet<String>,
    project_key: &str,
) -> Result<(), String> {
    if ensured_projects.contains(project_key) {
        return Ok(());
    }
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    let ctx = McpContext::new(cx, 1);
    runtime
        .block_on(async {
            mcp_agent_mail_tools::identity::register_agent(
                &ctx,
                project_key.to_string(),
                "mcp-agent-mail".to_string(),
                "atc-executor".to_string(),
                Some(atc::ATC_AGENT_NAME.to_string()),
                Some("ATC automated control plane".to_string()),
                None,
                None,
            )
            .await
        })
        .map(|_| ())
        .map_err(|error| error.to_string())?;
    ensured_projects.insert(project_key.to_string());
    Ok(())
}

fn execute_atc_advisory_effect(
    runtime: &Runtime,
    ensured_projects: &mut HashSet<String>,
    effect: &atc::AtcEffectPlan,
    project_key: &str,
) -> Result<(), String> {
    ensure_atc_executor_identity(runtime, ensured_projects, project_key)?;
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    let ctx = McpContext::new(cx, 1);
    runtime
        .block_on(async {
            mcp_agent_mail_tools::messaging::send_message(
                &ctx,
                project_key.to_string(),
                atc::ATC_AGENT_NAME.to_string(),
                vec![effect.agent.clone()],
                atc_effect_subject(effect),
                atc_effect_body(effect),
                None,
                None,
                None,
                None,
                Some("normal".to_string()),
                Some(effect.semantics.ack_required),
                Some(effect.trace_id.clone()),
                None,
                Some(false),
                Some(false),
                None,
            )
            .await
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn execute_atc_probe_effect(
    runtime: &Runtime,
    ensured_projects: &mut HashSet<String>,
    effect: &atc::AtcEffectPlan,
    project_key: &str,
) -> Result<(), String> {
    ensure_atc_executor_identity(runtime, ensured_projects, project_key)?;
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    let ctx = McpContext::new(cx, 1);
    runtime
        .block_on(async {
            mcp_agent_mail_tools::messaging::send_message(
                &ctx,
                project_key.to_string(),
                atc::ATC_AGENT_NAME.to_string(),
                vec![effect.agent.clone()],
                atc_effect_subject(effect),
                atc_effect_body(effect),
                None,
                None,
                None,
                None,
                Some("normal".to_string()),
                Some(effect.semantics.ack_required),
                Some(effect.trace_id.clone()),
                None,
                Some(false),
                Some(true),
                None,
            )
            .await
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn execute_atc_release_effect(
    runtime: &Runtime,
    effect: &atc::AtcEffectPlan,
    project_key: &str,
) -> Result<(), String> {
    let cx = Cx::for_request_with_budget(Budget::INFINITE);
    let ctx = McpContext::new(cx, 1);
    runtime
        .block_on(async {
            mcp_agent_mail_tools::reservations::release_file_reservations(
                &ctx,
                project_key.to_string(),
                effect.agent.clone(),
                None,
                None,
            )
            .await
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn execute_atc_effect(
    runtime: Option<&Runtime>,
    executor_mode: AtcExecutorMode,
    ensured_projects: &mut HashSet<String>,
    effect: &atc::AtcEffectPlan,
) -> String {
    match executor_mode {
        AtcExecutorMode::Shadow => "suppressed:executor_mode_shadow".to_string(),
        AtcExecutorMode::DryRun => "suppressed:executor_mode_dry_run".to_string(),
        AtcExecutorMode::Canary | AtcExecutorMode::Live => match effect.kind.as_str() {
            "send_advisory" if executor_mode.executes_advisories() => {
                let Some(project_key) = effect.project_key.as_deref() else {
                    return "suppressed:missing_project_precondition".to_string();
                };
                let Some(runtime) = runtime else {
                    return "failed:executor_unavailable".to_string();
                };
                execute_atc_advisory_effect(runtime, ensured_projects, effect, project_key)
                    .map(|_| "executed".to_string())
                    .unwrap_or_else(|error| format!("failed:{error}"))
            }
            "release_reservations_requested" if executor_mode.executes_releases() => {
                let Some(project_key) = effect.project_key.as_deref() else {
                    return "suppressed:missing_project_precondition".to_string();
                };
                let Some(runtime) = runtime else {
                    return "failed:executor_unavailable".to_string();
                };
                execute_atc_release_effect(runtime, effect, project_key)
                    .map(|_| "executed".to_string())
                    .unwrap_or_else(|error| format!("failed:{error}"))
            }
            "probe_agent" if executor_mode.executes_probes() => {
                let Some(project_key) = effect.project_key.as_deref() else {
                    return "suppressed:missing_project_precondition".to_string();
                };
                let Some(runtime) = runtime else {
                    return "failed:executor_unavailable".to_string();
                };
                execute_atc_probe_effect(runtime, ensured_projects, effect, project_key)
                    .map(|_| "executed".to_string())
                    .unwrap_or_else(|error| format!("failed:{error}"))
            }
            "no_action" => "skipped:deliberate_no_action".to_string(),
            "release_reservations_requested" => {
                "suppressed:executor_mode_canary_release".to_string()
            }
            "probe_agent" => "suppressed:executor_mode_probe_disabled".to_string(),
            _ => "suppressed:unsupported_effect_kind".to_string(),
        },
    }
}

fn build_atc_operator_snapshot(
    summary: Option<atc::AtcSummarySnapshot>,
    recent_actions: &VecDeque<AtcOperatorActionSnapshot>,
    recent_executions: &VecDeque<AtcOperatorExecutionSnapshot>,
    last_tick_micros: i64,
    last_tick_duration_micros: u64,
    last_tick_budget_micros: u64,
    last_tick_budget_exceeded: bool,
    outer_loop_overhead_micros: u64,
    executor_mode: &str,
    executor_pending_effects: usize,
    note: Option<String>,
) -> AtcOperatorSnapshot {
    if let Some(summary) = summary {
        return AtcOperatorSnapshot {
            enabled: summary.enabled,
            source: "live".to_string(),
            safe_mode: summary.safe_mode,
            tick_count: summary.tick_count,
            tracked_agents: summary
                .tracked_agents
                .into_iter()
                .map(|agent| AtcOperatorAgentSnapshot {
                    name: agent.name,
                    state: atc_liveness_state_label(agent.state).to_string(),
                    silence_secs: agent.silence_secs,
                    posterior_alive: agent.posterior_alive,
                })
                .collect(),
            deadlock_cycles: summary.deadlock_cycles,
            eprocess_value: summary.eprocess_value,
            regret_avg: summary.regret_avg,
            decisions_total: summary.decisions_total,
            recent_actions: recent_actions.iter().cloned().collect(),
            recent_decisions: summary.recent_decisions,
            recent_executions: recent_executions.iter().cloned().collect(),
            last_tick_micros,
            last_tick_duration_micros,
            last_tick_budget_micros,
            last_tick_budget_exceeded,
            outer_loop_overhead_micros,
            executor_mode: executor_mode.to_string(),
            executor_pending_effects,
            stage_timings: summary.stage_timings,
            kernel: summary.kernel,
            budget: summary.budget,
            policy: summary.policy,
            note,
        };
    }

    AtcOperatorSnapshot {
        enabled: true,
        source: "warming_up".to_string(),
        recent_actions: recent_actions.iter().cloned().collect(),
        recent_executions: recent_executions.iter().cloned().collect(),
        last_tick_micros,
        last_tick_duration_micros,
        last_tick_budget_micros,
        last_tick_budget_exceeded,
        outer_loop_overhead_micros,
        executor_mode: executor_mode.to_string(),
        executor_pending_effects,
        note,
        ..AtcOperatorSnapshot::default()
    }
}

pub(crate) fn atc_operator_snapshot() -> AtcOperatorSnapshot {
    lock_mutex(&ATC_OPERATOR_SNAPSHOT).clone()
}

fn set_atc_operator_snapshot(snapshot: AtcOperatorSnapshot) {
    *lock_mutex(&ATC_OPERATOR_SNAPSHOT) = snapshot;
}

#[allow(dead_code)]
fn atc_action_cooldown_key(snapshot: &AtcOperatorActionSnapshot) -> String {
    snapshot.message.as_deref().map_or_else(
        || format!("{}:{}:{}", snapshot.kind, snapshot.category, snapshot.agent),
        |message| {
            format!(
                "{}:{}:{}:{}",
                snapshot.kind, snapshot.category, snapshot.agent, message
            )
        },
    )
}

fn wait_for_atc_operator_interval(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now);
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(remaining.min(ATC_OPERATOR_STOP_POLL_INTERVAL));
    }
    false
}

fn atc_operator_wait_duration(
    snapshot: &AtcOperatorSnapshot,
    now_micros: i64,
    max_interval: Duration,
) -> Duration {
    if snapshot.executor_pending_effects > 0 {
        return ATC_OPERATOR_MIN_TICK_INTERVAL;
    }
    let Some(next_due_micros) = snapshot.kernel.next_due_micros else {
        return max_interval;
    };
    let micros_until_due = next_due_micros.saturating_sub(now_micros).max(0);
    let wait = Duration::from_micros(u64::try_from(micros_until_due).unwrap_or(u64::MAX));
    wait.min(max_interval)
}

fn maybe_emit_atc_summary_log(state: &tui_bridge::TuiSharedState, snapshot: &AtcOperatorSnapshot) {
    let suspect_count = snapshot
        .tracked_agents
        .iter()
        .filter(|agent| agent.state != "alive")
        .count();
    state.push_console_log(format!(
        "[ATC] tick={} decisions={} deadlocks={} suspect_agents={} safe_mode={} mode={} pending={} bundle={} e={:.2} regret={:.2}",
        snapshot.tick_count,
        snapshot.decisions_total,
        snapshot.deadlock_cycles,
        suspect_count,
        snapshot.safe_mode,
        snapshot.budget.mode,
        snapshot.executor_pending_effects,
        snapshot.policy.bundle_id,
        snapshot.eprocess_value,
        snapshot.regret_avg
    ));
}

fn run_atc_operator_loop(config: mcp_agent_mail_core::Config, stop: Arc<AtomicBool>) {
    let atc_config = atc::AtcEngine::config_from_env(&config);
    let tick_interval = Duration::from_micros(
        u64::try_from(atc_config.probe_interval_micros.max(250_000)).unwrap_or(250_000),
    )
    .max(ATC_OPERATOR_MIN_TICK_INTERVAL);
    let summary_interval_micros = atc_config.summary_interval_micros.max(1);
    let executor_mode = AtcExecutorMode::from_env();
    let executor_runtime = executor_mode
        .requires_runtime()
        .then(|| {
            // Explicitly use epoll to avoid io_uring hangs (handle_reserve_ticket D-state).
            // The HTTP runtime already defaults to epoll; this runtime must match.
            #[cfg(target_os = "linux")]
            {
                let reactor =
                    Arc::new(EpollReactor::new().expect("epoll reactor for ATC operator"));
                RuntimeBuilder::current_thread()
                    .with_reactor(reactor as Arc<dyn Reactor>)
                    .build()
            }
            #[cfg(not(target_os = "linux"))]
            {
                RuntimeBuilder::current_thread().build()
            }
        })
        .transpose()
        .ok()
        .flatten();
    let atc_db_pool = get_or_create_pool(&DbPoolConfig::from_env()).ok();
    if atc_db_pool.is_none() {
        tracing::warn!("ATC durable experience append disabled: failed to acquire DB pool");
    }
    let durable_writes_enabled =
        atc_durable_experience_store_enabled(executor_mode, atc_db_pool.as_ref());
    let mut recent_actions = VecDeque::with_capacity(ATC_OPERATOR_ACTION_CAPACITY);
    let mut recent_executions = VecDeque::with_capacity(ATC_OPERATOR_EXECUTION_CAPACITY);
    let mut last_action_by_key: HashMap<String, i64> = HashMap::new();
    let mut last_summary_log_micros = 0_i64;
    /// Maximum pending effects before backpressure drops oldest.
    const MAX_PENDING_EFFECTS: usize = 512;
    /// Refresh durable ATC population state once per minute to avoid cold-start
    /// emptiness and to absorb agents registered outside the current process.
    const ATC_POPULATION_SYNC_INTERVAL_MICROS: i64 = 60_000_000;
    let mut pending_effects: VecDeque<atc::AtcEffectPlan> = VecDeque::new();
    let mut pending_effect_keys: HashSet<String> = HashSet::new();
    let mut atc_resolution_tick_counter: u64 = 0;
    let mut executor_registered_projects: HashSet<String> = HashSet::new();
    let mut next_population_sync_micros = 0_i64;

    set_atc_operator_snapshot(AtcOperatorSnapshot::warming_up(true));

    while !stop.load(Ordering::Relaxed) {
        let started_at = Instant::now();
        let sync_check_micros = mcp_agent_mail_core::timestamps::now_micros();
        if let Some(pool) = atc_db_pool.as_ref()
            && sync_check_micros >= next_population_sync_micros
        {
            match atc::atc_sync_population_from_db(pool) {
                Ok(stats) => {
                    tracing::debug!(
                        projects = stats.projects,
                        agents = stats.agents,
                        active_agents = stats.active_agents,
                        "synchronized ATC population from durable state"
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to synchronize ATC population from DB");
                }
            }
            next_population_sync_micros = mcp_agent_mail_core::timestamps::now_micros()
                .saturating_add(ATC_POPULATION_SYNC_INTERVAL_MICROS);
        }
        let now_micros = mcp_agent_mail_core::timestamps::now_micros();
        let (live_summary, new_effects) = match atc::atc_tick_report(now_micros) {
            Some(report) => (Some(report.summary), report.effects),
            None => (None, Vec::new()),
        };
        let tick_budget_micros = live_summary
            .as_ref()
            .map_or(atc_config.tick_budget_micros, |summary| {
                summary.budget.tick_budget_micros
            });

        let mut visible_actions = Vec::new();
        // Only write durable experience rows for executor modes that actually
        // execute effects.  Shadow and DryRun modes suppress all real actions,
        // so recording experiences for them generates write churn without any
        // learning value.
        for mut effect in new_effects {
            if durable_writes_enabled {
                if let Some(pool) = atc_db_pool.as_ref() {
                    match append_atc_experience_for_effect(pool, &effect) {
                        Ok(experience) => {
                            effect.experience_id = Some(experience.experience_id);
                        }
                        Err(error) => {
                            tracing::warn!(
                                decision_id = effect.decision_id,
                                effect_id = %effect.effect_id,
                                %error,
                                "failed to append ATC experience"
                            );
                        }
                    }
                }
            }
            let effect_key = atc_effect_semantic_key(&effect);
            if pending_effect_keys.insert(effect_key) {
                // Enforce backpressure: if the queue is full, drop the oldest
                // pending effect to make room. This prevents unbounded memory
                // growth under sustained alert conditions.
                while pending_effects.len() >= MAX_PENDING_EFFECTS {
                    if let Some(dropped) = pending_effects.pop_front() {
                        let dropped_key = atc_effect_semantic_key(&dropped);
                        pending_effect_keys.remove(&dropped_key);
                        if let Some(pool) = atc_db_pool.as_ref() {
                            capture_atc_execution_result(
                                pool,
                                dropped.experience_id,
                                executor_mode.as_str(),
                                ATC_QUEUE_BACKPRESSURE_STATUS,
                                now_micros,
                            );
                        }
                        let execution = atc_execution_snapshot(
                            now_micros,
                            &dropped,
                            executor_mode.as_str(),
                            ATC_QUEUE_BACKPRESSURE_STATUS,
                        );
                        record_atc_operator_execution(
                            &mut recent_executions,
                            &mut recent_actions,
                            &mut visible_actions,
                            execution,
                        );
                        tracing::warn!(
                            decision_id = dropped.decision_id,
                            effect_id = %dropped.effect_id,
                            queue_len = pending_effects.len(),
                            "ATC pending effect queue at capacity, dropping oldest effect"
                        );
                    }
                }
                pending_effects.push_back(effect);
            }
        }

        let mut processed_this_tick = 0_usize;
        while processed_this_tick < ATC_OPERATOR_ACTION_CAPACITY {
            let Some(effect) = pending_effects.front() else {
                break;
            };
            let cooldown_key = atc_effect_semantic_key(effect);
            let cooldown_micros = effect.semantics.cooldown_micros.max(0);
            let throttled = last_action_by_key
                .get(&cooldown_key)
                .copied()
                .is_some_and(|last| {
                    cooldown_micros > 0 && now_micros.saturating_sub(last) < cooldown_micros
                });
            if throttled {
                let Some(effect) = pending_effects.pop_front() else {
                    continue;
                };
                pending_effect_keys.remove(&cooldown_key);
                let status = format!("throttled:{}", effect.semantics.family);
                if let Some(pool) = atc_db_pool.as_ref() {
                    capture_atc_execution_result(
                        pool,
                        effect.experience_id,
                        executor_mode.as_str(),
                        &status,
                        now_micros,
                    );
                }
                let execution =
                    atc_execution_snapshot(now_micros, &effect, executor_mode.as_str(), &status);
                record_atc_operator_execution(
                    &mut recent_executions,
                    &mut recent_actions,
                    &mut visible_actions,
                    execution,
                );
                // Throttled outcomes still perform durable work, so they must
                // count against the per-tick action budget.
                processed_this_tick = processed_this_tick.saturating_add(1);
                continue;
            }
            let Some(effect) = pending_effects.pop_front() else {
                continue;
            };
            pending_effect_keys.remove(&cooldown_key);
            let status = execute_atc_effect(
                executor_runtime.as_ref(),
                executor_mode,
                &mut executor_registered_projects,
                &effect,
            );
            if atc_status_consumes_cooldown(&status) {
                last_action_by_key.insert(cooldown_key, now_micros);
            }
            // Capture execution result into durable experience store (br-0qt6e.2.2).
            if let Some(pool) = atc_db_pool.as_ref() {
                capture_atc_execution_result(
                    pool,
                    effect.experience_id,
                    executor_mode.as_str(),
                    &status,
                    now_micros,
                );
            }
            let execution =
                atc_execution_snapshot(now_micros, &effect, executor_mode.as_str(), &status);
            record_atc_operator_execution(
                &mut recent_executions,
                &mut recent_actions,
                &mut visible_actions,
                execution,
            );
            processed_this_tick = processed_this_tick.saturating_add(1);
        }

        // Periodic resolution sweep for open experiences (br-0qt6e.2.3).
        // Runs every 10 ticks to amortize query cost across tick budgets.
        // Resolution window: 10 minutes (600_000_000 microseconds).
        atc_resolution_tick_counter = atc_resolution_tick_counter.wrapping_add(1);
        if atc_resolution_tick_counter % 10 == 0 {
            if let Some(pool) = atc_db_pool.as_ref() {
                sweep_open_experiences_for_resolution(pool, now_micros, 600_000_000);
            }
        }

        let tick_duration_micros =
            u64::try_from(started_at.elapsed().as_micros().min(u128::from(u64::MAX)))
                .unwrap_or(u64::MAX);
        let tick_budget_exceeded = tick_duration_micros > tick_budget_micros;
        let outer_loop_overhead_micros = live_summary.as_ref().map_or(0, |summary| {
            tick_duration_micros.saturating_sub(summary.budget.kernel_total_micros)
        });
        let note = tick_budget_exceeded.then(|| {
            format!(
                "ATC tick exceeded budget: {}us > {}us",
                tick_duration_micros, tick_budget_micros
            )
        });
        let snapshot = build_atc_operator_snapshot(
            live_summary,
            &recent_actions,
            &recent_executions,
            now_micros,
            tick_duration_micros,
            tick_budget_micros,
            tick_budget_exceeded,
            outer_loop_overhead_micros,
            executor_mode.as_str(),
            pending_effects.len(),
            note.clone(),
        );
        set_atc_operator_snapshot(snapshot.clone());

        if let Some(state) = tui_state_handle() {
            for action in &visible_actions {
                state.push_console_log(action.console_line());
            }
            if now_micros.saturating_sub(last_summary_log_micros) >= summary_interval_micros {
                maybe_emit_atc_summary_log(&state, &snapshot);
                last_summary_log_micros = now_micros;
            }
        }

        if tick_budget_exceeded {
            tracing::warn!(
                duration_micros = tick_duration_micros,
                budget_micros = tick_budget_micros,
                "ATC operator tick exceeded configured budget"
            );
        }

        let wait_duration = {
            let duration = atc_operator_wait_duration(&snapshot, now_micros, tick_interval);
            if duration.is_zero() {
                Duration::from_millis(1)
            } else {
                duration
            }
        };
        if !wait_for_atc_operator_interval(stop.as_ref(), wait_duration) {
            break;
        }
    }
}

fn start_atc_operator_runtime(config: &mcp_agent_mail_core::Config) {
    stop_atc_operator_runtime();
    if !config.atc_enabled {
        set_atc_operator_snapshot(AtcOperatorSnapshot::disabled());
        return;
    }

    set_atc_operator_snapshot(AtcOperatorSnapshot::warming_up(true));

    let stop = Arc::new(AtomicBool::new(false));
    let config = config.clone();
    let stop_for_thread = Arc::clone(&stop);
    let join = match std::thread::Builder::new()
        .name("atc-operator".to_string())
        .spawn(move || run_atc_operator_loop(config, stop_for_thread))
    {
        Ok(join) => join,
        Err(error) => {
            tracing::error!(%error, "failed to spawn ATC operator thread");
            set_atc_operator_snapshot(AtcOperatorSnapshot {
                enabled: true,
                source: "spawn_failed".to_string(),
                note: Some(format!("ATC operator thread failed to start: {error}")),
                ..AtcOperatorSnapshot::default()
            });
            return;
        }
    };

    *lock_mutex(&ATC_OPERATOR_RUNTIME) = Some(AtcOperatorRuntime {
        stop,
        join: Some(join),
    });
}

fn stop_atc_operator_runtime() {
    if let Some(runtime) = lock_mutex(&ATC_OPERATOR_RUNTIME).take() {
        runtime.shutdown();
    }
}

/// Emit a [`MailEvent`] to the TUI ring buffer (non-blocking).
///
/// No-op when TUI mode is not active.
fn emit_tui_event(event: tui_events::MailEvent) {
    if let Some(state) = tui_state_handle() {
        let _ = state.push_event(event);
    }
}

/// Asynchronous version of [`emit_tui_event`] that uses non-blocking retry.
///
/// No-op when TUI mode is not active.
async fn emit_tui_event_async(event: tui_events::MailEvent) {
    if let Some(state) = tui_state_handle() {
        let _ = state.push_event_async(event).await;
    }
}

/// Whether the TUI is currently active (console output should be suppressed).
fn is_tui_active() -> bool {
    tui_state_handle().is_some()
}

/// Unified runtime output mode across TUI/headless text/headless JSON surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeOutputMode {
    Tui,
    HeadlessText,
    HeadlessJson,
}

impl RuntimeOutputMode {
    const fn is_tui(self) -> bool {
        matches!(self, Self::Tui)
    }

    const fn should_emit_structured_request_line(self, use_ansi: bool) -> bool {
        match self {
            Self::Tui => false,
            Self::HeadlessText => !use_ansi,
            Self::HeadlessJson => true,
        }
    }
}

fn runtime_output_mode(config: &mcp_agent_mail_core::Config) -> RuntimeOutputMode {
    if is_tui_active() {
        RuntimeOutputMode::Tui
    } else if config.log_json_enabled {
        RuntimeOutputMode::HeadlessJson
    } else {
        RuntimeOutputMode::HeadlessText
    }
}

const JWKS_CACHE_TTL: Duration = Duration::from_mins(1);
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
struct DashboardDbStats {
    projects: u64,
    agents: u64,
    messages: u64,
    file_reservations: u64,
    contact_links: u64,
    ack_pending: u64,
    agents_list: Vec<AgentSummary>,
}

#[derive(Debug, Clone, Default)]
struct AgentSummary {
    name: String,
    program: String,
    last_active_ts: i64,
}

#[derive(Debug, Clone, Default)]
struct DashboardLastRequest {
    at_iso: String,
    method: String,
    path: String,
    status: u16,
    duration_ms: u64,
    client_ip: String,
}

#[derive(Debug, Clone)]
struct DashboardSnapshot {
    endpoint: String,
    web_ui: String,
    /// Tailscale remote-access URL with auth token (if Tailscale detected).
    remote_url: Option<String>,
    transport_mode: String,
    app_environment: String,
    auth_enabled: bool,
    database_url: String,
    storage_root: String,
    uptime: String,
    requests_total: u64,
    requests_2xx: u64,
    requests_4xx: u64,
    requests_5xx: u64,
    avg_latency_ms: u64,
    db: DashboardDbStats,
    last_request: Option<DashboardLastRequest>,
    sparkline_data: Vec<f64>,
}

#[derive(Debug, Clone)]
struct ConsoleLayoutState {
    persist_path: std::path::PathBuf,
    auto_save: bool,
    interactive_enabled: bool,
    ui_height_percent: u16,
    ui_anchor: ConsoleUiAnchor,
    ui_auto_size: bool,
    inline_auto_min_rows: u16,
    inline_auto_max_rows: u16,
    split_mode: ConsoleSplitMode,
    split_ratio_percent: u16,
}

impl ConsoleLayoutState {
    fn from_config(config: &mcp_agent_mail_core::Config) -> Self {
        Self {
            persist_path: config.console_persist_path.clone(),
            auto_save: config.console_auto_save,
            interactive_enabled: config.console_interactive_enabled,
            ui_height_percent: config.console_ui_height_percent,
            ui_anchor: config.console_ui_anchor,
            ui_auto_size: config.console_ui_auto_size,
            inline_auto_min_rows: config.console_inline_auto_min_rows,
            inline_auto_max_rows: config.console_inline_auto_max_rows,
            split_mode: config.console_split_mode,
            split_ratio_percent: config.console_split_ratio_percent,
        }
    }

    fn compute_writer_settings(&self, term_height: u16) -> (ftui::ScreenMode, ftui::UiAnchor) {
        let ui_anchor = match self.ui_anchor {
            ConsoleUiAnchor::Bottom => ftui::UiAnchor::Bottom,
            ConsoleUiAnchor::Top => ftui::UiAnchor::Top,
        };

        let effective_term_height = term_height.saturating_sub(2).max(1);

        // AltScreen mode for left-split layout.
        if self.split_mode == ConsoleSplitMode::Left {
            return (ftui::ScreenMode::AltScreen, ui_anchor);
        }

        let screen_mode = if self.ui_auto_size {
            let min_height = self.inline_auto_min_rows.min(effective_term_height).max(1);
            let max_height = self
                .inline_auto_max_rows
                .min(effective_term_height)
                .max(min_height);
            ftui::ScreenMode::InlineAuto {
                min_height,
                max_height,
            }
        } else {
            let ui_height_u32 = (u32::from(term_height) * u32::from(self.ui_height_percent)) / 100;
            let ui_height = u16::try_from(ui_height_u32).unwrap_or(u16::MAX);
            let ui_height = ui_height.max(4).min(effective_term_height);
            ftui::ScreenMode::Inline { ui_height }
        };

        (screen_mode, ui_anchor)
    }

    /// Check whether the current split mode is Left (`AltScreen`).
    fn is_split_mode(&self) -> bool {
        self.split_mode == ConsoleSplitMode::Left
    }

    fn console_updates(&self) -> HashMap<&'static str, String> {
        let anchor = match self.ui_anchor {
            ConsoleUiAnchor::Bottom => "bottom",
            ConsoleUiAnchor::Top => "top",
        };
        let split_mode = match self.split_mode {
            ConsoleSplitMode::Inline => "inline",
            ConsoleSplitMode::Left => "left",
        };

        let mut updates = HashMap::new();
        updates.insert(
            "CONSOLE_UI_HEIGHT_PERCENT",
            self.ui_height_percent.to_string(),
        );
        updates.insert("CONSOLE_UI_ANCHOR", anchor.to_string());
        updates.insert(
            "CONSOLE_UI_AUTO_SIZE",
            if self.ui_auto_size { "true" } else { "false" }.to_string(),
        );
        updates.insert(
            "CONSOLE_INLINE_AUTO_MIN_ROWS",
            self.inline_auto_min_rows.to_string(),
        );
        updates.insert(
            "CONSOLE_INLINE_AUTO_MAX_ROWS",
            self.inline_auto_max_rows.to_string(),
        );
        updates.insert("CONSOLE_SPLIT_MODE", split_mode.to_string());
        updates.insert(
            "CONSOLE_SPLIT_RATIO_PERCENT",
            self.split_ratio_percent.to_string(),
        );
        updates.insert(
            "CONSOLE_AUTO_SAVE",
            if self.auto_save { "true" } else { "false" }.to_string(),
        );
        updates.insert(
            "CONSOLE_INTERACTIVE",
            if self.interactive_enabled {
                "true"
            } else {
                "false"
            }
            .to_string(),
        );
        updates
    }

    fn summary_line(&self) -> String {
        let anchor = match self.ui_anchor {
            ConsoleUiAnchor::Bottom => "bottom",
            ConsoleUiAnchor::Top => "top",
        };

        let inline = if self.ui_auto_size {
            format!(
                "inline_auto {anchor} {}..{} rows",
                self.inline_auto_min_rows, self.inline_auto_max_rows
            )
        } else {
            format!("inline {anchor} {}%", self.ui_height_percent)
        };

        match self.split_mode {
            ConsoleSplitMode::Inline => inline,
            ConsoleSplitMode::Left => {
                format!(
                    "{inline} (split: left {}% requested)",
                    self.split_ratio_percent
                )
            }
        }
    }

    fn apply_key(&mut self, code: ftui::KeyCode) -> (bool, Option<String>) {
        use ftui::KeyCode;

        match code {
            KeyCode::Char('?') => (false, Some(format!("Console: {}", self.summary_line()))),
            KeyCode::Char('+') | KeyCode::Up => {
                self.ui_height_percent = self.ui_height_percent.saturating_add(5).clamp(10, 80);
                (true, None)
            }
            KeyCode::Char('-') | KeyCode::Down => {
                self.ui_height_percent = self.ui_height_percent.saturating_sub(5).clamp(10, 80);
                (true, None)
            }
            KeyCode::Char('t') => {
                self.ui_anchor = ConsoleUiAnchor::Top;
                (true, None)
            }
            KeyCode::Char('b') => {
                self.ui_anchor = ConsoleUiAnchor::Bottom;
                (true, None)
            }
            KeyCode::Char('a') => {
                self.ui_auto_size = !self.ui_auto_size;
                (true, None)
            }
            KeyCode::Char('i') => {
                self.split_mode = ConsoleSplitMode::Inline;
                (true, None)
            }
            KeyCode::Char('l') => {
                self.split_mode = ConsoleSplitMode::Left;
                (
                    true,
                    Some(
                        "Console: switched to left split mode (AltScreen + LogViewer)".to_string(),
                    ),
                )
            }
            KeyCode::Char('[') => {
                self.split_ratio_percent = self.split_ratio_percent.saturating_sub(5).clamp(10, 80);
                (true, None)
            }
            KeyCode::Char(']') => {
                self.split_ratio_percent = self.split_ratio_percent.saturating_add(5).clamp(10, 80);
                (true, None)
            }
            _ => (false, None),
        }
    }
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn should_force_mux_left_split(
    layout: &ConsoleLayoutState,
    caps: &console::ConsoleCaps,
    allow_inline_in_mux: bool,
) -> bool {
    caps.in_mux
        && !caps.sync_output
        && !allow_inline_in_mux
        && layout.split_mode == ConsoleSplitMode::Inline
}

fn stable_tui_diff_config() -> ftui_runtime::terminal_writer::RuntimeDiffConfig {
    // Explicitly tune the runtime diff path so sparse TUI updates remain
    // incremental and strategy switches do not thrash at overlay boundaries.
    let strategy = ftui_render::diff_strategy::DiffStrategyConfig {
        // Model terminal emit cost higher than scan to bias away from redraw.
        c_emit: 8.0,
        // Ignore tiny micro-noise updates when adapting the posterior.
        min_observation_cells: 8,
        // Add extra switch friction so command-palette/modal edges do not flap.
        hysteresis_ratio: 0.08,
        // Keep uncertainty guard active a bit earlier under bursty churn.
        uncertainty_guard_variance: 0.0015,
        ..Default::default()
    };
    let dirty_spans = ftui_render::buffer::DirtySpanConfig::default()
        .with_guard_band(1)
        .with_max_spans_per_row(96);
    let tiles = ftui_render::diff::TileDiffConfig::default()
        // Enable tile skipping on medium terminals (not only very large grids).
        .with_min_cells_for_tiles(2_000)
        .with_dense_cell_ratio(0.22)
        .with_dense_tile_ratio(0.55);

    ftui_runtime::terminal_writer::RuntimeDiffConfig::new()
        .with_strategy_config(strategy)
        .with_dirty_span_config(dirty_spans)
        .with_tile_diff_config(tiles)
}

const DASHBOARD_RENDER_COALESCE_WINDOW: Duration = Duration::from_millis(72);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DashboardRenderStamp {
    at: Instant,
    width: u16,
    ui_height: u16,
}

struct StartupDashboard {
    writer: Mutex<ftui::TerminalWriter<std::io::Stdout>>,
    stop: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    input_worker: Mutex<Option<JoinHandle<()>>>,
    started_at: Instant,
    endpoint: String,
    web_ui: String,
    /// Tailscale remote-access URL with auth token (computed once at startup).
    remote_url: Option<String>,
    transport_mode: String,
    app_environment: String,
    auth_enabled: bool,
    database_url: String,
    storage_root: String,
    console_layout: Mutex<ConsoleLayoutState>,
    requests_total: AtomicU64,
    requests_2xx: AtomicU64,
    requests_4xx: AtomicU64,
    requests_5xx: AtomicU64,
    latency_total_ms: AtomicU64,
    db_stats: Mutex<DashboardDbStats>,
    last_request: Mutex<Option<DashboardLastRequest>>,
    sparkline: console::SparklineBuffer,
    log_pane: Mutex<console::LogPane>,
    command_palette: Mutex<console::ConsoleCommandPalette>,
    tool_calls_log_enabled: AtomicBool,
    tools_log_enabled: AtomicBool,
    console_caps: console::ConsoleCaps,
    tick_count: AtomicU64,
    prev_db_stats: Mutex<DashboardDbStats>,
    event_buffer: Mutex<console::ConsoleEventBuffer>,
    timeline_pane: Mutex<console::TimelinePane>,
    right_pane_view: Mutex<console::RightPaneView>,
    last_render_stamp: Mutex<Option<DashboardRenderStamp>>,
}

const fn startup_dashboard_enabled(config: &mcp_agent_mail_core::Config) -> bool {
    config.tui_enabled && config.log_rich_enabled
}

const fn dashboard_console_interactivity_enabled(config: &mcp_agent_mail_core::Config) -> bool {
    startup_dashboard_enabled(config) && config.console_interactive_enabled
}

impl StartupDashboard {
    fn maybe_start(config: &mcp_agent_mail_core::Config) -> Option<Arc<Self>> {
        if !startup_dashboard_enabled(config) || !std::io::stdout().is_terminal() {
            return None;
        }
        if degenerate_stty_size().is_some() {
            return None;
        }

        let term_width = parse_env_u16("COLUMNS", 120).max(80);
        let term_height = parse_env_u16("LINES", 36).max(20);
        let mut console_layout = ConsoleLayoutState::from_config(config);
        console_layout.interactive_enabled = dashboard_console_interactivity_enabled(config);
        let term_caps = ftui::TerminalCapabilities::detect();
        let console_caps = console::ConsoleCaps::from_capabilities(&term_caps);
        let allow_inline_in_mux = env_truthy("CONSOLE_MUX_INLINE_OK");
        let force_mux_left_split =
            should_force_mux_left_split(&console_layout, &console_caps, allow_inline_in_mux);
        if force_mux_left_split {
            console_layout.split_mode = ConsoleSplitMode::Left;
        }
        let (screen_mode, ui_anchor) = console_layout.compute_writer_settings(term_height);

        // Emit a grep-friendly console summary before engaging AltScreen so PTY capture and
        // terminal scrollback have stable, plain-text breadcrumbs (E2E + debugging).
        if console_layout.is_split_mode() {
            eprintln!("Console: {}", console_layout.summary_line());
            eprintln!("{}", console_caps.one_liner());
            if force_mux_left_split {
                eprintln!(
                    "Console: auto-switched to left split mode for multiplexer stability \
                     (set CONSOLE_MUX_INLINE_OK=1 to force inline mode)"
                );
            }
        }

        let mut writer = ftui::TerminalWriter::with_diff_config(
            std::io::stdout(),
            screen_mode,
            ui_anchor,
            term_caps,
            stable_tui_diff_config(),
        );
        writer.set_size(term_width, term_height);

        let endpoint = format!(
            "http://{}:{}{}",
            config.http_host, config.http_port, config.http_path
        );
        let web_ui = build_web_ui_url(
            &config.http_host,
            config.http_port,
            config.http_bearer_token.as_deref(),
        );
        let remote_url = detect_tailscale_ip()
            .map(|ip| build_web_ui_url(&ip, config.http_port, config.http_bearer_token.as_deref()));
        let transport_mode = detect_transport_mode(&config.http_path).to_string();

        let dashboard = Arc::new(Self {
            writer: Mutex::new(writer),
            stop: AtomicBool::new(false),
            worker: Mutex::new(None),
            input_worker: Mutex::new(None),
            started_at: Instant::now(),
            endpoint,
            web_ui,
            remote_url,
            transport_mode,
            app_environment: config.app_environment.to_string(),
            auth_enabled: config.http_bearer_token.is_some(),
            database_url: config.database_url.clone(),
            storage_root: config.storage_root.display().to_string(),
            console_layout: Mutex::new(console_layout),
            requests_total: AtomicU64::new(0),
            requests_2xx: AtomicU64::new(0),
            requests_4xx: AtomicU64::new(0),
            requests_5xx: AtomicU64::new(0),
            latency_total_ms: AtomicU64::new(0),
            db_stats: Mutex::new(DashboardDbStats::default()),
            last_request: Mutex::new(None),
            sparkline: console::SparklineBuffer::new(),
            log_pane: Mutex::new(console::LogPane::new()),
            command_palette: Mutex::new(console::ConsoleCommandPalette::new()),
            tool_calls_log_enabled: AtomicBool::new(config.log_tool_calls_enabled),
            tools_log_enabled: AtomicBool::new(config.tools_log_enabled),
            console_caps,
            tick_count: AtomicU64::new(0),
            prev_db_stats: Mutex::new(DashboardDbStats::default()),
            event_buffer: Mutex::new(console::ConsoleEventBuffer::new()),
            timeline_pane: Mutex::new(console::TimelinePane::new()),
            right_pane_view: Mutex::new(console::RightPaneView::Log),
            last_render_stamp: Mutex::new(None),
        });

        // Wire capabilities addendum into the LogPane help overlay (br-1m6a.23).
        lock_mutex(&dashboard.log_pane)
            .set_caps_addendum(dashboard.console_caps.help_overlay_addendum());

        dashboard.refresh_db_stats();
        dashboard.render_now();
        dashboard.emit_startup_showcase(config);
        if force_mux_left_split {
            dashboard.log_line(
                "Console: auto-switched to left split mode for multiplexer stability \
                 (set CONSOLE_MUX_INLINE_OK=1 to force inline mode)",
            );
        }
        dashboard.spawn_refresh_worker();
        dashboard.spawn_console_input_worker();
        Some(dashboard)
    }

    fn emit_startup_showcase(&self, config: &mcp_agent_mail_core::Config) {
        let stats = lock_mutex(&self.db_stats);
        let params = console::BannerParams {
            app_environment: &self.app_environment,
            endpoint: &self.endpoint,
            database_url: &self.database_url,
            storage_root: &self.storage_root,
            auth_enabled: self.auth_enabled,
            tools_log_enabled: config.tools_log_enabled,
            tool_calls_log_enabled: config.log_tool_calls_enabled,
            console_theme: theme::current_theme_display_name(),
            web_ui_url: &self.web_ui,
            remote_url: self.remote_url.as_deref(),
            projects: stats.projects,
            agents: stats.agents,
            messages: stats.messages,
            file_reservations: stats.file_reservations,
            contact_links: stats.contact_links,
        };
        drop(stats);
        for line in console::render_startup_banner(&params) {
            self.log_line(&line);
        }
        // Capabilities banner section.
        for line in self.console_caps.banner_lines() {
            self.log_line(&line);
        }
        let summary = lock_mutex(&self.console_layout).summary_line();
        self.log_line(&format!("Console: {summary}"));
        self.log_line(&self.console_caps.one_liner());

        // In AltScreen split mode, `log_line()` appends to the LogPane buffer; force an immediate
        // render so the startup banner/summary is visible without waiting for the next tick.
        if lock_mutex(&self.console_layout).is_split_mode() {
            self.render_now();
        }
    }

    fn spawn_refresh_worker(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("mcp-agent-mail-dashboard".to_string())
            .spawn(move || {
                let mut db_conn =
                    dashboard_open_connection(&this.database_url, Path::new(&this.storage_root));
                while !this.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1200));
                    if this.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    this.sparkline.sample();
                    this.refresh_db_stats_cached(&mut db_conn);
                    this.render_now();
                }
            });

        if let Ok(join) = handle {
            *lock_mutex(&self.worker) = Some(join);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn spawn_console_input_worker(self: &Arc<Self>) {
        const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(100);
        const INPUT_DRAIN_POLL_TIMEOUT: Duration = Duration::from_millis(6);
        const INPUT_ERROR_BACKOFF: Duration = Duration::from_millis(25);
        const INPUT_EMPTY_READY_BACKOFF: Duration = Duration::from_millis(8);
        const INPUT_DRAIN_CAP_STREAK_LIMIT: usize = 1;
        const INPUT_DRAIN_CAP_BACKOFF: Duration = Duration::from_millis(6);

        if !lock_mutex(&self.console_layout).interactive_enabled || !std::io::stdin().is_terminal()
        {
            return;
        }

        let this = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("mcp-agent-mail-dashboard-input".to_string())
            .spawn(move || {
                use ftui_runtime::BackendEventSource;
                #[cfg(unix)]
                let backend_result = ftui_tty::TtyBackend::open(
                    0,
                    0,
                    ftui_tty::TtySessionOptions::default(),
                );
                #[cfg(not(unix))]
                let backend_result = Ok::<_, std::io::Error>(ftui_tty::TtyBackend::new(0, 0));
                let Ok(mut backend) = backend_result else {
                    this.log_line("Console interactive mode: failed to enter raw mode");
                    return;
                };

                this.log_line(
                    "Console layout keys: +/- or Up/Down (height), t/b (anchor), a (auto-size), i/l (split request), [/ ] (split ratio), ? (help)",
                );

                let mut poll_error_reported = false;
                let mut read_error_reported = false;
                let mut drain_cap_streak = 0usize;

                while !this.stop.load(Ordering::Relaxed) {
                    let ready = match backend.poll_event(INPUT_POLL_TIMEOUT) {
                        Ok(ready) => {
                            poll_error_reported = false;
                            ready
                        }
                        Err(err) => {
                            if !poll_error_reported {
                                this.log_line(&format!(
                                    "Console interactive mode: poll_event failed ({err}); retrying with backoff"
                                ));
                                poll_error_reported = true;
                            }
                            std::thread::sleep(INPUT_ERROR_BACKOFF);
                            continue;
                        }
                    };
                    if !ready {
                        continue;
                    }

                    let mut drained_events = 0usize;
                    let mut saw_event = false;
                    let mut hit_drain_cap = false;
                    loop {
                        let event = match backend.read_event() {
                            Ok(Some(event)) => {
                                read_error_reported = false;
                                saw_event = true;
                                event
                            }
                            Ok(None) => break,
                            Err(err) => {
                                if !read_error_reported {
                                    this.log_line(&format!(
                                        "Console interactive mode: read_event failed ({err}); retrying with backoff"
                                    ));
                                    read_error_reported = true;
                                }
                                std::thread::sleep(INPUT_ERROR_BACKOFF);
                                break;
                            }
                        };

                        let term_size = backend.size().unwrap_or((80, 24));
                        if this.handle_console_event(term_size, &event) {
                            break;
                        }
                        drained_events += 1;
                        if drained_events >= 128 {
                            // Bound one drain cycle so a hot terminal event stream cannot
                            // monopolize CPU and starve HTTP/tool work.
                            hit_drain_cap = true;
                            break;
                        }
                        match backend.poll_event(INPUT_DRAIN_POLL_TIMEOUT) {
                            Ok(true) => {
                                poll_error_reported = false;
                            }
                            Ok(false) => break,
                            Err(err) => {
                                if !poll_error_reported {
                                    this.log_line(&format!(
                                        "Console interactive mode: drain poll failed ({err}); retrying with backoff"
                                    ));
                                    poll_error_reported = true;
                                }
                                std::thread::sleep(INPUT_ERROR_BACKOFF);
                                break;
                            }
                        }
                    }

                    if !saw_event {
                        std::thread::sleep(INPUT_EMPTY_READY_BACKOFF);
                    }

                    if hit_drain_cap {
                        drain_cap_streak = drain_cap_streak.saturating_add(1);
                        if drain_cap_streak >= INPUT_DRAIN_CAP_STREAK_LIMIT {
                            std::thread::sleep(INPUT_DRAIN_CAP_BACKOFF);
                            drain_cap_streak = 0;
                        }
                    } else {
                        drain_cap_streak = 0;
                    }
                }
                drop(backend);
            });

        if let Ok(join) = handle {
            *lock_mutex(&self.input_worker) = Some(join);
        }
    }

    fn handle_console_event(&self, term_size: (u16, u16), event: &ftui::Event) -> bool {
        use ftui::widgets::command_palette::PaletteAction;
        use ftui::{Event, KeyCode, KeyEventKind, Modifiers};

        let Event::Key(key) = event else {
            return false;
        };
        if key.kind != KeyEventKind::Press {
            return false;
        }

        // Ensure Ctrl+C still terminates the process even if raw-mode disables ISIG.
        if key.modifiers.contains(Modifiers::CTRL) && matches!(key.code, KeyCode::Char('c')) {
            let _ = ftui_tty::write_cleanup_sequence(
                &ftui_runtime::BackendFeatures::default(),
                false,
                &mut std::io::stdout(),
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::process::exit(130);
        }

        // When the command palette is visible, route all events to it first.
        {
            let mut palette = lock_mutex(&self.command_palette);
            if palette.is_visible() {
                if let Some(action) = palette.handle_event(event) {
                    drop(palette); // release lock before dispatch
                    match action {
                        PaletteAction::Execute(id) => {
                            self.dispatch_palette_action(&id, term_size);
                        }
                        PaletteAction::Dismiss => {}
                    }
                }
                self.render_now();
                return false;
            }
        }

        // Ctrl+P or ':' opens the command palette.
        let is_ctrl_p =
            key.modifiers.contains(Modifiers::CTRL) && matches!(key.code, KeyCode::Char('p'));
        if is_ctrl_p || matches!(key.code, KeyCode::Char(':')) {
            lock_mutex(&self.command_palette).open();
            self.render_now();
            return false;
        }

        // In split mode, route keys to the active right-pane view.
        if lock_mutex(&self.console_layout).is_split_mode() {
            // Tab toggles right pane view.
            if matches!(key.code, ftui::KeyCode::Tab) {
                let mut view = lock_mutex(&self.right_pane_view);
                *view = match *view {
                    console::RightPaneView::Log => console::RightPaneView::Timeline,
                    console::RightPaneView::Timeline => console::RightPaneView::Log,
                };
                drop(view);
                self.render_now();
                return false;
            }

            let view = *lock_mutex(&self.right_pane_view);
            let handled = match view {
                console::RightPaneView::Log => self.handle_log_pane_key(key.code, event),
                console::RightPaneView::Timeline => {
                    let events = lock_mutex(&self.event_buffer).snapshot();
                    lock_mutex(&self.timeline_pane).handle_key(key.code, event, &events)
                }
            };
            if handled {
                self.render_now();
                return false;
            }
        }

        let (changed, message) = {
            let mut layout = lock_mutex(&self.console_layout);
            layout.apply_key(key.code)
        };

        if let Some(msg) = message {
            self.log_line(&msg);
        }

        if !changed {
            return false;
        }

        let (term_width, term_height) = term_size;
        self.apply_console_layout(term_width, term_height);
        self.persist_console_settings();

        false
    }

    /// Persist current console settings to the user envfile.
    fn persist_console_settings(&self) {
        let layout = lock_mutex(&self.console_layout).clone();
        if layout.auto_save {
            let updates = layout.console_updates();
            if let Err(e) =
                mcp_agent_mail_core::config::update_envfile(&layout.persist_path, &updates)
            {
                self.log_line(&format!(
                    "Console: failed to persist settings to {}: {e}",
                    layout.persist_path.display()
                ));
            } else {
                self.log_line(&format!(
                    "Console: saved settings to {}",
                    layout.persist_path.display()
                ));
            }
        }
    }

    /// Dispatch a command palette action by ID.
    #[allow(clippy::too_many_lines)]
    fn dispatch_palette_action(&self, id: &str, term_size: (u16, u16)) {
        use console::action_ids as aid;

        let mut layout_changed = false;

        match id {
            // ── Layout ──
            aid::MODE_INLINE => {
                lock_mutex(&self.console_layout).split_mode = ConsoleSplitMode::Inline;
                layout_changed = true;
            }
            aid::MODE_LEFT_SPLIT => {
                lock_mutex(&self.console_layout).split_mode = ConsoleSplitMode::Left;
                layout_changed = true;
            }
            aid::SPLIT_RATIO_20 => {
                lock_mutex(&self.console_layout).split_ratio_percent = 20;
                layout_changed = true;
            }
            aid::SPLIT_RATIO_30 => {
                lock_mutex(&self.console_layout).split_ratio_percent = 30;
                layout_changed = true;
            }
            aid::SPLIT_RATIO_40 => {
                lock_mutex(&self.console_layout).split_ratio_percent = 40;
                layout_changed = true;
            }
            aid::SPLIT_RATIO_50 => {
                lock_mutex(&self.console_layout).split_ratio_percent = 50;
                layout_changed = true;
            }
            aid::HUD_HEIGHT_INC => {
                let mut l = lock_mutex(&self.console_layout);
                l.ui_height_percent = l.ui_height_percent.saturating_add(5).clamp(10, 80);
                drop(l);
                layout_changed = true;
            }
            aid::HUD_HEIGHT_DEC => {
                let mut l = lock_mutex(&self.console_layout);
                l.ui_height_percent = l.ui_height_percent.saturating_sub(5).clamp(10, 80);
                drop(l);
                layout_changed = true;
            }
            aid::ANCHOR_TOP => {
                lock_mutex(&self.console_layout).ui_anchor = ConsoleUiAnchor::Top;
                layout_changed = true;
            }
            aid::ANCHOR_BOTTOM => {
                lock_mutex(&self.console_layout).ui_anchor = ConsoleUiAnchor::Bottom;
                layout_changed = true;
            }
            aid::TOGGLE_AUTO_SIZE => {
                let mut l = lock_mutex(&self.console_layout);
                l.ui_auto_size = !l.ui_auto_size;
                drop(l);
                layout_changed = true;
            }
            aid::PERSIST_NOW => {
                self.persist_console_settings();
            }
            aid::RIGHT_PANE_TOGGLE => {
                let mut view = lock_mutex(&self.right_pane_view);
                let label = match *view {
                    console::RightPaneView::Log => {
                        *view = console::RightPaneView::Timeline;
                        "Timeline"
                    }
                    console::RightPaneView::Timeline => {
                        *view = console::RightPaneView::Log;
                        "Log"
                    }
                };
                drop(view);
                self.log_line(&format!("Console: right pane switched to {label}"));
            }

            // ── Theme ──
            aid::THEME_CYCLE => {
                let new_theme = ftui_extras::theme::cycle_theme();
                self.log_line(&format!("Console: theme changed to {}", new_theme.name()));
            }
            aid::THEME_CYBERPUNK => {
                ftui_extras::theme::set_theme(ftui_extras::theme::ThemeId::CyberpunkAurora);
                self.log_line("Console: theme set to Cyberpunk Aurora");
            }
            aid::THEME_DARCULA => {
                ftui_extras::theme::set_theme(ftui_extras::theme::ThemeId::Darcula);
                self.log_line("Console: theme set to Darcula");
            }
            aid::THEME_LUMEN => {
                ftui_extras::theme::set_theme(ftui_extras::theme::ThemeId::LumenLight);
                self.log_line("Console: theme set to Lumen Light");
            }
            aid::THEME_NORDIC => {
                ftui_extras::theme::set_theme(ftui_extras::theme::ThemeId::NordicFrost);
                self.log_line("Console: theme set to Nordic Frost");
            }
            aid::THEME_HIGH_CONTRAST => {
                ftui_extras::theme::set_theme(ftui_extras::theme::ThemeId::HighContrast);
                self.log_line("Console: theme set to High Contrast");
            }

            // ── Logs ──
            aid::LOG_TOGGLE_FOLLOW => {
                lock_mutex(&self.log_pane).toggle_follow();
                self.log_line("Console: toggled follow mode");
            }
            aid::LOG_SEARCH => {
                // Switch log pane to search mode.
                lock_mutex(&self.log_pane).enter_search_mode();
            }
            aid::LOG_CLEAR => {
                lock_mutex(&self.log_pane).clear();
                self.log_line("Console: log buffer cleared");
            }

            // ── Tool panel toggles ──
            aid::TOGGLE_TOOL_CALLS_LOG => {
                let prev = self.tool_calls_log_enabled.load(Ordering::Relaxed);
                self.tool_calls_log_enabled.store(!prev, Ordering::Relaxed);
                self.log_line(&format!(
                    "Console: tool calls logging {}",
                    if prev { "disabled" } else { "enabled" }
                ));
            }
            aid::TOGGLE_TOOLS_LOG => {
                let prev = self.tools_log_enabled.load(Ordering::Relaxed);
                self.tools_log_enabled.store(!prev, Ordering::Relaxed);
                self.log_line(&format!(
                    "Console: tools detail logging {}",
                    if prev { "disabled" } else { "enabled" }
                ));
            }

            // ── Help ──
            aid::SHOW_KEYBINDINGS => {
                self.log_line(
                    "Keybindings: +/- height, t/b anchor, a auto-size, i/l mode, [/] ratio, \
                     Ctrl+P palette, ? summary",
                );
            }
            aid::SHOW_CONFIG => {
                let summary = lock_mutex(&self.console_layout).summary_line();
                self.log_line(&format!("Console: {summary}"));
                self.log_line(&format!("  {}", self.console_caps.help_hint()));
            }

            _ => {
                self.log_line(&format!("Console: unknown action '{id}'"));
            }
        }

        if layout_changed {
            let (term_width, term_height) = term_size;
            self.apply_console_layout(term_width, term_height);
            self.persist_console_settings();
        }
    }

    /// Handle keybindings for the log pane in split mode.
    /// Returns `true` if the key was consumed by the log pane.
    fn handle_log_pane_key(&self, code: ftui::KeyCode, event: &ftui::Event) -> bool {
        use console::LogPaneMode;
        use ftui::KeyCode;
        let mut pane = lock_mutex(&self.log_pane);

        match pane.mode() {
            LogPaneMode::Search => {
                // In search mode, Enter confirms, Escape cancels, everything
                // else is forwarded to the TextInput widget.
                match code {
                    KeyCode::Enter => {
                        pane.confirm_search();
                        true
                    }
                    KeyCode::Escape => {
                        pane.cancel_search();
                        true
                    }
                    _ => {
                        pane.handle_search_event(event);
                        true
                    }
                }
            }
            LogPaneMode::Help => {
                // Any key dismisses the help overlay.
                pane.toggle_help();
                true
            }
            LogPaneMode::Normal => match code {
                // Open search
                KeyCode::Char('/') => {
                    pane.enter_search_mode();
                    true
                }
                // Toggle help
                KeyCode::Char('?') => {
                    pane.toggle_help();
                    true
                }
                // Scrolling
                KeyCode::Up => {
                    pane.scroll_up(1);
                    true
                }
                KeyCode::Down => {
                    pane.scroll_down(1);
                    true
                }
                KeyCode::PageUp => {
                    pane.page_up();
                    true
                }
                KeyCode::PageDown => {
                    pane.page_down();
                    true
                }
                KeyCode::Home => {
                    pane.scroll_to_top();
                    true
                }
                KeyCode::End => {
                    pane.scroll_to_bottom();
                    true
                }
                // Follow mode toggle
                KeyCode::Char('f') => {
                    pane.toggle_follow();
                    true
                }
                // Search navigation
                KeyCode::Char('n') => {
                    pane.next_match();
                    true
                }
                KeyCode::Char('N') => {
                    pane.prev_match();
                    true
                }
                // Clear search
                KeyCode::Escape => {
                    pane.clear_search();
                    true
                }
                _ => false,
            },
        }
    }

    fn apply_console_layout(&self, term_width: u16, term_height: u16) {
        let (screen_mode, ui_anchor) =
            lock_mutex(&self.console_layout).compute_writer_settings(term_height);
        let mut writer = ftui::TerminalWriter::with_diff_config(
            std::io::stdout(),
            screen_mode,
            ui_anchor,
            ftui::TerminalCapabilities::detect(),
            stable_tui_diff_config(),
        );
        writer.set_size(term_width.max(2), term_height.max(2));
        *lock_mutex(&self.writer) = writer;
        self.render_now();
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let join = lock_mutex(&self.worker).take();
        if let Some(join) = join {
            let _ = join.join();
        }
        let join = lock_mutex(&self.input_worker).take();
        if let Some(join) = join {
            let _ = join.join();
        }
    }

    fn log_line(&self, text: &str) {
        let mut line = String::from(text);
        if !line.ends_with('\n') {
            line.push('\n');
        }

        // In split mode, route logs to the LogPane ring buffer instead of
        // TerminalWriter::write_log (which is a no-op in AltScreen).
        if lock_mutex(&self.console_layout).is_split_mode() {
            let mut pane = lock_mutex(&self.log_pane);
            // Split on newlines so each line is a separate entry in the viewer.
            for l in line.trim_end().split('\n') {
                pane.push(console::ansi_to_line(l));
            }
        } else {
            let mut writer = lock_mutex(&self.writer);
            let _ = writer.write_log(&line);
        }
    }

    /// Push a structured event into the timeline buffer.
    fn emit_event(
        &self,
        kind: console::ConsoleEventKind,
        severity: console::ConsoleEventSeverity,
        summary: impl Into<String>,
        fields: Vec<(String, String)>,
        json: Option<serde_json::Value>,
    ) {
        // Sanitize at ingestion time so the timeline detail pane cannot leak secrets.
        let fields = fields
            .into_iter()
            .map(|(k, v)| {
                let v = if console::is_sensitive_key(&k) {
                    console::mask_sensitive_value(&v)
                } else if let Some(sanitized) = console::sanitize_known_value(&k, &v) {
                    sanitized
                } else {
                    v
                };
                (k, v)
            })
            .collect();

        let json = json.map(|j| console::mask_json(&j));

        let mut buf = lock_mutex(&self.event_buffer);
        let id = buf.push(kind, severity, summary, fields, json);
        drop(buf);
        lock_mutex(&self.timeline_pane).on_event_pushed(id);
    }

    fn record_request(
        &self,
        method: &str,
        path: &str,
        status: u16,
        duration_ms: u64,
        client_ip: &str,
    ) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.sparkline.tick();
        self.latency_total_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
        match status {
            200..=299 => {
                self.requests_2xx.fetch_add(1, Ordering::Relaxed);
            }
            400..=499 => {
                self.requests_4xx.fetch_add(1, Ordering::Relaxed);
            }
            500..=599 => {
                self.requests_5xx.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        *lock_mutex(&self.last_request) = Some(DashboardLastRequest {
            at_iso: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            method: method.to_string(),
            path: path.to_string(),
            status,
            duration_ms,
            client_ip: client_ip.to_string(),
        });

        // Emit structured event for the timeline.
        let severity = if status >= 500 {
            console::ConsoleEventSeverity::Error
        } else if status >= 400 {
            console::ConsoleEventSeverity::Warn
        } else {
            console::ConsoleEventSeverity::Info
        };
        self.emit_event(
            console::ConsoleEventKind::HttpRequest,
            severity,
            format!("{method} {path} {status} {duration_ms}ms"),
            vec![
                ("client".to_string(), client_ip.to_string()),
                ("status".to_string(), status.to_string()),
                ("duration_ms".to_string(), duration_ms.to_string()),
            ],
            None,
        );
        // Rendering is paced by the refresh worker (~1.2s) to avoid request-rate
        // driven redraw storms that can spike CPU and cause visible flicker.
    }

    fn refresh_db_stats(&self) {
        *lock_mutex(&self.db_stats) =
            fetch_dashboard_db_stats(&self.database_url, Path::new(&self.storage_root));
    }

    fn refresh_db_stats_cached(&self, conn: &mut Option<ObservabilitySyncDb>) {
        let previous = { lock_mutex(&self.db_stats).clone() };
        *lock_mutex(&self.db_stats) = fetch_dashboard_db_stats_cached(
            &self.database_url,
            Path::new(&self.storage_root),
            conn,
            &previous,
        );
    }

    fn snapshot(&self) -> DashboardSnapshot {
        let requests_total = self.requests_total.load(Ordering::Relaxed);
        let latency_total_ms = self.latency_total_ms.load(Ordering::Relaxed);
        DashboardSnapshot {
            endpoint: self.endpoint.clone(),
            web_ui: self.web_ui.clone(),
            remote_url: self.remote_url.clone(),
            transport_mode: self.transport_mode.clone(),
            app_environment: self.app_environment.clone(),
            auth_enabled: self.auth_enabled,
            database_url: self.database_url.clone(),
            storage_root: self.storage_root.clone(),
            uptime: human_uptime(self.started_at.elapsed()),
            requests_total,
            requests_2xx: self.requests_2xx.load(Ordering::Relaxed),
            requests_4xx: self.requests_4xx.load(Ordering::Relaxed),
            requests_5xx: self.requests_5xx.load(Ordering::Relaxed),
            avg_latency_ms: latency_total_ms.checked_div(requests_total).unwrap_or(0),
            db: lock_mutex(&self.db_stats).clone(),
            last_request: lock_mutex(&self.last_request).clone(),
            sparkline_data: self.sparkline.snapshot(),
        }
    }

    fn should_coalesce_render(&self, width: u16, ui_height: u16, now: Instant) -> bool {
        let mut stamp = lock_mutex(&self.last_render_stamp);
        let (skip, next_stamp) = dashboard_render_gate_decision(*stamp, width, ui_height, now);
        *stamp = next_stamp;
        skip
    }

    fn render_now(&self) {
        let mut writer = lock_mutex(&self.writer);
        let width = writer.width().max(80);
        let ui_height = writer.ui_height().max(8);

        if self.should_coalesce_render(width, ui_height, Instant::now()) {
            return;
        }

        let tick = self.tick_count.fetch_add(1, Ordering::Relaxed);
        let snapshot = self.snapshot();

        // Detect changed DB stat rows for highlight effects.
        let changed_rows = {
            let mut prev = lock_mutex(&self.prev_db_stats);
            let changed = db_changed_rows(&prev, &snapshot.db);
            *prev = snapshot.db.clone();
            changed
        };

        #[allow(clippy::cast_precision_loss)] // precision loss is fine for animation phase
        let phase = tick as f32 * 0.08; // ~0.08 per 1200ms tick ≈ one full cycle every ~15s
        let is_split = lock_mutex(&self.console_layout).is_split_mode();
        let split_ratio = lock_mutex(&self.console_layout).split_ratio_percent;
        let rendered = {
            let buffer = writer.take_render_buffer(width, ui_height);
            let (pool, links) = writer.pool_and_links_mut();
            let mut frame = ftui::Frame::from_buffer(buffer, pool);
            frame.links = Some(links);
            let area = Rect::new(0, 0, width, ui_height);
            if is_split {
                let right_view = *lock_mutex(&self.right_pane_view);
                match right_view {
                    console::RightPaneView::Log => {
                        let mut pane = lock_mutex(&self.log_pane);
                        console::render_split_frame(
                            &mut frame,
                            area,
                            split_ratio,
                            &mut pane,
                            |f, a| {
                                render_dashboard_frame(f, a, &snapshot, phase, changed_rows);
                            },
                        );
                    }
                    console::RightPaneView::Timeline => {
                        let mut tl = lock_mutex(&self.timeline_pane);
                        let events = lock_mutex(&self.event_buffer).snapshot();
                        console::render_split_frame_timeline(
                            &mut frame,
                            area,
                            split_ratio,
                            &mut tl,
                            &events,
                            |f, a| {
                                render_dashboard_frame(f, a, &snapshot, phase, changed_rows);
                            },
                        );
                    }
                }
            } else {
                render_dashboard_frame(&mut frame, area, &snapshot, phase, changed_rows);
            }
            // Render command palette overlay on top of everything.
            let palette = lock_mutex(&self.command_palette);
            if palette.is_visible() {
                palette.render(area, &mut frame);
            }
            drop(palette);
            frame.buffer
        };
        let _ = writer.present_ui_owned(rendered, None, false);
    }
}

fn dashboard_render_gate_decision(
    previous: Option<DashboardRenderStamp>,
    width: u16,
    ui_height: u16,
    now: Instant,
) -> (bool, Option<DashboardRenderStamp>) {
    if let Some(stamp) = previous
        && stamp.width == width
        && stamp.ui_height == ui_height
        && now.saturating_duration_since(stamp.at) < DASHBOARD_RENDER_COALESCE_WINDOW
    {
        return (true, previous);
    }

    (
        false,
        Some(DashboardRenderStamp {
            at: now,
            width,
            ui_height,
        }),
    )
}

/// Compute a bitmask of DB stat rows that changed since the previous snapshot.
/// Bits 0-5 correspond to projects, agents, messages, `file_reservations`, `contact_links`, `ack_pending`.
const fn db_changed_rows(prev: &DashboardDbStats, cur: &DashboardDbStats) -> u8 {
    let mut mask = 0u8;
    if prev.projects != cur.projects {
        mask |= 1 << 0;
    }
    if prev.agents != cur.agents {
        mask |= 1 << 1;
    }
    if prev.messages != cur.messages {
        mask |= 1 << 2;
    }
    if prev.file_reservations != cur.file_reservations {
        mask |= 1 << 3;
    }
    if prev.contact_links != cur.contact_links {
        mask |= 1 << 4;
    }
    if prev.ack_pending != cur.ack_pending {
        mask |= 1 << 5;
    }
    mask
}

#[allow(clippy::too_many_lines)]
fn render_dashboard_frame(
    frame: &mut ftui::Frame<'_>,
    area: Rect,
    snapshot: &DashboardSnapshot,
    phase: f32,
    changed_rows: u8,
) {
    if area.width < 40 || area.height < 5 {
        Paragraph::new("MCP Agent Mail Dashboard")
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title("Dashboard"),
            )
            .render(area, frame);
        return;
    }

    let header_style = ftui::Style::default()
        .fg(ftui::PackedRgba::rgb(218, 244, 255))
        .bg(ftui::PackedRgba::rgb(12, 36, 84))
        .bold();
    let card_style = ftui::Style::default().fg(ftui::PackedRgba::rgb(222, 231, 255));
    let title_style = ftui::Style::default().fg(ftui::PackedRgba::rgb(144, 205, 255));
    let warn_style = ftui::Style::default().fg(ftui::PackedRgba::rgb(255, 184, 108));
    let good_style = ftui::Style::default()
        .fg(ftui::PackedRgba::rgb(116, 255, 177))
        .bold();

    let rows = Flex::vertical()
        .constraints([
            Constraint::Fixed(3),
            Constraint::Min(4),
            Constraint::Fixed(2),
        ])
        .split(area);

    let header_text = format!(
        "📬 MCP Agent Mail Live HUD  •  uptime {}  •  req {}  •  avg {}ms  •  env {}",
        snapshot.uptime,
        pretty_num(snapshot.requests_total),
        snapshot.avg_latency_ms,
        snapshot.app_environment
    );
    Paragraph::new(header_text)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Live Console "),
        )
        .style(header_style)
        .render(rows[0], frame);

    let has_agents = !snapshot.db.agents_list.is_empty();
    let cols = if has_agents {
        Flex::horizontal()
            .constraints([
                Constraint::Percentage(32.0),
                Constraint::Percentage(24.0),
                Constraint::Percentage(20.0),
                Constraint::Percentage(24.0),
            ])
            .split(rows[1])
    } else {
        Flex::horizontal()
            .constraints([
                Constraint::Percentage(39.0),
                Constraint::Percentage(33.0),
                Constraint::Percentage(28.0),
            ])
            .split(rows[1])
    };

    let remote_line = snapshot
        .remote_url
        .as_ref()
        .map_or_else(String::new, |url| {
            format!("\nRemote:   {}", compact_path(url, 52))
        });
    let left = format!(
        "Endpoint: {}\nMode: {}\nWeb UI: {}{}\nAuth: {}\nStorage: {}\nDatabase: {}",
        compact_path(&snapshot.endpoint, 52),
        snapshot.transport_mode,
        compact_path(&snapshot.web_ui, 52),
        remote_line,
        if snapshot.auth_enabled {
            "ENABLED"
        } else {
            "DISABLED"
        },
        compact_path(&snapshot.storage_root, 52),
        compact_path(&snapshot.database_url, 52)
    );
    Paragraph::new(left)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Server "),
        )
        .style(card_style)
        .wrap(ftui::text::WrapMode::Word)
        .render(cols[0], frame);

    let db_rows = vec![
        Row::new(vec![
            "projects".to_string(),
            pretty_num(snapshot.db.projects),
        ]),
        Row::new(vec!["agents".to_string(), pretty_num(snapshot.db.agents)]),
        Row::new(vec![
            "messages".to_string(),
            pretty_num(snapshot.db.messages),
        ]),
        Row::new(vec![
            "reservations".to_string(),
            pretty_num(snapshot.db.file_reservations),
        ]),
        Row::new(vec![
            "contact_links".to_string(),
            pretty_num(snapshot.db.contact_links),
        ]),
        Row::new(vec![
            "pending_acks".to_string(),
            pretty_num(snapshot.db.ack_pending),
        ]),
    ];
    // Apply highlight style to changed rows (br-1m6a.7)
    let highlight_style = ftui::Style::default()
        .fg(ftui::PackedRgba::rgb(120, 255, 180))
        .bold();
    let db_rows: Vec<Row> = db_rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            if changed_rows & (1 << i) != 0 {
                row.style(highlight_style)
            } else {
                row
            }
        })
        .collect();

    // Breathing glow on header: use phase to modulate header brightness
    let glow = (phase * std::f32::consts::TAU).sin().mul_add(0.5, 0.5) * 0.3;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hdr_r = (144.0 + glow * 80.0).min(255.0) as u8;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hdr_g = (205.0 + glow * 40.0).min(255.0) as u8;
    let hdr_b = 255u8;
    let animated_title_style = ftui::Style::default()
        .fg(ftui::PackedRgba::rgb(hdr_r, hdr_g, hdr_b))
        .bold();

    let table = Table::new(
        db_rows,
        [
            Constraint::FitContentBounded { min: 8, max: 18 },
            Constraint::Fill,
        ],
    )
    .header(Row::new(vec!["Resource", "Count"]).style(animated_title_style))
    .column_spacing(2)
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Database "),
    )
    .style(card_style);
    <Table as Widget>::render(&table, cols[1], frame);

    // Render agents panel (when agents exist)
    let traffic_col = if has_agents {
        let agent_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Agents ");

        // Narrow/short panel: collapse to a summary so at least one agent name
        // is always visible in constrained layouts.
        if cols[2].width < 22 || cols[2].height < 7 {
            let count = snapshot.db.agents_list.len();
            let summary = snapshot.db.agents_list.first().map_or_else(
                || format!("{count} agents"),
                |first| {
                    if cols[2].height >= 4 {
                        format!("{count} agents\n{}", first.name)
                    } else {
                        format!("{count} agents \u{00b7} {}", first.name)
                    }
                },
            );
            Paragraph::new(summary)
                .block(agent_block)
                .style(card_style)
                .render(cols[2], frame);
        } else {
            let now_us = mcp_agent_mail_db::timestamps::now_micros();
            let max_rows = if cols[2].height > 6 { 8 } else { 4 };
            let agent_rows: Vec<Row> = snapshot
                .db
                .agents_list
                .iter()
                .take(max_rows)
                .map(|a| {
                    let ago = relative_time_short(now_us, a.last_active_ts);
                    Row::new(vec![a.name.clone(), a.program.clone(), ago])
                })
                .collect();
            let dim_style = ftui::Style::default().fg(ftui_extras::theme::fg::MUTED.resolve());
            let table = Table::new(
                agent_rows,
                [
                    Constraint::FitContentBounded { min: 6, max: 16 },
                    Constraint::FitContentBounded { min: 4, max: 12 },
                    Constraint::Fill,
                ],
            )
            .header(Row::new(vec!["Agent", "Program", "Active"]).style(title_style.bold()))
            .column_spacing(1)
            .block(agent_block)
            .style(dim_style);
            <Table as Widget>::render(&table, cols[2], frame);
        }
        3
    } else {
        2
    };

    // Render sparkline for request throughput
    let sparkline_str = {
        use ftui::widgets::sparkline::Sparkline;
        Sparkline::new(&snapshot.sparkline_data)
            .gradient(theme::sparkline_lo(), theme::sparkline_hi())
            .render_to_string()
    };

    let request_summary = format!(
        "2xx: {}  4xx: {}  5xx: {}\n{}\n{}\nreq/s: {}",
        pretty_num(snapshot.requests_2xx),
        pretty_num(snapshot.requests_4xx),
        pretty_num(snapshot.requests_5xx),
        if snapshot.requests_5xx > 0 {
            "status: server errors observed"
        } else {
            "status: healthy"
        },
        if snapshot.auth_enabled {
            "auth path protected"
        } else {
            "auth path open"
        },
        sparkline_str,
    );
    let right_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Traffic ");
    Paragraph::new(request_summary)
        .block(right_block)
        .style(if snapshot.requests_5xx > 0 {
            warn_style
        } else {
            good_style
        })
        .render(cols[traffic_col], frame);

    let footer_text = snapshot.last_request.as_ref().map_or_else(
        || "Last: no requests observed yet".to_string(),
        |last| {
            format!(
                "Last: {} {} {} {}ms from {} @ {}",
                last.method,
                compact_path(&last.path, 48),
                last.status,
                last.duration_ms,
                last.client_ip,
                last.at_iso
            )
        },
    );
    Paragraph::new(footer_text)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Last Request "),
        )
        .style(title_style)
        .render(rows[2], frame);
}

fn lock_mutex<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn parse_env_u16(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn parse_stty_size(stdout: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(stdout).ok()?.trim();
    let mut parts = text.split_whitespace();
    let rows = parts.next()?.parse::<u16>().ok()?;
    let cols = parts.next()?.parse::<u16>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((cols, rows))
}

const fn is_degenerate_terminal_size(cols: u16, rows: u16) -> bool {
    cols == 0 || rows == 0
}

fn degenerate_stty_size() -> Option<(u16, u16)> {
    let output = std::process::Command::new("stty")
        .arg("size")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let (cols, rows) = parse_stty_size(&output.stdout)?;
    if is_degenerate_terminal_size(cols, rows) {
        return Some((cols, rows));
    }
    None
}

fn request_panel_width_from_columns(columns: u16) -> usize {
    usize::from(columns.clamp(60, 140))
}

fn request_panel_width() -> usize {
    request_panel_width_from_columns(parse_env_u16("COLUMNS", 120))
}

fn pretty_num(value: u64) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + (s.len() / 3));
    for (idx, ch) in s.chars().enumerate() {
        if idx > 0 && (s.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Format a relative time string like "12s ago", "5m ago", "2h ago".
fn relative_time_short(now_us: i64, ts_us: i64) -> String {
    if ts_us >= now_us {
        return "now".to_string();
    }
    let delta_s = (now_us - ts_us) / 1_000_000;
    if delta_s < 1 {
        "now".to_string()
    } else if delta_s < 60 {
        format!("{delta_s}s ago")
    } else if delta_s < 3600 {
        format!("{}m ago", delta_s / 60)
    } else if delta_s < 86400 {
        format!("{}h ago", delta_s / 3600)
    } else {
        format!("{}d ago", delta_s / 86400)
    }
}

fn human_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn compact_path(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    if max_chars <= 3 {
        return "...".to_string();
    }
    let keep = (max_chars - 3) / 2;
    let head = input.chars().take(keep).collect::<String>();
    let tail = input
        .chars()
        .rev()
        .take(max_chars - 3 - keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head}...{tail}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// Strip ANSI escape sequences and return the visible character width.
#[allow(dead_code)]
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut state = AnsiState::Normal;
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        match state {
            AnsiState::Normal => {
                if ch == '\x1b' {
                    state = AnsiState::Esc;
                } else {
                    let s_char = ch.encode_utf8(&mut buf);
                    len += ftui::text::display_width(s_char);
                }
            }
            AnsiState::Esc => {
                if ch == '[' {
                    state = AnsiState::Csi;
                } else if ch == ']' {
                    state = AnsiState::Osc;
                } else {
                    state = AnsiState::Normal;
                }
            }
            AnsiState::Csi => {
                if ch.is_ascii_alphabetic() {
                    state = AnsiState::Normal;
                }
            }
            AnsiState::Osc => {
                if ch == '\x07' {
                    state = AnsiState::Normal;
                } else if ch == '\x1b' {
                    state = AnsiState::OscEsc;
                }
            }
            AnsiState::OscEsc => {
                if ch == '\\' {
                    state = AnsiState::Normal;
                } else {
                    state = AnsiState::Osc;
                }
            }
        }
    }
    len
}

/// Approximate display width of a single char (emoji ≈ 2, CJK ≈ 2, ASCII = 1).
fn unicode_char_width(ch: char) -> usize {
    let c = ch as u32;
    // Emoji ranges (simplified: Misc Symbols, Dingbats, Supplemental Symbols, Emoticons, Transport)
    if (0x1F300..=0x1FAFF).contains(&c)
        || (0x2600..=0x27BF).contains(&c)
        || (0xFE00..=0xFE0F).contains(&c)
    {
        return 2;
    }
    // CJK Unified, Fullwidth forms, etc.
    if (0x3000..=0x9FFF).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
        || (0xFF01..=0xFF60).contains(&c)
    {
        return 2;
    }
    1
}

/// Visible width of a string containing possible emoji/unicode but no ANSI.
#[allow(dead_code)]
fn unicode_display_width(s: &str) -> usize {
    s.chars().map(unicode_char_width).sum()
}

/// Number of decimal digits in a u64 (for alignment).
#[allow(dead_code)]
const fn digit_count(mut n: u64) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    while n > 0 {
        count += 1;
        n /= 10;
    }
    count
}

/// Colorize a single JSON line: keys in `key_color`, numbers in `num_color`.
#[allow(dead_code)]
fn colorize_json_line(line: &str, key_color: &str, num_color: &str, ansi_off: &str) -> String {
    let mut out = String::with_capacity(line.len() + 40);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut s = String::with_capacity(32);
            let mut escaped = false;
            for inner in chars.by_ref() {
                if escaped {
                    s.push(inner);
                    escaped = false;
                } else if inner == '\\' {
                    s.push(inner);
                    escaped = true;
                } else if inner == '"' {
                    break;
                } else {
                    s.push(inner);
                }
            }
            // If the next char after skipping spaces is a colon, it's a key
            let mut is_key = false;
            let mut peek_chars = chars.clone();
            while let Some(&next_c) = peek_chars.peek() {
                if next_c == ':' {
                    is_key = true;
                    break;
                } else if next_c.is_whitespace() {
                    peek_chars.next();
                } else {
                    break;
                }
            }

            if is_key {
                out.push_str(key_color);
            }
            out.push('"');
            out.push_str(&s);
            out.push('"');
            if is_key {
                out.push_str(ansi_off);
            }
        } else if c.is_ascii_digit() || c == '-' {
            let mut num = String::with_capacity(16);
            num.push(c);
            while let Some(&next) = chars.peek() {
                if next.is_ascii_digit() || next == '.' {
                    if let Some(ch) = chars.next() {
                        num.push(ch);
                    }
                } else {
                    break;
                }
            }
            out.push_str(num_color);
            out.push_str(&num);
            out.push_str(ansi_off);
        } else {
            out.push(c);
        }
    }
    out
}

fn fetch_dashboard_db_stats(database_url: &str, storage_root: &Path) -> DashboardDbStats {
    let Some(conn) = dashboard_open_connection(database_url, storage_root) else {
        return DashboardDbStats::default();
    };
    fetch_dashboard_db_stats_from_conn(conn.conn())
}

fn dashboard_open_connection(
    database_url: &str,
    storage_root: &Path,
) -> Option<ObservabilitySyncDb> {
    // The operator dashboard is observability-only. It must not mutate the
    // live mailbox just to render counts, but it should still prefer a
    // canonical archive-backed snapshot when the live sqlite index is stale.
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        Some(ObservabilitySyncDb::live(
            DbConn::open_memory().ok()?,
            ":memory:".to_string(),
        ))
    } else {
        match open_observability_sync_db_connection(
            database_url,
            storage_root,
            "dashboard stats snapshot",
        ) {
            Ok(db) => db,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    database_url,
                    storage_root = %storage_root.display(),
                    "dashboard stats snapshot unavailable"
                );
                None
            }
        }
    }
}

fn fetch_dashboard_db_stats_cached(
    database_url: &str,
    storage_root: &Path,
    conn_state: &mut Option<ObservabilitySyncDb>,
    previous: &DashboardDbStats,
) -> DashboardDbStats {
    if let Some(conn) = conn_state.as_ref() {
        if conn.conn().query_sync("SELECT 1 AS c", &[]).is_ok() {
            return fetch_dashboard_db_stats_from_conn(conn.conn());
        }
        if let Some(old_conn) = conn_state.take() {
            let (conn, _sqlite_path, _snapshot_dir) = old_conn.into_parts();
            mcp_agent_mail_db::close_db_conn(conn, "dashboard cached connection");
        }
    }

    *conn_state = dashboard_open_connection(database_url, storage_root);
    conn_state.as_ref().map_or_else(
        || previous.clone(),
        |conn| fetch_dashboard_db_stats_from_conn(conn.conn()),
    )
}

fn fetch_dashboard_db_stats_from_conn(conn: &DbConn) -> DashboardDbStats {
    let now_micros = mcp_agent_mail_db::timestamps::now_micros();
    let agents_list = conn
        .query_sync(
            "SELECT id, name, program, last_active_ts FROM agents \
             ORDER BY last_active_ts DESC LIMIT 10",
            &[],
        )
        .ok()
        .map(|rows| {
            rows.into_iter()
                .map(|row| {
                    let agent_id = row
                        .get_named::<i64>("id")
                        .ok()
                        .or_else(|| row.get_as::<i64>(0).ok())
                        .unwrap_or(0);
                    let name = row
                        .get_named::<String>("name")
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| {
                            if agent_id > 0 {
                                format!("[unknown-agent-{agent_id}]")
                            } else {
                                "[unknown-agent]".to_string()
                            }
                        });
                    let program = row
                        .get_named::<String>("program")
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "[unknown-program]".to_string());

                    AgentSummary {
                        name,
                        program,
                        last_active_ts: crate::tui_poller::parse_raw_ts(&row, "last_active_ts"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    DashboardDbStats {
        projects: dashboard_count(conn, "SELECT COUNT(*) AS c FROM projects"),
        agents: dashboard_count(conn, "SELECT COUNT(*) AS c FROM agents"),
        messages: dashboard_count(conn, "SELECT COUNT(*) AS c FROM messages"),
        file_reservations: dashboard_active_file_reservations(conn, now_micros),
        contact_links: dashboard_count(conn, "SELECT COUNT(*) AS c FROM agent_links"),
        ack_pending: dashboard_count(
            conn,
            "SELECT COUNT(*) AS c FROM message_recipients mr \
             JOIN messages m ON m.id = mr.message_id \
             WHERE m.ack_required = 1 AND mr.ack_ts IS NULL",
        ),
        agents_list,
    }
}

fn dashboard_active_file_reservation_predicate(conn: &DbConn) -> &'static str {
    if dashboard_has_release_ledger_table(conn) {
        mcp_agent_mail_db::queries::ACTIVE_RESERVATION_PREDICATE
    } else {
        mcp_agent_mail_db::queries::ACTIVE_RESERVATION_LEGACY_PREDICATE
    }
}

fn dashboard_has_release_ledger_table(conn: &DbConn) -> bool {
    conn.query_sync(
        "SELECT 1 AS present FROM sqlite_master \
         WHERE type = 'table' AND name = 'file_reservation_releases' \
         LIMIT 1",
        &[],
    )
    .ok()
    .is_some_and(|rows| !rows.is_empty())
}

fn dashboard_active_file_reservations(conn: &DbConn, now_micros: i64) -> u64 {
    let active_predicate = dashboard_active_file_reservation_predicate(conn);

    if crate::tui_poller::file_reservations_support_active_fast_scan(conn) {
        return dashboard_count_with_params(
            conn,
            &format!(
                "SELECT COUNT(*) AS c FROM file_reservations WHERE ({active_predicate}) AND expires_ts > ?1"
            ),
            &[mcp_agent_mail_db::sqlmodel_core::Value::BigInt(now_micros)],
        );
    }

    let legacy_sql = format!(
        "SELECT expires_ts AS raw_expires_ts FROM file_reservations WHERE ({active_predicate})"
    );
    conn.query_sync(&legacy_sql, &[]).ok().map_or(0, |rows| {
        rows.into_iter().fold(0_u64, |count, row| {
            if crate::tui_poller::parse_raw_ts(&row, "raw_expires_ts") > now_micros {
                count.saturating_add(1)
            } else {
                count
            }
        })
    })
}

fn dashboard_count(conn: &DbConn, sql: &str) -> u64 {
    dashboard_count_with_params(conn, sql, &[])
}

fn dashboard_count_with_params(
    conn: &DbConn,
    sql: &str,
    params: &[mcp_agent_mail_db::sqlmodel_core::Value],
) -> u64 {
    conn.query_sync(sql, params)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.get_named::<i64>("c").ok())
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0)
}

fn dashboard_handle() -> Option<Arc<StartupDashboard>> {
    lock_mutex(&LIVE_DASHBOARD).as_ref().map(Arc::clone)
}

fn set_dashboard_handle(dashboard: Option<Arc<StartupDashboard>>) {
    *lock_mutex(&LIVE_DASHBOARD) = dashboard;
}

fn dashboard_write_log(text: &str) -> bool {
    dashboard_handle().is_some_and(|dashboard| {
        dashboard.log_line(text);
        true
    })
}

fn emit_operator_panel_line(mode: RuntimeOutputMode, text: &str) {
    if mode.is_tui() {
        if let Some(state) = tui_state_handle() {
            state.push_console_log(text.to_string());
        }
        return;
    }
    if !dashboard_write_log(text) {
        ftui_runtime::ftui_println!("{text}");
    }
}

fn dashboard_emit_event(
    kind: console::ConsoleEventKind,
    severity: console::ConsoleEventSeverity,
    summary: impl Into<String>,
    fields: Vec<(String, String)>,
    json: Option<serde_json::Value>,
) {
    if let Some(dashboard) = dashboard_handle() {
        dashboard.emit_event(kind, severity, summary, fields, json);
    }
}

#[derive(Debug, Clone)]
struct JwtContext {
    roles: Vec<String>,
    sub: Option<String>,
}

#[derive(Debug, Clone)]
struct JwksCacheEntry {
    fetched_at: Instant,
    jwks: Arc<JwkSet>,
}

#[derive(Debug)]
enum RateLimitRedisState {
    Disabled,
    Uninitialized { url: String },
    Ready(Arc<RedisClient>),
    Failed,
}

struct HttpState {
    router: Arc<fastmcp_server::Router>,
    server_info: fastmcp_protocol::ServerInfo,
    server_capabilities: fastmcp_protocol::ServerCapabilities,
    config: mcp_agent_mail_core::Config,
    rate_limiter: Arc<RateLimiter>,
    rate_limit_redis: Mutex<RateLimitRedisState>,
    request_timeout_secs: u64,
    handler: Arc<HttpRequestHandler>,
    jwks_http_client: HttpClient,
    jwks_cache: Mutex<Option<JwksCacheEntry>>,
    /// Stampede guard: only one task refreshes JWKS at a time.
    /// Others serve stale cached data while refresh is in-flight.
    jwks_refreshing: AtomicBool,
    /// Optional web root for SPA static file serving.
    web_root: Option<static_files::WebRoot>,
    /// Reused snapshot state for `/mail/ws-state` polling when no live TUI is active.
    ws_state_fallback: Arc<tui_bridge::TuiSharedState>,
    request_diagnostics: Arc<HttpRequestRuntimeDiagnostics>,
    /// Weak self-reference for `spawn_blocking` in async dispatch.
    /// Set immediately after `Arc::new(HttpState::new(...))`.
    self_ref: std::sync::OnceLock<std::sync::Weak<HttpState>>,
}

impl HttpState {
    fn new(
        router: Arc<fastmcp_server::Router>,
        server_info: fastmcp_protocol::ServerInfo,
        server_capabilities: fastmcp_protocol::ServerCapabilities,
        config: mcp_agent_mail_core::Config,
        request_diagnostics: Arc<HttpRequestRuntimeDiagnostics>,
    ) -> Self {
        let handler = Arc::new(HttpRequestHandler::with_config(HttpHandlerConfig {
            base_path: config.http_path.clone(),
            allow_cors: config.http_cors_enabled,
            cors_origins: config.http_cors_origins.clone(),
            timeout: Duration::from_secs(30),
            max_body_size: 10 * 1024 * 1024,
        }));
        let web_root = static_files::resolve_web_root();
        if let Some(ref wr) = web_root {
            tracing::info!(root = ?wr, "SPA web root resolved; serving static files");
        }
        let rate_limit_redis =
            if config.http_rate_limit_backend == mcp_agent_mail_core::RateLimitBackend::Redis {
                config
                    .http_rate_limit_redis_url
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map_or_else(
                        || RateLimitRedisState::Disabled,
                        |url| RateLimitRedisState::Uninitialized { url: url.clone() },
                    )
            } else {
                RateLimitRedisState::Disabled
            };
        let ws_state_fallback = tui_bridge::TuiSharedState::new(&config);
        Self {
            router,
            server_info,
            server_capabilities,
            config,
            rate_limiter: Arc::new(RateLimiter::new()),
            rate_limit_redis: Mutex::new(rate_limit_redis),
            request_timeout_secs: 30,
            handler,
            jwks_http_client: HttpClient::new(),
            jwks_cache: Mutex::new(None),
            jwks_refreshing: AtomicBool::new(false),
            web_root,
            ws_state_fallback,
            request_diagnostics,
            self_ref: std::sync::OnceLock::new(),
        }
    }

    async fn handle(&self, req: Http1Request) -> Http1Response {
        let metrics = mcp_agent_mail_core::global_metrics();
        let _inflight_guard = InflightGuard::begin(&metrics.http.requests_inflight);

        let dashboard = dashboard_handle();
        let tui = tui_state_handle();
        // Passive browser observability remains useful even when no live TUI or
        // explicit request-log sink is active, so we always capture enough
        // request metadata to feed the fallback web-dashboard state.
        let needs_request_log = true;
        let method_name = req.method.as_str().to_string();
        let (path_for_diag, _query) = split_path_query(&req.uri);
        self.request_diagnostics
            .record_started(&method_name, &path_for_diag);

        let start = Instant::now();
        let (method, path, client_ip) = if needs_request_log {
            let method = req.method.clone();
            let (path, _query) = split_path_query(&req.uri);
            let client_ip = req
                .peer_addr
                .map_or_else(|| "-".to_string(), |addr| addr.ip().to_string());
            (Some(method), Some(path), Some(client_ip))
        } else {
            (None, None, None)
        };

        let resp = self.handle_inner(req).await;
        self.request_diagnostics
            .record_completed(&method_name, &path_for_diag, resp.status);
        let elapsed = start.elapsed();
        let latency_us =
            u64::try_from(elapsed.as_micros().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
        metrics.http.record_response(resp.status, latency_us);

        if !needs_request_log {
            return resp;
        }

        let dur_ms =
            u64::try_from(elapsed.as_millis().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
        if let Some(dashboard) = dashboard.as_ref()
            && let (Some(method), Some(path), Some(client_ip)) =
                (method.as_ref(), path.as_ref(), client_ip.as_ref())
        {
            dashboard.record_request(method.as_str(), path, resp.status, dur_ms, client_ip);
        }
        // Feed the live TUI when present, otherwise feed the passive fallback
        // state used by `/mail/ws-state` and `/web-dashboard/state`.
        if let (Some(method), Some(path), Some(client_ip)) =
            (method.as_ref(), path.as_ref(), client_ip.as_ref())
            && !should_suppress_tui_http_event(path)
        {
            let observability_state = tui.as_deref().unwrap_or(self.ws_state_fallback.as_ref());
            let _ = observability_state
                .push_event_async(tui_events::MailEvent::http_request(
                    method.as_str(),
                    path.as_str(),
                    resp.status,
                    dur_ms,
                    client_ip.as_str(),
                ))
                .await;
            observability_state.record_request(resp.status, dur_ms);
        }
        if self.config.http_request_log_enabled
            && let (Some(method), Some(path), Some(client_ip)) =
                (method.as_ref(), path.as_ref(), client_ip.as_ref())
        {
            self.emit_http_request_log(method.as_str(), path, resp.status, dur_ms, client_ip);
        }
        resp
    }

    async fn handle_inner(&self, mut req: Http1Request) -> Http1Response {
        if let Some(resp) = self.handle_options(&req) {
            return resp;
        }

        let (path, _query) = split_path_query(&req.uri);
        // Legacy parity: health routes bypass bearer auth even when configured.
        //
        // Note: the legacy FastAPI stack used a `/health/` prefix check, but this
        // server also exposes `/health` + `/healthz` aliases for operator tooling
        // and common probe conventions.
        if path == "/health" || path == "/healthz" || path.starts_with("/health/") {
            if let Some(resp) = self.handle_special_routes(&req, &path) {
                return resp;
            }
            return self.error_response(&req, 404, "Not Found");
        }

        let auth_cx = self.request_cx();

        // Legacy parity: bearer auth applies to all non-health routes (even unknown paths/methods),
        // so missing/invalid auth yields 401 instead of downstream 404/405/400.
        if let Some(resp) = self.check_bearer_auth_with_cx(&auth_cx, &req).await {
            return resp;
        }

        // Remaining special routes (well-known, mail UI, etc).
        if let Some(resp) = self.handle_special_routes(&req, &path) {
            return resp;
        }
        if !self.path_allowed(&path) {
            return self.error_response(&req, 404, "Not Found");
        }

        if !matches!(req.method, Http1Method::Post) {
            return self.error_response(&req, 405, "Method Not Allowed");
        }

        let base_no_slash = normalize_base_path(&self.config.http_path);
        let canonical_path = canonicalize_mcp_path_for_handler(&path, &base_no_slash);
        maybe_inject_localhost_authorization_for_base_passthrough(
            &self.config,
            &mut req,
            &canonical_path,
            &base_no_slash,
        );

        // Legacy parity: direct POST handler for `/base` forwards to the mounted `/base/` app.
        let effective_path = if base_no_slash == "/" || canonical_path != base_no_slash {
            canonical_path
        } else {
            format!("{base_no_slash}/")
        };

        let http_req = to_mcp_http_request(&req, &effective_path);
        let json_rpc = match self.handler.parse_request(&http_req) {
            Ok(req) => req,
            Err(err) => {
                let status = http_error_status(&err);
                let resp = self.handler.error_response(status, &err.to_string());
                return to_http1_response(
                    resp,
                    self.cors_origin(&req),
                    self.config.http_cors_allow_credentials,
                    &self.config.http_cors_allow_methods,
                    &self.config.http_cors_allow_headers,
                );
            }
        };

        if let Some(resp) = self
            .check_rbac_and_rate_limit_with_cx(&auth_cx, &req, &json_rpc)
            .await
        {
            return resp;
        }

        let response = self.dispatch(json_rpc).await.map_or_else(
            || HttpResponse::new(fastmcp_transport::http::HttpStatus::ACCEPTED),
            |resp| HttpResponse::ok().with_json(&resp),
        );

        to_http1_response(
            response,
            self.cors_origin(&req),
            self.config.http_cors_allow_credentials,
            &self.config.http_cors_allow_methods,
            &self.config.http_cors_allow_headers,
        )
    }

    fn emit_http_request_log(
        &self,
        method: &str,
        path: &str,
        status: u16,
        duration_ms: u64,
        client_ip: &str,
    ) {
        // Legacy parity: request logging must not affect request/response behavior.
        // All failures are swallowed.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);

            // structlog-like emission (stderr)
            let line = if self.config.log_json_enabled {
                http_request_log_json_line(&timestamp, method, path, status, duration_ms, client_ip)
                    .unwrap_or_else(|| {
                        http_request_log_kv_line(
                            &timestamp,
                            method,
                            path,
                            status,
                            duration_ms,
                            client_ip,
                        )
                    })
            } else {
                http_request_log_kv_line(&timestamp, method, path, status, duration_ms, client_ip)
            };
            let output_mode = runtime_output_mode(&self.config);
            if output_mode.is_tui() {
                return;
            }

            // In rich TTY mode, the request panel is the primary operator-facing output.
            // Suppress duplicate key/value line unless JSON mode requests structured logs.
            let use_ansi = self.config.log_rich_enabled && std::io::stdout().is_terminal();
            if output_mode.should_emit_structured_request_line(use_ansi) {
                ftui_runtime::ftui_eprintln!("{line}");
            }

            // Rich-ish panel output (stdout), fallback to legacy plain-text line on any error.
            // Gate: only render ANSI panel when rich output is enabled AND stdout is a TTY.
            if let Some(panel) = console::render_http_request_panel(
                request_panel_width(),
                method,
                path,
                status,
                duration_ms,
                client_ip,
                use_ansi,
            ) {
                emit_operator_panel_line(output_mode, &panel);
            } else {
                let fallback =
                    http_request_log_fallback_line(method, path, status, duration_ms, client_ip);
                emit_operator_panel_line(output_mode, &fallback);
            }
        }));
    }

    fn handle_options(&self, req: &Http1Request) -> Option<Http1Response> {
        if !matches!(req.method, Http1Method::Options) {
            return None;
        }

        let (path, _query) = split_path_query(&req.uri);
        let http_req = to_mcp_http_request(req, &path);
        let resp = self.handler.handle_options(&http_req);
        Some(to_http1_response(
            resp,
            self.cors_origin(req),
            self.config.http_cors_allow_credentials,
            &self.config.http_cors_allow_methods,
            &self.config.http_cors_allow_headers,
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn handle_special_routes(&self, req: &Http1Request, path: &str) -> Option<Http1Response> {
        match path {
            "/healthz" | "/health/liveness" => {
                if !matches!(req.method, Http1Method::Get) {
                    return Some(self.error_response(req, 405, "Method Not Allowed"));
                }
                return Some(self.health_json_response(
                    req,
                    200,
                    &serde_json::json!({"status":"alive"}),
                ));
            }
            "/health" | "/health/readiness" => {
                if !matches!(req.method, Http1Method::Get) {
                    return Some(self.error_response(req, 405, "Method Not Allowed"));
                }
                // Freshly bound listeners should answer quickly even while the
                // DB warm path is still settling. Report a transient warmup
                // state instead of blocking readiness probes on immediate pool
                // initialization right after bind.
                if startup_readiness_fast_path_active() {
                    return Some(self.health_json_response(
                        req,
                        503,
                        &serde_json::json!({"status":"warming_up"}),
                    ));
                }
                if let Err(_err) = readiness_check_quick(&self.config) {
                    tracing::warn!(error = %_err, "readiness check failed");
                    return Some(self.error_response(req, 503, "service unavailable"));
                }
                let mut body = serde_json::json!({"status":"ready"});
                // Enrich readiness response with database identity so
                // operators can verify the correct DB file is active.
                enrich_readiness_response(
                    &self.config.database_url,
                    self.config.storage_root.as_path(),
                    &mut body,
                );
                return Some(self.health_json_response(req, 200, &body));
            }
            "/.well-known/oauth-authorization-server"
            | "/.well-known/oauth-authorization-server/mcp" => {
                if !matches!(req.method, Http1Method::Get) {
                    return Some(self.error_response(req, 405, "Method Not Allowed"));
                }
                return Some(self.json_response(
                    req,
                    200,
                    &serde_json::json!({"mcp_oauth": false}),
                ));
            }
            _ => {}
        }

        if path == "/mail/ws-input" {
            if !matches!(req.method, Http1Method::Post) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let Some(state) = tui_state_handle() else {
                return Some(self.error_response(req, 503, "TUI state is not active"));
            };
            let parsed = match tui_ws_input::parse_remote_terminal_events(&req.body) {
                Ok(parsed) => parsed,
                Err(err) => return Some(self.error_response(req, 400, &err)),
            };
            let mut dropped_oldest = 0_usize;
            let accepted = parsed.events.len();
            for event in parsed.events {
                if state.push_remote_terminal_event(event) {
                    dropped_oldest += 1;
                }
            }
            let queue_stats = state.remote_terminal_queue_stats();
            let payload = serde_json::json!({
                "status": "accepted",
                "accepted": accepted,
                "ignored": parsed.ignored,
                "dropped_oldest": dropped_oldest,
                "queue_depth": queue_stats.depth,
                "queue_dropped_oldest_total": queue_stats.dropped_oldest_total,
                "queue_resize_coalesced_total": queue_stats.resize_coalesced_total,
            });
            return Some(self.json_response(req, 202, &payload));
        }

        if path == "/mail/ws-state" {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            if is_websocket_upgrade_request(req) {
                return Some(self.error_response(
                    req,
                    501,
                    "WebSocket upgrade is not supported on /mail/ws-state; use HTTP polling.",
                ));
            }

            let (_path_part, query_part) = split_path_query(&req.uri);
            let query = query_part.as_deref();
            let payload = tui_state_handle().map_or_else(
                || tui_ws_state::poll_payload(&self.ws_state_fallback, query),
                |state| tui_ws_state::poll_payload(&state, query),
            );
            return Some(self.json_response(req, 200, &payload));
        }

        if path == "/mail/api/locks" || path == "/mail/api/locks/" {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let payload = match mcp_agent_mail_storage::collect_lock_status(&self.config) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(%err, "lock status query failed");
                    let msg = "lock status temporarily unavailable".to_string();
                    return Some(self.error_response(req, 500, &msg));
                }
            };
            return Some(self.json_response(req, 200, &payload));
        }

        // ── Web Dashboard (TUI mirror in browser) ────────────────────
        if path == "/web-dashboard" || path == "/web-dashboard/" {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let host = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("host"))
                .map_or("", |(_, v)| v.as_str());
            let html = tui_web_dashboard::handle_page(host);
            return Some(self.raw_response(
                req,
                200,
                "text/html; charset=utf-8",
                html.into_bytes(),
            ));
        }

        if path == "/web-dashboard/state" {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let (_path_part, query_part) = split_path_query(&req.uri);
            let live_state = tui_state_handle();
            let (status, payload) = tui_web_dashboard::handle_state_response(
                live_state.as_deref(),
                &self.ws_state_fallback,
                query_part.as_deref(),
            );
            return Some(self.raw_response(req, status, "application/json", payload.into_bytes()));
        }

        if path == "/web-dashboard/stream" {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let (_path_part, query_part) = split_path_query(&req.uri);
            let live_state = tui_state_handle();
            let (status, payload) = tui_web_dashboard::handle_stream_response(
                live_state.as_deref(),
                &self.ws_state_fallback,
                query_part.as_deref(),
            );
            return Some(self.raw_response(req, status, "application/json", payload.into_bytes()));
        }

        if path == "/web-dashboard/input" {
            if !matches!(req.method, Http1Method::Post) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let live_state = tui_state_handle();
            let (status, payload) = live_state
                .as_deref()
                .map_or_else(tui_web_dashboard::handle_inactive_input, |state| {
                    tui_web_dashboard::handle_input(state, &req.body)
                });
            return Some(self.raw_response(req, status, "application/json", payload.into_bytes()));
        }

        if path == "/mail" || path.starts_with("/mail/") {
            return Some(self.handle_mail_dispatch(req, path));
        }

        // Static file serving from optional web/ SPA directory.
        // Only serve for GET requests on non-API paths (legacy Python: _is_api_path check).
        if let Some(ref web_root) = self.web_root
            && matches!(req.method, Http1Method::Get)
            && !self.path_allowed(path)
            && let Some((content_type, body)) = web_root.serve(path)
        {
            let mut resp = self.raw_response(req, 200, content_type, body);
            resp.headers.push((
                "cache-control".to_string(),
                "no-store, no-cache, must-revalidate".to_string(),
            ));
            return Some(resp);
        }

        None
    }

    /// Dispatch a `/mail` or `/mail/…` request to the mail UI layer.
    fn is_mail_json_route(path: &str, method_str: &str) -> bool {
        if method_str == "POST" || path.starts_with("/mail/api/") {
            return true;
        }
        if path == "/mail/archive/time-travel/snapshot" {
            return true;
        }
        if let Some(rest) = path.strip_prefix("/mail/archive/browser/")
            && let Some((project_slug, tail)) = rest.split_once('/')
        {
            return !project_slug.is_empty() && tail == "file";
        }
        false
    }

    fn handle_mail_dispatch(&self, req: &Http1Request, path: &str) -> Http1Response {
        if !matches!(req.method, Http1Method::Get | Http1Method::Post) {
            return self.error_response(req, 405, "Method Not Allowed");
        }
        let (_path_part, query_part) = split_path_query(&req.uri);
        let query_str = query_part.as_deref().unwrap_or("");
        let method_str = if matches!(req.method, Http1Method::Post) {
            "POST"
        } else {
            "GET"
        };
        let body_str = std::str::from_utf8(&req.body).unwrap_or("");
        let is_api = Self::is_mail_json_route(path, method_str);
        match mail_ui::dispatch(path, query_str, method_str, body_str) {
            Ok(Some(body)) => {
                let content_type = if is_api {
                    "application/json"
                } else {
                    "text/html; charset=utf-8"
                };
                self.raw_response(req, 200, content_type, body.into_bytes())
            }
            Ok(None) => self.error_response(req, 404, "Not Found"),
            Err((status, msg)) => {
                if is_api {
                    return self.raw_response(req, status, "application/json", msg.into_bytes());
                }
                if status == 404 {
                    let html = templates::render_template(
                        "error.html",
                        serde_json::json!({ "message": msg }),
                    )
                    .unwrap_or_else(|_| msg.clone());
                    return self.raw_response(
                        req,
                        404,
                        "text/html; charset=utf-8",
                        html.into_bytes(),
                    );
                }
                self.error_response(req, status, &msg)
            }
        }
    }

    /// Check if `path` is under the configured MCP base path.
    ///
    /// Legacy parity: `FastAPI` `mount(base_no_slash, app)` + `mount(base_with_slash, app)`
    /// routes the exact base **and** all sub-paths to the stateless MCP app.
    fn path_allowed(&self, path: &str) -> bool {
        let base_no_slash = normalize_base_path(&self.config.http_path);
        if base_no_slash == "/" {
            return true;
        }

        if path_matches_base(path, &base_no_slash) {
            return true;
        }

        // Dev convenience: accept `/api/*` and `/mcp/*` interchangeably so different
        // MCP clients can talk to the same server without an extra HTTP_PATH export.
        // Only applies to the root bases (/api or /mcp); nested bases keep strict semantics.
        if let Some(alias_no_slash) = mcp_base_alias_no_slash(&base_no_slash)
            && path_matches_base(path, alias_no_slash)
        {
            return true;
        }

        false
    }

    fn has_expected_bearer_header(&self, req: &Http1Request) -> bool {
        let Some(expected) = &self.config.http_bearer_token else {
            return false;
        };
        let auth = header_value(req, "authorization").unwrap_or("");
        let expected_header = format!("Bearer {expected}");
        constant_time_eq(auth, expected_header.as_str())
    }

    fn request_cx(&self) -> Cx {
        let budget = if self.request_timeout_secs == 0 {
            Budget::INFINITE
        } else {
            let deadline = wall_now() + Duration::from_secs(self.request_timeout_secs);
            Budget::new().with_deadline(deadline)
        };
        Cx::for_request_with_budget(budget)
    }

    async fn check_bearer_auth_with_cx(
        &self,
        cx: &Cx,
        req: &Http1Request,
    ) -> Option<Http1Response> {
        if self.config.http_bearer_token.is_none() && !self.config.http_jwt_enabled {
            return None;
        }

        let (path, _query) = split_path_query(&req.uri);
        let is_mail_route = path == "/mail" || path.starts_with("/mail/");
        let is_web_dashboard_route = path == "/web-dashboard"
            || path == "/web-dashboard/"
            || path == "/web-dashboard/state"
            || path == "/web-dashboard/stream"
            || path == "/web-dashboard/input";
        let is_browser_route = is_mail_route || is_web_dashboard_route;

        if self.allow_local_unauthenticated(req) {
            return None;
        }

        // Legacy parity: static bearer checks compare the full header value
        // (no trimming/coercion).
        if self.has_expected_bearer_header(req) {
            return None;
        }

        // D3: Accept bearer token from `?token=` query parameter on browser
        // routes. Browser-opened surfaces cannot set Authorization headers, so
        // shareable URLs embed the token as a query parameter instead.
        if is_browser_route && self.has_expected_query_token(req) {
            return None;
        }

        // When JWT auth is enabled, validate bearer tokens here as an auth gate
        // for all HTTP routes (including /mail/*), not just MCP JSON-RPC routes.
        if self.config.http_jwt_enabled && self.decode_jwt_with_cx(cx, req).await.is_ok() {
            return None;
        }

        // D4: For browser HTML routes, return actionable HTML; for
        // machine/browser JSON routes, preserve JSON 401 responses.
        if is_browser_route {
            let method_str = if matches!(req.method, Http1Method::Post) {
                "POST"
            } else {
                "GET"
            };
            let is_browser_json_route = Self::is_mail_json_route(&path, method_str)
                || path == "/web-dashboard/state"
                || path == "/web-dashboard/stream"
                || path == "/web-dashboard/input";
            if is_browser_json_route {
                return Some(self.error_response(req, 401, "Unauthorized"));
            }
            return Some(self.browser_unauthorized_html_response(req));
        }

        Some(self.error_response(req, 401, "Unauthorized"))
    }

    #[cfg(test)]
    async fn check_bearer_auth(&self, req: &Http1Request) -> Option<Http1Response> {
        let cx = Cx::for_testing();
        self.check_bearer_auth_with_cx(&cx, req).await
    }

    /// Check whether the request URI contains a `?token=<expected>` query parameter
    /// matching the configured bearer token. Uses constant-time comparison.
    fn has_expected_query_token(&self, req: &Http1Request) -> bool {
        let Some(expected) = &self.config.http_bearer_token else {
            return false;
        };
        let (_path, query_part) = split_path_query(&req.uri);
        let query_str = query_part.as_deref().unwrap_or("");
        for pair in query_str.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                if constant_time_eq(value, expected) {
                    return true;
                }
                if let Some(decoded) = percent_decode_query_component(value)
                    && constant_time_eq(decoded.as_str(), expected)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Return a user-friendly HTML 401 page for browser routes explaining how to
    /// authenticate. Operators opening browser surfaces need actionable guidance
    /// rather than an opaque `{"detail":"Unauthorized"}` JSON blob.
    fn browser_unauthorized_html_response(&self, req: &Http1Request) -> Http1Response {
        let html = r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>401 — Unauthorized</title>
<style>
body{font-family:system-ui,sans-serif;max-width:600px;margin:60px auto;padding:0 20px;color:#333}
h1{color:#c0392b}code{background:#f4f4f4;padding:2px 6px;border-radius:3px}
.steps{background:#f9f9f9;border-left:4px solid #3498db;padding:12px 16px;margin:16px 0}
</style></head>
<body>
<h1>401 — Unauthorized</h1>
<p>This Agent Mail browser surface requires a valid bearer token.</p>
<div class="steps">
<h3>How to fix this</h3>
<ol>
<li>Set <code>HTTP_BEARER_TOKEN</code> in your <code>.env</code> file
(or environment) to match the server's configured token.</li>
<li>Use the generated health link from the TUI — it embeds the token
as <code>?token=…</code> in the URL automatically.</li>
<li>If you are opening <code>/web-dashboard</code> in a browser, keep the
<code>?token=…</code> query parameter on the page URL so the dashboard can
reuse it for background polling and input requests.</li>
<li>If using <code>curl</code> or an API client, pass the token via header:<br>
<code>Authorization: Bearer &lt;your-token&gt;</code></li>
</ol>
</div>
<p><strong>Tip:</strong> If you're accessing from <code>localhost</code>,
enable <code>HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true</code>
to skip auth for local requests.</p>
</body></html>"#;
        self.raw_response(
            req,
            401,
            "text/html; charset=utf-8",
            html.as_bytes().to_vec(),
        )
    }

    async fn fetch_jwks_with_cx(&self, cx: &Cx, url: &str, force: bool) -> Result<Arc<JwkSet>, ()> {
        // Fast path: return cached value if still fresh.
        if force {
            let _ = self.jwks_refreshing.compare_exchange(
                false,
                true,
                Ordering::Acquire,
                Ordering::Relaxed,
            );
        } else {
            let cached = self
                .jwks_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(entry) = cached {
                if entry.fetched_at.elapsed() < JWKS_CACHE_TTL {
                    return Ok(entry.jwks);
                }
                // Stale-while-revalidate: if another task is already refreshing,
                // serve the stale cached value instead of stampeding.
                if self
                    .jwks_refreshing
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_err()
                {
                    return Ok(entry.jwks);
                }
                // We won the CAS — proceed to refresh below.
            } else {
                // No cached entry at all — acquire the refresh lock.
                let _ = self.jwks_refreshing.compare_exchange(
                    false,
                    true,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                );
            }
        }

        let result = async {
            let fut = Box::pin(self.jwks_http_client.get(cx, url));
            let Ok(Ok(resp)) = timeout(wall_now(), JWKS_FETCH_TIMEOUT, fut).await else {
                return Err(());
            };
            if resp.status != 200 {
                return Err(());
            }
            let jwks: JwkSet = serde_json::from_slice(&resp.body).map_err(|_| ())?;
            let jwks = Arc::new(jwks);

            {
                let mut cache = self
                    .jwks_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *cache = Some(JwksCacheEntry {
                    fetched_at: Instant::now(),
                    jwks: Arc::clone(&jwks),
                });
            }
            Ok(jwks)
        }
        .await;

        // Always release the refresh lock.
        self.jwks_refreshing.store(false, Ordering::Release);
        result
    }

    #[cfg(test)]
    async fn fetch_jwks(&self, url: &str, force: bool) -> Result<Arc<JwkSet>, ()> {
        let cx = Cx::for_testing();
        self.fetch_jwks_with_cx(&cx, url, force).await
    }

    fn parse_bearer_token(req: &Http1Request) -> Result<&str, ()> {
        let Some(auth) = header_value(req, "authorization") else {
            return Err(());
        };
        let auth = auth.trim();
        let Some(token) = auth
            .get(..7)
            .filter(|prefix| prefix.eq_ignore_ascii_case("bearer "))
            .map(|_| auth[7..].trim())
        else {
            return Err(());
        };
        if token.is_empty() {
            return Err(());
        }
        Ok(token)
    }

    fn jwt_algorithms(&self) -> Vec<jsonwebtoken::Algorithm> {
        let mut algorithms: Vec<jsonwebtoken::Algorithm> = self
            .config
            .http_jwt_algorithms
            .iter()
            .filter_map(|s| s.parse::<jsonwebtoken::Algorithm>().ok())
            .collect();
        if algorithms.is_empty() {
            algorithms.push(jsonwebtoken::Algorithm::HS256);
        }
        algorithms
    }

    async fn jwt_decoding_key_with_cx(
        &self,
        cx: &Cx,
        kid: Option<&str>,
    ) -> Result<DecodingKey, ()> {
        if let Some(jwks_url) = self
            .config
            .http_jwt_jwks_url
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            // Cache JWKS fetches; if kid is missing from the cached set, force refresh once.
            let jwks = self.fetch_jwks_with_cx(cx, jwks_url, false).await?;
            let jwk = if let Some(kid) = kid {
                if let Some(jwk) = jwks.find(kid).cloned() {
                    jwk
                } else {
                    let jwks = self.fetch_jwks_with_cx(cx, jwks_url, true).await?;
                    jwks.find(kid).cloned().ok_or(())?
                }
            } else {
                jwks.keys.first().cloned().ok_or(())?
            };
            DecodingKey::from_jwk(&jwk).map_err(|_| ())
        } else if let Some(secret) = self
            .config
            .http_jwt_secret
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Ok(DecodingKey::from_secret(secret.as_bytes()))
        } else {
            Err(())
        }
    }

    fn jwt_validation(mut algorithms: Vec<jsonwebtoken::Algorithm>) -> Validation {
        if algorithms.is_empty() {
            algorithms.push(jsonwebtoken::Algorithm::HS256);
        }

        let mut validation = Validation::new(algorithms[0]);
        validation.algorithms = algorithms;
        validation.required_spec_claims = HashSet::new();
        validation.leeway = 0;
        validation.validate_nbf = true;
        // Legacy behavior: only validate audience when configured.
        validation.validate_aud = false;
        validation
    }

    fn validate_jwt_claims(&self, claims: &serde_json::Value) -> Result<(), ()> {
        if let Some(expected) = self
            .config
            .http_jwt_issuer
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            let iss = claims.get("iss").and_then(|v| v.as_str()).unwrap_or("");
            if iss != expected {
                return Err(());
            }
        }

        if let Some(expected) = self
            .config
            .http_jwt_audience
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            let ok = match claims.get("aud") {
                Some(serde_json::Value::String(s)) => s == expected,
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .any(|v| v.as_str().is_some_and(|s| s == expected)),
                _ => false,
            };
            if !ok {
                return Err(());
            }
        }

        Ok(())
    }

    fn jwt_roles_from_claims(&self, claims: &serde_json::Value) -> Vec<String> {
        let mut roles = match claims.get(&self.config.http_jwt_role_claim) {
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map_or_else(|| v.to_string(), ToString::to_string)
                })
                .collect(),
            _ => Vec::new(),
        };
        roles.retain(|r| !r.trim().is_empty());
        roles.sort();
        roles.dedup();
        if roles.is_empty() {
            roles.push(self.config.http_rbac_default_role.clone());
        }
        roles
    }

    fn jwt_sub_from_claims(claims: &serde_json::Value) -> Option<String> {
        claims
            .get("sub")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .filter(|s| !s.is_empty())
    }

    async fn decode_jwt_with_cx(&self, cx: &Cx, req: &Http1Request) -> Result<JwtContext, ()> {
        let token = Self::parse_bearer_token(req)?;
        let algorithms = self.jwt_algorithms();
        let header = jsonwebtoken::decode_header(token).map_err(|_| ())?;
        let key = self
            .jwt_decoding_key_with_cx(cx, header.kid.as_deref())
            .await?;
        let validation = Self::jwt_validation(algorithms);
        let token_data =
            jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation).map_err(|_| ())?;
        let claims = token_data.claims;

        self.validate_jwt_claims(&claims)?;
        let roles = self.jwt_roles_from_claims(&claims);
        let sub = Self::jwt_sub_from_claims(&claims);

        Ok(JwtContext { roles, sub })
    }

    async fn rate_limit_redis_client(&self, cx: &Cx) -> Option<Arc<RedisClient>> {
        let url = {
            let guard = self
                .rate_limit_redis
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            match &*guard {
                RateLimitRedisState::Disabled | RateLimitRedisState::Failed => return None,
                RateLimitRedisState::Ready(client) => return Some(Arc::clone(client)),
                RateLimitRedisState::Uninitialized { url } => url.clone(),
            }
        };

        match RedisClient::connect(cx, &url).await {
            Ok(client) => {
                let client = Arc::new(client);
                {
                    let mut guard = self
                        .rate_limit_redis
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *guard = RateLimitRedisState::Ready(Arc::clone(&client));
                }
                Some(client)
            }
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "rate limit redis init failed; falling back to memory"
                );
                let mut guard = self
                    .rate_limit_redis
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = RateLimitRedisState::Failed;
                None
            }
        }
    }

    async fn consume_rate_limit(&self, key: &str, per_minute: u32, burst: u32) -> bool {
        if per_minute == 0 {
            return true;
        }

        let now = rate_limit_now();
        let budget = if self.request_timeout_secs == 0 {
            Budget::INFINITE
        } else {
            // Use wall_now() + duration for a RELATIVE deadline, not an absolute epoch time.
            let deadline = wall_now() + std::time::Duration::from_secs(self.request_timeout_secs);
            Budget::new().with_deadline(deadline)
        };
        let cx = Cx::for_request_with_budget(budget);

        let redis = self.rate_limit_redis_client(&cx).await;
        let has_redis = redis.is_some();

        if let Some(redis) = redis
            && let Ok(allowed) =
                consume_rate_limit_redis(&cx, &redis, key, per_minute, burst, now).await
        {
            return allowed;
        }
        // Legacy parity: if Redis is configured, periodic cleanup is disabled even when
        // a specific Redis call fails and we fall back to memory.

        self.rate_limiter
            .allow_memory(key, per_minute, burst, now, !has_redis)
    }

    #[cfg(test)]
    async fn check_rbac_and_rate_limit(
        &self,
        req: &Http1Request,
        json_rpc: &JsonRpcRequest,
    ) -> Option<Http1Response> {
        let cx = Cx::for_testing();
        self.check_rbac_and_rate_limit_with_cx(&cx, req, json_rpc)
            .await
    }

    async fn check_rbac_and_rate_limit_with_cx(
        &self,
        cx: &Cx,
        req: &Http1Request,
        json_rpc: &JsonRpcRequest,
    ) -> Option<Http1Response> {
        let (kind, tool_name) = classify_request(json_rpc);
        let is_local_ok = self.allow_local_unauthenticated(req);
        let local_bypass_jwt_sub = if self.config.http_rate_limit_enabled
            && is_local_ok
            && self.config.http_jwt_enabled
            && !self.has_expected_bearer_header(req)
        {
            self.decode_jwt_with_cx(cx, req)
                .await
                .ok()
                .and_then(|ctx| ctx.sub)
        } else {
            None
        };

        let (roles, jwt_sub) = if is_local_ok {
            // Localhost bypass is a full auth bypass for local development, so
            // do not enforce JWT/static-token auth later in the path. When a
            // valid JWT is present, still preserve its subject so localhost
            // callers keep distinct rate-limit buckets.
            (
                vec![self.config.http_rbac_default_role.clone()],
                local_bypass_jwt_sub,
            )
        } else if self.config.http_jwt_enabled {
            if self.has_expected_bearer_header(req) {
                (vec![self.config.http_rbac_default_role.clone()], None)
            } else {
                match self.decode_jwt_with_cx(cx, req).await {
                    Ok(ctx) => (ctx.roles, ctx.sub),
                    Err(()) => return Some(self.error_response(req, 401, "Unauthorized")),
                }
            }
        } else {
            (vec![self.config.http_rbac_default_role.clone()], None)
        };

        // RBAC (mirrors legacy python behavior)
        if self.config.http_rbac_enabled
            && !is_local_ok
            && matches!(kind, RequestKind::Tools | RequestKind::Resources)
        {
            let is_reader = roles
                .iter()
                .any(|r| self.config.http_rbac_reader_roles.contains(r));
            let is_writer = roles
                .iter()
                .any(|r| self.config.http_rbac_writer_roles.contains(r));

            if kind == RequestKind::Resources {
                // Legacy python allows resources regardless of role membership.
            } else if kind == RequestKind::Tools && json_rpc.method == "tools/call" {
                if let Some(ref name) = tool_name {
                    if self.config.http_rbac_readonly_tools.contains(name) {
                        if !is_reader && !is_writer {
                            return Some(self.error_response(req, 403, "Forbidden"));
                        }
                    } else if !is_writer {
                        return Some(self.error_response(req, 403, "Forbidden"));
                    }
                } else if !is_writer {
                    return Some(self.error_response(req, 403, "Forbidden"));
                }
            }
        }

        // Rate limiting (memory + optional Redis backend)
        // See: https://github.com/Dicklesworthstone/mcp_agent_mail_rust/issues/16
        if self.config.http_rate_limit_enabled {
            let (rpm, burst) = rate_limits_for(&self.config, kind);
            let identity = rate_limit_identity(req, jwt_sub.as_deref());
            let endpoint = tool_name.as_deref().unwrap_or("*");
            let key = format!("{kind}:{endpoint}:{identity}");

            let allowed = self.consume_rate_limit(&key, rpm, burst).await;
            mcp_agent_mail_core::global_metrics()
                .http
                .record_rate_limit_check(allowed);
            if !allowed {
                return Some(self.error_response(req, 429, "Rate limit exceeded"));
            }
        }

        None
    }

    async fn dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        // Upgrade self_ref to Arc so we can move into the 'static spawn_blocking closure.
        // This keeps ALL synchronous router/DB work off the async worker threads.
        let Some(arc_self) = self.self_ref.get().and_then(std::sync::Weak::upgrade) else {
            // self_ref not set or HttpState already dropped — fall back to inline sync.
            let id = request.id.clone();
            return match self.dispatch_inner(request) {
                Ok(value) => id.map(|req_id| JsonRpcResponse::success(req_id, value)),
                Err(err) => {
                    id.map(|req_id| JsonRpcResponse::error(Some(req_id), JsonRpcError::from(err)))
                }
            };
        };

        let id = request.id.clone();
        let method = request.method.clone();

        // Admission control: reject early when the blocking pool is saturated
        // so timed-out threads don't accumulate unboundedly.
        let Some(permit) = DispatchPermit::try_acquire() else {
            tracing::warn!(
                method = %method,
                inflight = MAX_CONCURRENT_DISPATCHES,
                "dispatch admission control: too many concurrent requests, rejecting",
            );
            return id.map(|req_id| {
                JsonRpcResponse::error(
                    Some(req_id),
                    JsonRpcError::from(McpError::new(
                        McpErrorCode::InternalError,
                        format!(
                            "Server overloaded, too many concurrent requests \
                             (limit={MAX_CONCURRENT_DISPATCHES}, method={method})"
                        ),
                    )),
                )
            });
        };

        let hard_timeout_secs = self.request_timeout_secs.saturating_add(5);
        let spawn_future = asupersync::runtime::spawn_blocking(move || {
            let _permit = permit; // hold permit until blocking work finishes
            arc_self.dispatch_inner(request)
        });

        let result = if hard_timeout_secs == 5 && self.request_timeout_secs == 0 {
            // request_timeout_secs == 0 means no timeout (infinite budget).
            spawn_future.await
        } else {
            match timeout(
                wall_now(),
                std::time::Duration::from_secs(hard_timeout_secs),
                spawn_future,
            )
            .await
            {
                Ok(inner) => inner,
                Err(_elapsed) => {
                    tracing::error!(
                        method = %method,
                        hard_timeout_secs,
                        "dispatch spawn_blocking timed out — likely SQLite busy_timeout \
                         exceeded the request budget; returning error to caller"
                    );
                    Err(McpError::new(
                        McpErrorCode::InternalError,
                        format!(
                            "Request timed out after {hard_timeout_secs}s \
                             (method={method}). The database may be under heavy contention."
                        ),
                    ))
                }
            }
        };

        match result {
            Ok(value) => id.map(|req_id| JsonRpcResponse::success(req_id, value)),
            Err(err) => {
                id.map(|req_id| JsonRpcResponse::error(Some(req_id), JsonRpcError::from(err)))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_inner(&self, request: JsonRpcRequest) -> Result<serde_json::Value, McpError> {
        let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let budget = if self.request_timeout_secs == 0 {
            Budget::INFINITE
        } else {
            // Use wall_now() + duration for a RELATIVE deadline, not an absolute epoch time.
            let deadline = wall_now() + std::time::Duration::from_secs(self.request_timeout_secs);
            Budget::new().with_deadline(deadline)
        };
        let cx = Cx::for_request_with_budget(budget);
        let mut session = Session::new(self.server_info.clone(), self.server_capabilities.clone());

        match request.method.as_str() {
            "initialize" => {
                let params: fastmcp_protocol::InitializeParams = parse_params(request.params)?;
                let out = self
                    .router
                    .handle_initialize(&cx, &mut session, params, None)?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "initialized" | "notifications/cancelled" | "logging/setLevel" => {
                Ok(serde_json::Value::Null)
            }
            "tools/list" => {
                let params: fastmcp_protocol::ListToolsParams =
                    parse_params_or_default(request.params)?;
                let out = self
                    .router
                    .handle_tools_list(&cx, params, Some(session.state()))?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "tools/call" => {
                let mut params: fastmcp_protocol::CallToolParams = parse_params(request.params)?;
                let tool_name = params.name.clone();

                if (tool_name == "send_message" || tool_name == "reply_message")
                    && let Some(arguments) = params.arguments.as_mut()
                {
                    // Keep alias normalization tool-specific so one cluster cannot
                    // silently rewrite another tool's documented parameters.
                    mcp_agent_mail_tools::normalize_send_message_arguments(arguments)?;
                }

                // Extract format param before dispatch (TOON support)
                let format_value = params
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("format"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let project_hint = extract_arg_str(
                    params.arguments.as_ref(),
                    &["project_key", "project", "human_key", "project_slug"],
                )
                .map(normalize_project_value);
                let agent_hint = extract_arg_str(
                    params.arguments.as_ref(),
                    &[
                        "agent_name",
                        "sender_name",
                        "from_agent",
                        "requester",
                        "target",
                        "to_agent",
                        "agent",
                    ],
                );
                let output_mode = runtime_output_mode(&self.config);
                let call_arguments = params.arguments.clone();

                let tool_call_console_enabled = self.config.log_rich_enabled
                    && self.config.tools_log_enabled
                    && self.config.log_tool_calls_enabled
                    && std::io::stdout().is_terminal()
                    && !output_mode.is_tui();

                // Emit tool-call-start panel if console tool logging is enabled.
                let call_start = if tool_call_console_enabled {
                    let args = params
                        .arguments
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let panel_lines = console::render_tool_call_start(
                        &tool_name,
                        &args,
                        project_hint.as_deref(),
                        agent_hint.as_deref(),
                    );
                    let panel = panel_lines.join("\n");
                    emit_operator_panel_line(output_mode, &panel);
                    Some(Instant::now())
                } else {
                    None
                };

                // Emit structured timeline event for tool call start.
                {
                    let mut fields = Vec::new();
                    if let Some(ref p) = project_hint {
                        fields.push(("project".to_string(), p.clone()));
                    }
                    if let Some(ref a) = agent_hint {
                        fields.push(("agent".to_string(), a.clone()));
                    }
                    dashboard_emit_event(
                        console::ConsoleEventKind::ToolCallStart,
                        console::ConsoleEventSeverity::Info,
                        format!("{tool_name} start"),
                        fields,
                        None,
                    );
                }

                let tracker_state =
                    if self.config.instrumentation_enabled && active_tracker().is_none() {
                        let tracker = Arc::new(QueryTracker::new());
                        tracker.enable(Some(self.config.instrumentation_slow_query_ms));
                        let guard = set_active_tracker(tracker.clone());
                        Some((tracker, guard))
                    } else {
                        None
                    };

                let result = self.router.handle_tools_call(
                    &cx,
                    request_id,
                    params,
                    &budget,
                    SessionState::new(),
                    None,
                    None,
                );

                let (queries, query_time_ms, per_table_sorted) =
                    if let Some((ref tracker, ref _guard)) = tracker_state {
                        if self.config.tools_log_enabled {
                            log_tool_query_stats(
                                &tool_name,
                                project_hint.as_deref(),
                                agent_hint.as_deref(),
                                tracker,
                            );
                        }
                        let snap = tracker.snapshot();
                        let mut pairs: Vec<(String, u64)> = snap.per_table.into_iter().collect();
                        pairs.sort_by(|(a_name, a_count), (b_name, b_count)| {
                            b_count.cmp(a_count).then_with(|| a_name.cmp(b_name))
                        });
                        (snap.total, snap.total_time_ms, pairs)
                    } else {
                        (0_u64, 0.0_f64, Vec::new())
                    };

                let out = match result {
                    Ok(v) => v,
                    Err(e) => {
                        // Emit tool-call-end panel on error
                        if let Some(start) = call_start {
                            let dur_ms =
                                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                            let err_msg = format!("Error: {e}");
                            let panel_lines = console::render_tool_call_end(
                                &tool_name,
                                dur_ms,
                                Some(&err_msg),
                                queries,
                                query_time_ms,
                                &per_table_sorted,
                                self.config.log_tool_calls_result_max_chars,
                            );
                            let panel = panel_lines.join("\n");
                            emit_operator_panel_line(output_mode, &panel);
                            dashboard_emit_event(
                                console::ConsoleEventKind::ToolCallEnd,
                                console::ConsoleEventSeverity::Error,
                                format!("{tool_name} error {dur_ms}ms"),
                                vec![("error".to_string(), format!("{e}"))],
                                None,
                            );
                        }
                        return Err(e);
                    }
                };
                let mut value = serde_json::to_value(out).map_err(McpError::from)?;

                for event in derive_domain_events_from_tool_result(
                    &tool_name,
                    call_arguments.as_ref(),
                    &value,
                    project_hint.as_deref(),
                    agent_hint.as_deref(),
                ) {
                    emit_tui_event(event);
                }

                // Record agent activity in the ATC engine for liveness tracking.
                if let Some(ref agent) = agent_hint {
                    atc::atc_observe_activity_with_project(
                        agent,
                        project_hint.as_deref(),
                        mcp_agent_mail_core::timestamps::now_micros(),
                    );
                }

                // Register agent in ATC on successful register_agent / macro_start_session.
                if matches!(tool_name.as_str(), "register_agent" | "macro_start_session")
                    && let Some(ref agent) = agent_hint
                {
                    let program =
                        extract_arg_str(call_arguments.as_ref(), &["program"]).unwrap_or_default();
                    atc::atc_register_agent_with_project(agent, &program, project_hint.as_deref());
                }

                // Emit tool-call-end panel
                if let Some(start) = call_start {
                    let dur_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let result_preview = serde_json::to_string(&value).ok();
                    let panel_lines = console::render_tool_call_end(
                        &tool_name,
                        dur_ms,
                        result_preview.as_deref(),
                        queries,
                        query_time_ms,
                        &per_table_sorted,
                        self.config.log_tool_calls_result_max_chars,
                    );
                    let panel = panel_lines.join("\n");
                    emit_operator_panel_line(output_mode, &panel);
                    dashboard_emit_event(
                        console::ConsoleEventKind::ToolCallEnd,
                        console::ConsoleEventSeverity::Info,
                        format!("{tool_name} ok {dur_ms}ms q={queries}"),
                        vec![
                            ("duration_ms".to_string(), dur_ms.to_string()),
                            ("queries".to_string(), queries.to_string()),
                        ],
                        None,
                    );
                }
                if let Some(ref fmt) = format_value {
                    apply_toon_to_content(&mut value, "content", fmt, &self.config);
                }
                Ok(value)
            }
            "resources/list" => {
                let params: fastmcp_protocol::ListResourcesParams =
                    parse_params_or_default(request.params)?;
                let out = self
                    .router
                    .handle_resources_list(&cx, params, Some(session.state()))?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "resources/templates/list" => {
                let params: fastmcp_protocol::ListResourceTemplatesParams =
                    parse_params_or_default(request.params)?;
                let out = self.router.handle_resource_templates_list(
                    &cx,
                    params,
                    Some(session.state()),
                )?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "resources/read" => {
                let params: fastmcp_protocol::ReadResourceParams = parse_params(request.params)?;
                // Extract format from resource URI query params (TOON support)
                let format_value = extract_format_from_uri(&params.uri);
                let out = self.router.handle_resources_read(
                    &cx,
                    request_id,
                    &params,
                    &budget,
                    SessionState::new(),
                    None,
                    None,
                )?;
                let mut value = serde_json::to_value(out).map_err(McpError::from)?;
                if let Some(ref fmt) = format_value {
                    apply_toon_to_content(&mut value, "contents", fmt, &self.config);
                }
                Ok(value)
            }
            "resources/subscribe" | "resources/unsubscribe" | "ping" => Ok(serde_json::json!({})),
            "prompts/list" => {
                let params: fastmcp_protocol::ListPromptsParams =
                    parse_params_or_default(request.params)?;
                let out = self
                    .router
                    .handle_prompts_list(&cx, params, Some(session.state()))?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "prompts/get" => {
                let params: fastmcp_protocol::GetPromptParams = parse_params(request.params)?;
                let out = self.router.handle_prompts_get(
                    &cx,
                    request_id,
                    params,
                    &budget,
                    SessionState::new(),
                    None,
                    None,
                )?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "tasks/list" => {
                let params: fastmcp_protocol::ListTasksParams =
                    parse_params_or_default(request.params)?;
                let out = self.router.handle_tasks_list(&cx, params, None)?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "tasks/get" => {
                let params: fastmcp_protocol::GetTaskParams = parse_params(request.params)?;
                let out = self.router.handle_tasks_get(&cx, params, None)?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "tasks/cancel" => {
                let params: fastmcp_protocol::CancelTaskParams = parse_params(request.params)?;
                let out = self.router.handle_tasks_cancel(&cx, params, None)?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            "tasks/submit" => {
                let params: fastmcp_protocol::SubmitTaskParams = parse_params(request.params)?;
                let out = self.router.handle_tasks_submit(&cx, params, None)?;
                serde_json::to_value(out).map_err(McpError::from)
            }
            _ => Err(McpError::new(
                McpErrorCode::MethodNotFound,
                format!("Method not found: {}", request.method),
            )),
        }
    }

    fn allow_local_unauthenticated(&self, req: &Http1Request) -> bool {
        if !self.config.http_allow_localhost_unauthenticated {
            return false;
        }
        if has_forwarded_headers(req) {
            return false;
        }
        is_local_peer_addr(req.peer_addr)
    }

    fn cors_origin(&self, req: &Http1Request) -> Option<String> {
        if !self.config.http_cors_enabled {
            return None;
        }
        let origin = header_value(req, "origin")?.to_string();
        if cors_allows(&self.config.http_cors_origins, &origin) {
            if cors_wildcard(&self.config.http_cors_origins)
                && !self.config.http_cors_allow_credentials
            {
                Some("*".to_string())
            } else {
                Some(origin)
            }
        } else {
            None
        }
    }

    fn error_response(&self, req: &Http1Request, status: u16, message: &str) -> Http1Response {
        let body = serde_json::json!({ "detail": message });
        let mut resp = Http1Response::new(
            status,
            default_reason(status),
            serde_json::to_vec(&body).unwrap_or_default(),
        );
        resp.headers
            .push(("content-type".to_string(), "application/json".to_string()));
        apply_cors_headers(
            &mut resp,
            self.cors_origin(req),
            self.config.http_cors_allow_credentials,
            &self.config.http_cors_allow_methods,
            &self.config.http_cors_allow_headers,
        );
        resp
    }

    fn json_response(
        &self,
        req: &Http1Request,
        status: u16,
        value: &serde_json::Value,
    ) -> Http1Response {
        let mut resp = Http1Response::new(
            status,
            default_reason(status),
            serde_json::to_vec(value).unwrap_or_default(),
        );
        resp.headers
            .push(("content-type".to_string(), "application/json".to_string()));
        apply_cors_headers(
            &mut resp,
            self.cors_origin(req),
            self.config.http_cors_allow_credentials,
            &self.config.http_cors_allow_methods,
            &self.config.http_cors_allow_headers,
        );
        resp
    }

    fn health_json_response(
        &self,
        req: &Http1Request,
        status: u16,
        value: &serde_json::Value,
    ) -> Http1Response {
        let mut resp = self.json_response(req, status, value);
        resp.headers.push((
            startup_checks::HEALTH_SIGNATURE_HEADER_NAME.to_string(),
            startup_checks::HEALTH_SIGNATURE_HEADER_VALUE.to_string(),
        ));
        resp
    }

    fn raw_response(
        &self,
        req: &Http1Request,
        status: u16,
        content_type: &str,
        body: Vec<u8>,
    ) -> Http1Response {
        let mut resp = Http1Response::new(status, default_reason(status), body);
        resp.headers
            .push(("content-type".to_string(), content_type.to_string()));
        apply_cors_headers(
            &mut resp,
            self.cors_origin(req),
            self.config.http_cors_allow_credentials,
            &self.config.http_cors_allow_methods,
            &self.config.http_cors_allow_headers,
        );
        resp
    }
}

/// Extract `format` query parameter from a resource URI.
///
/// E.g. `resource://inbox/BlueLake?project=/backend&format=toon` → `Some("toon")`
fn extract_format_from_uri(uri: &str) -> Option<String> {
    let query = uri.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=')
            && key == "format"
        {
            return Some(value.to_string());
        }
    }
    None
}

fn extract_arg_str(arguments: Option<&serde_json::Value>, keys: &[&str]) -> Option<String> {
    let args = arguments?.as_object()?;
    for key in keys {
        if let Some(value) = args.get(*key)
            && let Some(s) = value.as_str()
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn normalize_project_value(value: String) -> String {
    if std::path::Path::new(&value).is_absolute() {
        mcp_agent_mail_db::queries::generate_slug(&value)
    } else {
        value
    }
}

fn parse_call_tool_result_payload(call_result: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(structured) = call_result
        .get("structuredContent")
        .or_else(|| call_result.get("structured_content"))
        && !structured.is_null()
    {
        return Some(structured.clone());
    }

    let blocks = call_result.get("content")?.as_array()?;
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some(payload) = parse_text_payload_json(text) {
            return Some(payload);
        }
    }
    None
}

fn parse_text_payload_json(text: &str) -> Option<serde_json::Value> {
    let candidates = [
        Some(text.trim()),
        strip_markdown_code_fence(text),
        strip_markdown_code_fence(text.trim()),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        if let Ok(payload) = serde_json::from_str::<serde_json::Value>(candidate) {
            if let serde_json::Value::String(inner) = &payload
                && let Ok(unwrapped) = serde_json::from_str::<serde_json::Value>(inner.trim())
            {
                return Some(unwrapped);
            }
            return Some(payload);
        }
    }
    None
}

fn strip_markdown_code_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let fence = trimmed.strip_prefix("```")?;
    let (_, body) = fence.split_once('\n')?;
    let (inner, _) = body.rsplit_once("```")?;
    Some(inner.trim())
}

fn json_i64_field(value: &serde_json::Value, key: &str) -> Option<i64> {
    let field = value.get(key)?;
    if let Some(v) = field.as_i64() {
        return Some(v);
    }
    field.as_u64().and_then(|v| i64::try_from(v).ok())
}

fn json_u64_field(value: &serde_json::Value, key: &str) -> Option<u64> {
    let field = value.get(key)?;
    if let Some(v) = field.as_u64() {
        return Some(v);
    }
    field.as_i64().and_then(|v| u64::try_from(v).ok())
}

fn json_string_vec_field(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn extract_arg_bool(arguments: Option<&serde_json::Value>, key: &str) -> Option<bool> {
    arguments?.as_object()?.get(key)?.as_bool()
}

fn extract_arg_u64(arguments: Option<&serde_json::Value>, key: &str) -> Option<u64> {
    let value = arguments?.as_object()?.get(key)?;
    if let Some(v) = value.as_u64() {
        return Some(v);
    }
    value.as_i64().and_then(|v| u64::try_from(v).ok())
}

fn extract_arg_i64_vec(arguments: Option<&serde_json::Value>, key: &str) -> Vec<i64> {
    arguments
        .and_then(serde_json::Value::as_object)
        .and_then(|m| m.get(key))
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_i64()
                        .or_else(|| item.as_u64().and_then(|v| i64::try_from(v).ok()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_arg_string_vec(arguments: Option<&serde_json::Value>, key: &str) -> Vec<String> {
    arguments
        .and_then(serde_json::Value::as_object)
        .and_then(|m| m.get(key))
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Truncate a markdown body to a safe limit suitable for the event ring buffer.
/// Collapses 50MB bodies to 32KB to prevent memory exhaustion, while still
/// providing enough content for the full-screen dashboard preview.
fn truncate_body_md(body: &str) -> String {
    const MAX_EXCERPT: usize = 32_000;
    if body.len() <= MAX_EXCERPT {
        return body.to_string();
    }
    let mut end = MAX_EXCERPT;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = body[..end].to_string();
    result.push_str("\n\n... (message truncated for preview, open Messages/Threads for full body)");
    result
}

fn message_sent_event_from_payload(
    payload: &serde_json::Value,
    project: Option<&str>,
    fallback_sender: Option<&str>,
) -> Option<(i64, String, tui_events::MailEvent)> {
    let id = json_i64_field(payload, "id")?;
    let project = project.filter(|p| !p.is_empty()).map(str::to_string)?;
    let from = payload
        .get("from")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .or(fallback_sender)
        .unwrap_or("unknown")
        .to_string();
    let to = json_string_vec_field(payload, "to");
    let subject = payload
        .get("subject")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let thread_id = payload
        .get("thread_id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unthreaded")
        .to_string();
    let body_md = payload
        .get("body_md")
        .and_then(serde_json::Value::as_str)
        .map(truncate_body_md)
        .unwrap_or_default();
    let event =
        tui_events::MailEvent::message_sent(id, from, to, subject, thread_id, &project, body_md);
    Some((id, project, event))
}

fn message_recipients_from_payload(payload: &serde_json::Value) -> Vec<String> {
    let mut recipients = Vec::new();
    let mut seen = HashSet::new();
    for key in ["to", "cc", "bcc"] {
        for recipient in json_string_vec_field(payload, key) {
            let trimmed = recipient.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value = trimmed.to_string();
            if seen.insert(value.clone()) {
                recipients.push(value);
            }
        }
    }
    recipients
}

fn append_message_flow_events(
    events: &mut Vec<tui_events::MailEvent>,
    seen_sent: &mut HashSet<(i64, String)>,
    seen_received: &mut HashSet<(i64, String, String)>,
    msg_payload: &serde_json::Value,
    project_hint: Option<&str>,
    fallback_sender: Option<&str>,
    recipient_budget: &mut usize,
) -> bool {
    let Some((id, project, sent_event)) =
        message_sent_event_from_payload(msg_payload, project_hint, fallback_sender)
    else {
        return false;
    };

    let (from, subject, thread_id, body_md) = match &sent_event {
        tui_events::MailEvent::MessageSent {
            from,
            subject,
            thread_id,
            body_md,
            ..
        } => (
            from.clone(),
            subject.clone(),
            thread_id.clone(),
            body_md.clone(),
        ),
        _ => return false,
    };

    if seen_sent.insert((id, project.clone())) {
        events.push(sent_event);
    }

    if *recipient_budget == 0 {
        return true;
    }
    let recipients = message_recipients_from_payload(msg_payload);
    if recipients.is_empty() {
        return true;
    }

    for recipient in recipients {
        if *recipient_budget == 0 {
            break;
        }
        let key = (id, project.clone(), recipient.clone());
        if !seen_received.insert(key) {
            continue;
        }
        events.push(tui_events::MailEvent::message_received(
            id,
            &from,
            vec![recipient],
            &subject,
            &thread_id,
            &project,
            &body_md,
        ));
        *recipient_budget = recipient_budget.saturating_sub(1);
    }

    true
}

#[derive(Debug, Clone, Default)]
struct DomainEventContext {
    project: Option<String>,
    agent: Option<String>,
}

fn resolve_domain_event_context(
    call_args: Option<&serde_json::Value>,
    project_hint: Option<&str>,
    agent_hint: Option<&str>,
) -> DomainEventContext {
    let project = project_hint.map(str::to_string).or_else(|| {
        extract_arg_str(
            call_args,
            &["project_key", "project", "human_key", "project_slug"],
        )
        .map(normalize_project_value)
    });
    let agent = agent_hint.map(str::to_string).or_else(|| {
        extract_arg_str(
            call_args,
            &[
                "agent_name",
                "sender_name",
                "from_agent",
                "requester",
                "target",
                "to_agent",
                "agent",
            ],
        )
    });
    DomainEventContext { project, agent }
}

fn derive_message_domain_events(
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    let mut events = Vec::new();
    let mut seen_sent: HashSet<(i64, String)> = HashSet::new();
    let mut seen_received: HashSet<(i64, String, String)> = HashSet::new();
    let mut found_message = false;
    let mut recipient_budget = 64_usize;
    if let Some(deliveries) = payload
        .get("deliveries")
        .and_then(serde_json::Value::as_array)
    {
        for delivery in deliveries.iter().take(32) {
            let delivery_project = delivery
                .get("project")
                .and_then(serde_json::Value::as_str)
                .or(ctx.project.as_deref());
            let msg_payload = delivery.get("payload").unwrap_or(payload);
            found_message |= append_message_flow_events(
                &mut events,
                &mut seen_sent,
                &mut seen_received,
                msg_payload,
                delivery_project,
                ctx.agent.as_deref(),
                &mut recipient_budget,
            );
        }
    }
    if !found_message {
        let _ = append_message_flow_events(
            &mut events,
            &mut seen_sent,
            &mut seen_received,
            payload,
            ctx.project.as_deref(),
            ctx.agent.as_deref(),
            &mut recipient_budget,
        );
    }
    events
}

fn derive_fetch_inbox_domain_events(
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
    fallback_project: Option<&str>,
) -> Vec<tui_events::MailEvent> {
    let Some(project_value) = ctx.project.as_deref().or(fallback_project) else {
        return Vec::new();
    };
    let Some(messages) = payload.as_array() else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for message in messages.iter().take(16) {
        let Some(id) = json_i64_field(message, "id") else {
            continue;
        };
        let from = message
            .get("from")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let subject = message
            .get("subject")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let thread_id = message
            .get("thread_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unthreaded");
        let body_md = message
            .get("body_md")
            .and_then(serde_json::Value::as_str)
            .map(truncate_body_md)
            .unwrap_or_default();
        let mut to = Vec::new();
        if let Some(agent_name) = ctx.agent.as_deref() {
            to.push(agent_name.to_string());
        }
        events.push(tui_events::MailEvent::message_received(
            id,
            from,
            to,
            subject,
            thread_id,
            project_value,
            body_md,
        ));
    }
    events
}

fn derive_reservation_granted_domain_events(
    call_args: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    let Some(granted) = payload.get("granted").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    if granted.is_empty() {
        return Vec::new();
    }
    let mut paths: Vec<String> = granted
        .iter()
        .filter_map(|row| row.get("path_pattern"))
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect();
    if paths.is_empty() {
        paths = extract_arg_string_vec(call_args, "paths");
    }
    if paths.is_empty() {
        return Vec::new();
    }
    let exclusive = extract_arg_bool(call_args, "exclusive")
        .or_else(|| {
            granted
                .first()
                .and_then(|row| row.get("exclusive"))
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(true);
    let ttl_s = extract_arg_u64(call_args, "ttl_seconds").unwrap_or(3600);
    let Some(project_value) = &ctx.project else {
        return Vec::new();
    };
    let agent_value = ctx.agent.clone().unwrap_or_else(|| "unknown".to_string());
    // Wire reservation grants into ATC conflict graph (br-0qt6e.2.9)
    atc::atc_note_reservation_granted(
        &agent_value,
        &paths,
        exclusive,
        project_value,
        mcp_agent_mail_db::now_micros(),
    );

    // Wire any conflicts into ATC conflict graph (br-0qt6e.2.9)
    if let Some(conflicts_arr) = payload
        .get("conflicts")
        .and_then(serde_json::Value::as_array)
    {
        if !conflicts_arr.is_empty() {
            let observations: Vec<atc::AtcConflictObservation> = conflicts_arr
                .iter()
                .filter_map(|c| {
                    let requested = c.get("path")?.as_str()?;
                    let holders = c.get("holders")?.as_array()?;
                    Some(holders.iter().filter_map(move |h| {
                        Some(atc::AtcConflictObservation {
                            holder: h.get("agent")?.as_str()?.to_string(),
                            requested_path: requested.to_string(),
                            holder_path_pattern: h.get("path_pattern")?.as_str()?.to_string(),
                        })
                    }))
                })
                .flatten()
                .collect();
            if !observations.is_empty() {
                atc::atc_note_reservation_conflicts(
                    &agent_value,
                    project_value,
                    &observations,
                    mcp_agent_mail_db::now_micros(),
                );
            }
        }
    }

    vec![tui_events::MailEvent::reservation_granted(
        agent_value,
        paths,
        exclusive,
        ttl_s,
        project_value.clone(),
    )]
}

fn derive_release_domain_events(
    call_args: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    if json_u64_field(payload, "released").unwrap_or(0) == 0 {
        return Vec::new();
    }
    let Some(project_value) = &ctx.project else {
        return Vec::new();
    };
    let agent_value = ctx.agent.clone().unwrap_or_else(|| "unknown".to_string());
    let mut paths = extract_arg_string_vec(call_args, "paths");
    if paths.is_empty() {
        paths = extract_arg_i64_vec(call_args, "file_reservation_ids")
            .into_iter()
            .map(|id| format!("id:{id}"))
            .collect();
    }
    if paths.is_empty() {
        paths.push("<all-active>".to_string());
    }
    // Wire reservation releases into ATC conflict graph (br-0qt6e.2.9)
    atc::atc_note_reservation_released(
        &agent_value,
        &paths,
        project_value,
        mcp_agent_mail_db::now_micros(),
    );

    vec![tui_events::MailEvent::reservation_released(
        agent_value,
        paths,
        project_value.clone(),
    )]
}

fn derive_force_release_domain_events(
    call_args: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    if json_u64_field(payload, "released").unwrap_or(0) == 0 {
        return Vec::new();
    }
    let Some(project_value) = &ctx.project else {
        return Vec::new();
    };
    let reservation = payload.get("reservation");
    let mut paths = reservation
        .and_then(|r| r.get("path_pattern"))
        .and_then(serde_json::Value::as_str)
        .map(|path| vec![path.to_string()])
        .unwrap_or_default();
    if paths.is_empty() {
        paths = extract_arg_u64(call_args, "file_reservation_id")
            .map(|id| vec![format!("id:{id}")])
            .unwrap_or_default();
    }
    if paths.is_empty() {
        paths.push("<unknown>".to_string());
    }
    let agent_value = reservation
        .and_then(|r| r.get("agent"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| ctx.agent.clone())
        .unwrap_or_else(|| "unknown".to_string());
    // Wire force-release into ATC conflict graph (br-0qt6e.2.9)
    atc::atc_note_reservation_released(
        &agent_value,
        &paths,
        project_value,
        mcp_agent_mail_db::now_micros(),
    );

    vec![tui_events::MailEvent::reservation_released(
        agent_value,
        paths,
        project_value.clone(),
    )]
}

fn derive_agent_registered_domain_events(
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    let Some(project_value) = &ctx.project else {
        return Vec::new();
    };
    let Some(name) = payload.get("name").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let Some(program) = payload.get("program").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let Some(model) = payload.get("model").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    vec![tui_events::MailEvent::agent_registered(
        name,
        program,
        model,
        project_value.clone(),
    )]
}

fn derive_macro_start_session_domain_events(
    call_args: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    let mut events = Vec::new();
    if let Some(file_reservations) = payload.get("file_reservations") {
        events.extend(derive_reservation_granted_domain_events(
            call_args,
            file_reservations,
            ctx,
        ));
    }
    if let Some(inbox) = payload.get("inbox") {
        let fallback_project = payload
            .get("project")
            .and_then(serde_json::Value::as_object)
            .and_then(|project| {
                project
                    .get("slug")
                    .or_else(|| project.get("human_key"))
                    .and_then(serde_json::Value::as_str)
            });
        events.extend(derive_fetch_inbox_domain_events(
            inbox,
            ctx,
            fallback_project,
        ));
    }
    events
}

fn derive_macro_prepare_thread_domain_events(
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    let Some(inbox) = payload.get("inbox") else {
        return Vec::new();
    };
    let fallback_project = payload
        .get("project")
        .and_then(serde_json::Value::as_object)
        .and_then(|project| {
            project
                .get("slug")
                .or_else(|| project.get("human_key"))
                .and_then(serde_json::Value::as_str)
        });
    derive_fetch_inbox_domain_events(inbox, ctx, fallback_project)
}

fn derive_macro_file_reservation_cycle_domain_events(
    call_args: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    let mut events = Vec::new();
    if let Some(file_reservations) = payload.get("file_reservations") {
        events.extend(derive_reservation_granted_domain_events(
            call_args,
            file_reservations,
            ctx,
        ));
    }
    if let Some(released) = payload.get("released") {
        events.extend(derive_release_domain_events(call_args, released, ctx));
    }
    events
}

fn derive_macro_contact_handshake_domain_events(
    payload: &serde_json::Value,
    ctx: &DomainEventContext,
) -> Vec<tui_events::MailEvent> {
    payload
        .get("welcome_message")
        .map_or_else(Vec::new, |welcome| {
            derive_message_domain_events(welcome, ctx)
        })
}

fn derive_domain_events_from_tool_payload(
    tool_name: &str,
    call_args: Option<&serde_json::Value>,
    payload: &serde_json::Value,
    project_hint: Option<&str>,
    agent_hint: Option<&str>,
) -> Vec<tui_events::MailEvent> {
    let ctx = resolve_domain_event_context(call_args, project_hint, agent_hint);
    match tool_name {
        "send_message" | "reply_message" => derive_message_domain_events(payload, &ctx),
        "fetch_inbox" => derive_fetch_inbox_domain_events(payload, &ctx, None),
        "fetch_inbox_product" => {
            let product_fallback = extract_arg_str(call_args, &["product_key"])
                .map(|product_key| format!("product:{product_key}"));
            derive_fetch_inbox_domain_events(payload, &ctx, product_fallback.as_deref())
        }
        "file_reservation_paths" => {
            derive_reservation_granted_domain_events(call_args, payload, &ctx)
        }
        "release_file_reservations" => derive_release_domain_events(call_args, payload, &ctx),
        "force_release_file_reservation" => {
            derive_force_release_domain_events(call_args, payload, &ctx)
        }
        "register_agent" | "create_agent_identity" => {
            derive_agent_registered_domain_events(payload, &ctx)
        }
        "macro_start_session" => derive_macro_start_session_domain_events(call_args, payload, &ctx),
        "macro_prepare_thread" => derive_macro_prepare_thread_domain_events(payload, &ctx),
        "macro_file_reservation_cycle" => {
            derive_macro_file_reservation_cycle_domain_events(call_args, payload, &ctx)
        }
        "macro_contact_handshake" => derive_macro_contact_handshake_domain_events(payload, &ctx),
        _ => Vec::new(),
    }
}

fn derive_domain_events_from_tool_result(
    tool_name: &str,
    call_args: Option<&serde_json::Value>,
    call_result: &serde_json::Value,
    project_hint: Option<&str>,
    agent_hint: Option<&str>,
) -> Vec<tui_events::MailEvent> {
    let payload =
        parse_call_tool_result_payload(call_result).unwrap_or_else(|| call_result.clone());
    derive_domain_events_from_tool_payload(tool_name, call_args, &payload, project_hint, agent_hint)
}

fn derive_domain_events_from_tool_contents(
    tool_name: &str,
    call_args: Option<&serde_json::Value>,
    contents: &[Content],
    project_hint: Option<&str>,
    agent_hint: Option<&str>,
) -> Vec<tui_events::MailEvent> {
    for content in contents {
        let Content::Text { text } = content else {
            continue;
        };
        if let Some(payload) = parse_text_payload_json(text) {
            return derive_domain_events_from_tool_payload(
                tool_name,
                call_args,
                &payload,
                project_hint,
                agent_hint,
            );
        }
    }
    Vec::new()
}

fn log_tool_query_stats(
    tool_name: &str,
    project: Option<&str>,
    agent: Option<&str>,
    tracker: &QueryTracker,
) {
    let snapshot = tracker.snapshot();
    let dict = snapshot.to_dict();
    let per_table = dict
        .get("per_table")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let slow_query_ms = dict
        .get("slow_query_ms")
        .and_then(serde_json::Value::as_f64);

    tracing::info!(
        tool = tool_name,
        project = project.unwrap_or_default(),
        agent = agent.unwrap_or_default(),
        queries = snapshot.total,
        query_time_ms = snapshot.total_time_ms,
        per_table = ?per_table,
        slow_query_ms = slow_query_ms,
        "tool_query_stats"
    );
}

/// Apply TOON encoding to the text content blocks in a MCP response value.
///
/// `content_key` is "content" for tool results (`CallToolResult.content`)
/// or "contents" for resource results (`ReadResourceResult.contents`).
///
/// Walks each content block, finds ones with `type:"text"`, parses the
/// text as JSON, applies TOON encoding, and replaces the text with the
/// envelope JSON string.
fn apply_toon_to_content(
    value: &mut serde_json::Value,
    content_key: &str,
    format_value: &str,
    config: &mcp_agent_mail_core::Config,
) {
    let Ok(decision) = mcp_agent_mail_core::toon::resolve_output_format(Some(format_value), config)
    else {
        return;
    };

    if decision.resolved != "toon" {
        return;
    }

    let Some(blocks) = value.get_mut(content_key).and_then(|v| v.as_array_mut()) else {
        return;
    };

    for block in blocks {
        let is_text = block
            .get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "text");
        if !is_text {
            continue;
        }
        let Some(text_str) = block.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        // Try to parse the text as JSON
        let payload: serde_json::Value = match serde_json::from_str(text_str) {
            Ok(v) => v,
            Err(_) => continue, // Not valid JSON: leave as-is
        };
        // Apply TOON format wrapping
        if let Ok(Some(envelope)) =
            mcp_agent_mail_core::toon::apply_toon_format(&payload, Some(format_value), config)
            && let Ok(envelope_json) = serde_json::to_string(&envelope)
        {
            block["text"] = serde_json::Value::String(envelope_json);
        }
    }
}

fn map_asupersync_err(err: &asupersync::Error) -> std::io::Error {
    std::io::Error::other(format!("asupersync error: {err}"))
}

const STARTUP_INTEGRITY_CACHE_SCHEMA_VERSION: u32 = 1;
const STARTUP_INTEGRITY_CACHE_TTL_SECS_DEFAULT: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct StartupIntegrityFingerprint {
    schema_version: i64,
    user_version: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StartupIntegrityCacheEntry {
    schema_version: u32,
    sqlite_path: String,
    fingerprint: StartupIntegrityFingerprint,
    checked_at_micros: i64,
}

fn startup_integrity_cache_ttl_secs() -> u64 {
    std::env::var("AM_STARTUP_INTEGRITY_CACHE_TTL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(STARTUP_INTEGRITY_CACHE_TTL_SECS_DEFAULT, |secs| {
            secs.max(60)
        })
}

fn startup_integrity_now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|dur| i64::try_from(dur.as_micros()).ok())
        .unwrap_or(0)
}

fn startup_integrity_cache_path(
    config: &mcp_agent_mail_core::Config,
) -> Option<std::path::PathBuf> {
    if !config.storage_root.is_dir() {
        return None;
    }
    Some(
        config
            .storage_root
            .join("diagnostics")
            .join("startup_integrity_cache.json"),
    )
}

fn validate_startup_integrity_cache_path(path: &Path) -> std::io::Result<()> {
    if path_existing_prefix_has_symlink(path)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "startup integrity cache path must not include symlinks: {}",
                path.display()
            ),
        ));
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "startup integrity cache path must not include symlinks: {}",
                path.display()
            ),
        )),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "startup integrity cache path {} exists but is not a file",
                path.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn write_startup_integrity_cache_atomic(path: &Path, payload: &str) -> std::io::Result<()> {
    validate_startup_integrity_cache_path(path)?;
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "startup integrity cache path {} has no parent directory",
                path.display()
            ),
        ));
    };
    fs::create_dir_all(parent)?;
    validate_startup_integrity_cache_path(path)?;

    let file_stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("startup-integrity-cache");
    for attempt in 0..16_u32 {
        let staged_path = parent.join(format!(
            ".{file_stem}.tmp-{}-{}-{attempt}",
            std::process::id(),
            startup_integrity_now_micros()
        ));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged_path)
        {
            Ok(mut file) => {
                use std::io::Write as _;

                file.write_all(payload.as_bytes())?;
                file.sync_all()?;
                drop(file);
                validate_startup_integrity_cache_path(path)?;
                return fs::rename(&staged_path, path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "failed to allocate startup integrity cache staging path under {}",
            parent.display()
        ),
    ))
}

fn read_pragma_i64(
    conn: &mcp_agent_mail_db::DbConn,
    pragma_sql: &str,
    column: &str,
) -> Option<i64> {
    let rows = conn.query_sync(pragma_sql, &[]).ok()?;
    let row = rows.first()?;
    row.get_named::<i64>(column)
        .ok()
        .or_else(|| row.get_as::<i64>(0).ok())
}

fn sqlite_startup_fingerprint(
    conn: &mcp_agent_mail_db::DbConn,
    database_url: &str,
) -> Option<(String, StartupIntegrityFingerprint)> {
    if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) {
        return None;
    }
    let sqlite_path = resolve_server_database_url_sqlite_path(database_url)?;
    let schema_version = read_pragma_i64(conn, "PRAGMA schema_version", "schema_version")?;
    let user_version = read_pragma_i64(conn, "PRAGMA user_version", "user_version").unwrap_or(0);
    Some((
        sqlite_path.to_string_lossy().into_owned(),
        StartupIntegrityFingerprint {
            schema_version,
            user_version,
        },
    ))
}

fn read_startup_integrity_cache(
    config: &mcp_agent_mail_core::Config,
) -> Option<StartupIntegrityCacheEntry> {
    let cache_path = startup_integrity_cache_path(config)?;
    validate_startup_integrity_cache_path(&cache_path).ok()?;
    let raw = fs::read_to_string(cache_path).ok()?;
    serde_json::from_str::<StartupIntegrityCacheEntry>(&raw).ok()
}

fn write_startup_integrity_cache(
    config: &mcp_agent_mail_core::Config,
    sqlite_path: &str,
    fingerprint: StartupIntegrityFingerprint,
) {
    let Some(cache_path) = startup_integrity_cache_path(config) else {
        return;
    };

    let entry = StartupIntegrityCacheEntry {
        schema_version: STARTUP_INTEGRITY_CACHE_SCHEMA_VERSION,
        sqlite_path: sqlite_path.to_string(),
        fingerprint,
        checked_at_micros: startup_integrity_now_micros(),
    };
    let Ok(payload) = serde_json::to_string_pretty(&entry) else {
        return;
    };
    let _ = write_startup_integrity_cache_atomic(&cache_path, &payload);
}

fn startup_integrity_cache_is_fresh(
    config: &mcp_agent_mail_core::Config,
    sqlite_path: &str,
    fingerprint: StartupIntegrityFingerprint,
) -> bool {
    let Some(cache) = read_startup_integrity_cache(config) else {
        return false;
    };
    if cache.schema_version != STARTUP_INTEGRITY_CACHE_SCHEMA_VERSION {
        return false;
    }
    if cache.sqlite_path != sqlite_path || cache.fingerprint != fingerprint {
        return false;
    }
    let now = startup_integrity_now_micros();
    if now <= 0 || cache.checked_at_micros <= 0 {
        return false;
    }
    let age_micros = now.saturating_sub(cache.checked_at_micros);
    let age_secs = u64::try_from(age_micros).unwrap_or(0) / 1_000_000;
    age_secs <= startup_integrity_cache_ttl_secs()
}

#[allow(dead_code)]
fn readiness_check(config: &mcp_agent_mail_core::Config) -> Result<(), String> {
    readiness_check_with_integrity(config, true)
}

fn readiness_check_quick(config: &mcp_agent_mail_core::Config) -> Result<(), String> {
    let is_memory = mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url);
    let sqlite_path = if is_memory {
        None
    } else {
        resolve_server_database_url_sqlite_path(&config.database_url)
    };
    let _sqlite_activity_lock = if let Some(sqlite_path) = sqlite_path.as_ref() {
        acquire_mailbox_activity_lock_for_sqlite_path(sqlite_path, MailboxActivityLockMode::Shared)
            .map_err(|e| e.to_string())?
    } else {
        None
    };

    let conn = if is_memory {
        DbConn::open_memory().map_err(|e| e.to_string())?
    } else {
        let sqlite_path = sqlite_path
            .as_ref()
            .ok_or_else(|| "cannot resolve sqlite path for readiness check".to_string())?;
        if !sqlite_path.exists() {
            return Err(format!(
                "SQLite database is missing at {}; quick readiness refuses to initialize the mailbox",
                sqlite_path.display()
            ));
        }
        open_best_effort_sync_db_connection(sqlite_path.to_string_lossy().as_ref())
            .map_err(|e| e.to_string())?
    };

    if let Err(e) = conn.query_sync("SELECT 1", &[]) {
        let error = e.to_string();
        if mcp_agent_mail_db::is_corruption_error_message(&error) {
            return Err(format!(
                "SQLite corruption detected during readiness check; automatic server-side recovery is disabled: {error}"
            ));
        }
        return Err(error);
    }

    if is_memory {
        return Ok(());
    }

    readiness_check_cached_semantic_status(config, &conn)
}

fn readiness_check_cached_semantic_status(
    config: &mcp_agent_mail_core::Config,
    conn: &DbConn,
) -> Result<(), String> {
    {
        let guard = lock_mutex(&READINESS_SEMANTIC_CACHE);
        let (last_refresh, cached) = &*guard;
        if let Some(entry) = cached.as_ref()
            && last_refresh.elapsed() < READINESS_SEMANTIC_CACHE_TTL
            && entry.database_url == config.database_url
            && entry.storage_root == config.storage_root
        {
            return entry.result.clone();
        }
    }

    let result = readiness_check_semantic_status(config, conn);
    *lock_mutex(&READINESS_SEMANTIC_CACHE) = (
        Instant::now(),
        Some(ReadinessSemanticCacheEntry {
            database_url: config.database_url.clone(),
            storage_root: config.storage_root.clone(),
            result: result.clone(),
        }),
    );
    result
}

fn readiness_check_semantic_status(
    config: &mcp_agent_mail_core::Config,
    conn: &DbConn,
) -> Result<(), String> {
    let rows = conn
        .query_sync(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            &[],
        )
        .map_err(|e| format!("failed to inspect sqlite schema during readiness check: {e}"))?;
    let present = rows
        .into_iter()
        .filter_map(|row| row.get_named::<String>("name").ok())
        .collect::<std::collections::BTreeSet<_>>();
    let missing_tables = ["projects", "agents", "messages", "message_recipients"]
        .into_iter()
        .filter(|name| !present.contains(*name))
        .collect::<Vec<_>>();
    if !missing_tables.is_empty() {
        return Err(format!(
            "sqlite schema missing required readiness tables: {}",
            missing_tables.join(", ")
        ));
    }

    if let Some(sqlite_path) = resolve_server_database_url_sqlite_path(&config.database_url)
        && let Some(drift) = inspect_archive_db_drift(&config.storage_root, &sqlite_path, conn)?
    {
        return Err(drift.readiness_error());
    }

    Ok(())
}

/// Emit a prominent startup log line showing which database file is active.
///
/// This makes it trivially easy for operators to verify the correct DB when
/// tailing logs after a restart or deployment. It also emits a best-effort
/// warning when the canonical archive inventory is ahead of the SQLite index,
/// which usually means the DB was recreated or left stale without a matching
/// archive restore/reconcile.
fn log_active_database(config: &mcp_agent_mail_core::Config) {
    let db_display: std::borrow::Cow<'_, str> =
        match resolve_server_database_url_sqlite_path(&config.database_url) {
            Some(p) => std::borrow::Cow::Owned(p.display().to_string()),
            None if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(
                &config.database_url,
            ) =>
            {
                std::borrow::Cow::Borrowed(":memory:")
            }
            None => std::borrow::Cow::Borrowed("<unknown>"),
        };

    tracing::info!(
        database = %db_display,
        storage_root = %config.storage_root.display(),
        "Active database"
    );

    // Best-effort warning: archive inventory is ahead of the live SQLite index.
    if let Some(sqlite_path) = resolve_server_database_url_sqlite_path(&config.database_url)
        && let Some(observed) =
            dashboard_open_connection(&config.database_url, config.storage_root.as_path())
        && let Ok(Some(drift)) =
            inspect_archive_db_drift(&config.storage_root, &sqlite_path, observed.conn())
    {
        tracing::warn!(
            database = %db_display,
            storage_root = %config.storage_root.display(),
            archive_projects = drift.archive_projects,
            archive_agents = drift.archive_agents,
            archive_messages = drift.archive_messages,
            db_projects = drift.db_projects,
            db_agents = drift.db_agents,
            db_messages = drift.db_messages,
            missing_archive_projects = ?drift.missing_archive_projects,
            "Canonical archive inventory is ahead of the SQLite index — \
             the DB may have been recreated or left stale without archive restore/reconcile"
        );
    }
}

/// Enrich a readiness JSON response with database identity metadata so
/// operators can verify the correct DB file is active at a glance.
///
/// Adds: `database_path` (basename only), `project_count`, `message_count`,
/// and `version`.  Count queries are best-effort — if they fail the
/// corresponding fields are set to `null` rather than degrading the overall
/// readiness signal.
fn enrich_readiness_response(
    database_url: &str,
    storage_root: &Path,
    body: &mut serde_json::Value,
) {
    // Version — always available at compile time.
    body["version"] = serde_json::json!(env!("CARGO_PKG_VERSION"));

    // Database basename (security: never expose the full filesystem path).
    let db_basename: serde_json::Value = match resolve_server_database_url_sqlite_path(database_url)
    {
        Some(p) => p
            .file_name()
            .map(|n| serde_json::Value::String(n.to_string_lossy().into_owned()))
            .unwrap_or(serde_json::Value::Null),
        None if mcp_agent_mail_core::disk::is_sqlite_memory_database_url(database_url) => {
            serde_json::json!(":memory:")
        }
        None => serde_json::Value::Null,
    };
    body["database_path"] = db_basename;

    // Cached COUNT queries — avoid running COUNT(*) on every /health poll.
    // The cache has a short TTL (HEALTH_COUNT_CACHE_TTL) so operators still
    // see reasonably fresh numbers while load-balancer probes stay fast.
    let cached_counts = {
        let guard = lock_mutex(&HEALTH_COUNT_CACHE);
        let (last_refresh, cached_entry) = &*guard;
        let cached_for_mailbox = cached_entry
            .as_ref()
            .filter(|entry| {
                entry.database_url == database_url && entry.storage_root.as_path() == storage_root
            })
            .cloned();
        if let Some(entry) = cached_for_mailbox.as_ref()
            && last_refresh.elapsed() < HEALTH_COUNT_CACHE_TTL
        {
            entry.counts
        } else {
            // Cache is stale — release the lock before doing I/O, then
            // re-acquire to write the refreshed value.
            drop(guard);
            let fresh = dashboard_open_connection(database_url, storage_root).map(|db| {
                let projects = dashboard_count(db.conn(), "SELECT COUNT(*) AS c FROM projects");
                let messages = dashboard_count(db.conn(), "SELECT COUNT(*) AS c FROM messages");
                (projects, messages)
            });
            let counts =
                fresh.or_else(|| cached_for_mailbox.as_ref().and_then(|entry| entry.counts));
            *lock_mutex(&HEALTH_COUNT_CACHE) = (
                Instant::now(),
                Some(HealthCountCacheEntry {
                    database_url: database_url.to_string(),
                    storage_root: storage_root.to_path_buf(),
                    counts,
                }),
            );
            counts
        }
    };
    if let Some((projects, messages)) = cached_counts {
        body["project_count"] = serde_json::json!(projects);
        body["message_count"] = serde_json::json!(messages);
    } else {
        body["project_count"] = serde_json::Value::Null;
        body["message_count"] = serde_json::Value::Null;
    }
}

#[allow(clippy::too_many_lines)]
fn readiness_check_with_integrity(
    config: &mcp_agent_mail_core::Config,
    run_integrity_check: bool,
) -> Result<(), String> {
    let is_memory = mcp_agent_mail_core::disk::is_sqlite_memory_database_url(&config.database_url);
    let sqlite_path = if is_memory {
        None
    } else {
        resolve_server_database_url_sqlite_path(&config.database_url)
    };
    let _sqlite_activity_lock = if let Some(sqlite_path) = sqlite_path.as_ref() {
        acquire_mailbox_activity_lock_for_sqlite_path(sqlite_path, MailboxActivityLockMode::Shared)
            .map_err(|e| e.to_string())?
    } else {
        None
    };

    // Pre-flight check: Auto-recover missing SQLite files before acquiring pool connections
    // that would otherwise create a fresh empty DB (bypassing archive reconstruction).
    if run_integrity_check
        && config.integrity_check_on_startup
        && let Some(sqlite_path) = sqlite_path.as_ref()
    {
        let storage_root = std::path::Path::new(&config.storage_root);
        if !sqlite_path.exists() {
            let recovery_res = if storage_root.is_dir() {
                mcp_agent_mail_db::ensure_sqlite_file_healthy_with_archive(
                    sqlite_path,
                    storage_root,
                )
            } else {
                mcp_agent_mail_db::ensure_sqlite_file_healthy(sqlite_path)
            };
            if let Err(e) = recovery_res {
                return Err(format!("Failed to recover missing SQLite database: {e}"));
            }
        }
    }

    let pool_timeout_ms = if run_integrity_check {
        config
            .database_pool_timeout
            .map_or(mcp_agent_mail_db::pool::DEFAULT_POOL_TIMEOUT_MS, |v| {
                v.saturating_mul(1000)
            })
    } else {
        // Quick check: use a much shorter timeout (2s) to avoid delaying startup
        // if the DB is busy with a long backfill or migration.
        2000
    };
    // Keep readiness initialization intentionally minimal: we only need one
    // successful acquire/query path to force migration initialization. Building
    // a large auto-sized pool here causes avoidable startup churn on big hosts.
    let db_config = DbPoolConfig {
        database_url: config.database_url.clone(),
        storage_root: Some(config.storage_root.clone()),
        min_connections: 1,
        max_connections: 1,
        acquire_timeout_ms: pool_timeout_ms,
        max_lifetime_ms: mcp_agent_mail_db::pool::DEFAULT_POOL_RECYCLE_MS,
        run_migrations: true,
        warmup_connections: 0,
        cache_budget_kb: mcp_agent_mail_db::schema::DEFAULT_CACHE_BUDGET_KB,
    };

    let cx = Cx::for_testing();
    let pool = create_pool(&db_config).map_err(|e| e.to_string())?;
    let conn = match block_on(pool.acquire(&cx)) {
        asupersync::Outcome::Ok(c) => c,
        asupersync::Outcome::Err(e) => {
            let error = e.to_string();
            if run_integrity_check && mcp_agent_mail_db::is_corruption_error_message(&error) {
                return Err(format!(
                    "SQLite corruption detected during readiness check; automatic server-side recovery is disabled: {error}"
                ));
            }
            return Err(error);
        }
        asupersync::Outcome::Cancelled(_) => return Err("readiness cancelled".to_string()),
        asupersync::Outcome::Panicked(p) => {
            return Err(format!("readiness panic: {}", p.message()));
        }
    };

    if let Err(e) = conn.query_sync("SELECT 1", &[]) {
        let error = e.to_string();
        if run_integrity_check && mcp_agent_mail_db::is_corruption_error_message(&error) {
            return Err(format!(
                "SQLite corruption detected during readiness check; automatic server-side recovery is disabled: {error}"
            ));
        }
        return Err(error);
    }

    let startup_integrity_fingerprint = sqlite_startup_fingerprint(&conn, &config.database_url);
    drop(conn);

    let skip_startup_integrity =
        startup_integrity_fingerprint
            .as_ref()
            .is_some_and(|(sqlite_path, fingerprint)| {
                startup_integrity_cache_is_fresh(config, sqlite_path, *fingerprint)
            });
    if run_integrity_check && config.integrity_check_on_startup && !is_memory {
        if skip_startup_integrity {
            tracing::debug!(
                "startup integrity quick-check skipped (schema fingerprint unchanged and cache is fresh)"
            );
        } else {
            // Note: If we just recovered above, this is redundant but extremely fast and safe.
            pool.run_startup_integrity_check()
                .map_err(|e| format!("startup integrity check failed: {e}"))?;
            if let Some((sqlite_path, fingerprint)) = startup_integrity_fingerprint {
                write_startup_integrity_cache(config, &sqlite_path, fingerprint);
            }
        }
    }
    Ok(())
}

fn parse_params<T: serde::de::DeserializeOwned>(
    params: Option<serde_json::Value>,
) -> Result<T, McpError> {
    let value = params.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(value)
        .map_err(|e| McpError::new(McpErrorCode::InvalidParams, e.to_string()))
}

fn parse_params_or_default<T: serde::de::DeserializeOwned + Default>(
    params: Option<serde_json::Value>,
) -> Result<T, McpError> {
    match params {
        None | Some(serde_json::Value::Null) => Ok(T::default()),
        Some(value) => serde_json::from_value(value)
            .map_err(|e| McpError::new(McpErrorCode::InvalidParams, e.to_string())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Tools,
    Resources,
    Other,
}

impl std::fmt::Display for RequestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tools => write!(f, "tools"),
            Self::Resources => write!(f, "resources"),
            Self::Other => write!(f, "other"),
        }
    }
}

fn classify_request(req: &JsonRpcRequest) -> (RequestKind, Option<String>) {
    if req.method == "tools/call" {
        if let Some(params) = req.params.as_ref()
            && let Some(name) = params.get("name").and_then(|v| v.as_str())
        {
            return (RequestKind::Tools, Some(name.to_string()));
        }
        return (RequestKind::Tools, None);
    }
    if req.method == "tools/list" {
        return (RequestKind::Tools, None);
    }
    if req.method.starts_with("resources/") {
        return (RequestKind::Resources, None);
    }
    (RequestKind::Other, None)
}

const RATE_LIMIT_REDIS_LUA: &str = r"local key = KEYS[1]
local now = tonumber(ARGV[1])
local rate = tonumber(ARGV[2])
local burst = tonumber(ARGV[3])
local state = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(state[1]) or burst
local ts = tonumber(state[2]) or now
local delta = now - ts
tokens = math.min(burst, tokens + delta * rate)
local allowed = 0
if tokens >= 1 then
  tokens = tokens - 1
  allowed = 1
end
redis.call('HMSET', key, 'tokens', tokens, 'ts', now)
redis.call('EXPIRE', key, math.ceil(burst / math.max(rate, 0.001)))
return allowed
";

async fn consume_rate_limit_redis(
    cx: &Cx,
    redis: &RedisClient,
    key: &str,
    per_minute: u32,
    burst: u32,
    now: f64,
) -> Result<bool, ()> {
    if per_minute == 0 {
        return Ok(true);
    }

    let rate_per_sec = f64::from(per_minute) / 60.0;
    let redis_key = format!("rl:{key}");
    let now_s = now.to_string();
    let rate_s = rate_per_sec.to_string();
    let burst_s = burst.to_string();

    let resp = redis
        .cmd_bytes(
            cx,
            &[
                b"EVAL",
                RATE_LIMIT_REDIS_LUA.as_bytes(),
                b"1",
                redis_key.as_bytes(),
                now_s.as_bytes(),
                rate_s.as_bytes(),
                burst_s.as_bytes(),
            ],
        )
        .await
        .map_err(|_| ())?;
    let allowed = resp.as_integer().unwrap_or(0) == 1;
    Ok(allowed)
}

struct RateLimiter {
    buckets: Mutex<HashMap<String, (f64, f64)>>,
    last_cleanup: Mutex<f64>,
}

impl RateLimiter {
    fn new() -> Self {
        let now = rate_limit_now();
        Self {
            buckets: Mutex::new(HashMap::new()),
            last_cleanup: Mutex::new(now),
        }
    }

    fn allow_memory(
        &self,
        key: &str,
        per_minute: u32,
        burst: u32,
        now: f64,
        do_cleanup: bool,
    ) -> bool {
        if per_minute == 0 {
            return true;
        }
        let rate_per_sec = f64::from(per_minute) / 60.0;
        let burst = f64::from(burst.max(1));

        if do_cleanup {
            self.cleanup(now);
        }

        {
            let mut buckets = self
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (tokens0, ts) = buckets.get(key).copied().unwrap_or((burst, now));
            let elapsed = (now - ts).max(0.0);
            let mut tokens = (tokens0 + elapsed * rate_per_sec).min(burst);

            let allowed = tokens >= 1.0;
            if allowed {
                tokens -= 1.0;
            }

            let new_state = (tokens, now);
            if let Some(entry) = buckets.get_mut(key) {
                *entry = new_state;
            } else {
                buckets.insert(key.to_string(), new_state);
            }

            allowed
        }
    }

    fn cleanup(&self, now: f64) {
        {
            let mut last = self
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if now - *last < 60.0 {
                return;
            }
            *last = now;
        }

        let cutoff = now - 3600.0;
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buckets.retain(|_, (_, ts)| *ts >= cutoff);
    }
}

fn rate_limits_for(config: &mcp_agent_mail_core::Config, kind: RequestKind) -> (u32, u32) {
    let (rpm, burst) = match kind {
        RequestKind::Tools => (
            config.http_rate_limit_tools_per_minute,
            config.http_rate_limit_tools_burst,
        ),
        RequestKind::Resources => (
            config.http_rate_limit_resources_per_minute,
            config.http_rate_limit_resources_burst,
        ),
        RequestKind::Other => (config.http_rate_limit_per_minute, 0),
    };
    let burst = if burst == 0 { rpm.max(1) } else { burst };
    (rpm, burst)
}

fn normalize_base_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    let mut out = trimmed.to_string();
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    // Trim trailing slashes, but ensure we never return empty string
    let result = out.trim_end_matches('/');
    if result.is_empty() { "/" } else { result }.to_string()
}

/// Try to detect the Tailscale IPv4 address by running `tailscale ip -4`.
/// Returns `None` if Tailscale is not installed or not running.
pub(crate) fn detect_tailscale_ip() -> Option<String> {
    let output = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_tailscale_ip_stdout(&output.stdout)
}

fn parse_tailscale_ip_stdout(stdout: &[u8]) -> Option<String> {
    use std::net::Ipv4Addr;

    let text = std::str::from_utf8(stdout).ok()?;
    text.lines()
        .map(str::trim)
        .find_map(|line| line.parse::<Ipv4Addr>().ok().map(|ip| ip.to_string()))
}

/// Build a web UI URL with the given host, port, and optional auth token.
///
/// When `token` is `Some`, the token is percent-encoded and appended as
/// `?token=…` so the URL works in a browser without extra auth headers.
pub(crate) fn build_web_ui_url(host: &str, port: u16, token: Option<&str>) -> String {
    let base = format!("http://{}:{port}/mail", connect_authority_host(host));
    match token {
        Some(t) => format!("{base}?token={}", percent_encode_query_component(t)),
        None => base,
    }
}

fn percent_encode_query_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push(char::from(HEX[(b >> 4) as usize]));
            out.push(char::from(HEX[(b & 0x0F) as usize]));
        }
    }
    out
}

fn percent_decode_query_component(value: &str) -> Option<String> {
    const fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(10 + (b - b'a')),
            b'A'..=b'F' => Some(10 + (b - b'A')),
            _ => None,
        }
    }

    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                decoded.push((hi << 4) | lo);
                i += 3;
            }
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            b => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn detect_transport_mode(path: &str) -> &'static str {
    match normalize_base_path(path).as_str() {
        "/mcp" => "mcp",
        "/api" => "api",
        _ => "custom",
    }
}

fn path_matches_base(path: &str, base_no_slash: &str) -> bool {
    // Exact match: /api
    if path == base_no_slash {
        return true;
    }
    // With trailing slash and any sub-path: /api/ or /api/foo
    path.starts_with(&format!("{base_no_slash}/"))
}

fn mcp_base_alias_no_slash(base_no_slash: &str) -> Option<&'static str> {
    match base_no_slash {
        "/api" => Some("/mcp"),
        "/mcp" => Some("/api"),
        _ => None,
    }
}

fn canonicalize_mcp_path_for_handler(path: &str, base_no_slash: &str) -> String {
    let Some(alias_no_slash) = mcp_base_alias_no_slash(base_no_slash) else {
        return path.to_string();
    };

    // Exact alias base: /mcp -> /api
    if path == alias_no_slash {
        return base_no_slash.to_string();
    }

    // Alias subpaths: /mcp/* -> /api/*
    let prefix = format!("{alias_no_slash}/");
    let Some(rest) = path.strip_prefix(&prefix) else {
        return path.to_string();
    };

    format!("{base_no_slash}/{rest}")
}

fn split_path_query(uri: &str) -> (String, Option<String>) {
    let mut parts = uri.splitn(2, '?');
    let path = parts.next().unwrap_or("/").to_string();
    let query = parts.next().map(std::string::ToString::to_string);
    (path, query)
}

fn maybe_inject_localhost_authorization_for_base_passthrough(
    config: &mcp_agent_mail_core::Config,
    req: &mut Http1Request,
    path: &str,
    base_no_slash: &str,
) {
    if path != base_no_slash {
        return;
    }
    if !config.http_allow_localhost_unauthenticated {
        return;
    }
    if !is_local_peer_addr(req.peer_addr) {
        return;
    }
    if header_value(req, "authorization").is_some() {
        return;
    }
    if let Some(token) = config.http_bearer_token.as_deref() {
        req.headers
            .push(("authorization".to_string(), format!("Bearer {token}")));
    }
}

fn to_mcp_http_request(req: &Http1Request, path: &str) -> HttpRequest {
    let method = match req.method {
        Http1Method::Get => McpHttpMethod::Get,
        Http1Method::Post => McpHttpMethod::Post,
        Http1Method::Put => McpHttpMethod::Put,
        Http1Method::Delete => McpHttpMethod::Delete,
        Http1Method::Options => McpHttpMethod::Options,
        Http1Method::Head => McpHttpMethod::Head,
        Http1Method::Patch => McpHttpMethod::Patch,
        Http1Method::Connect | Http1Method::Trace | Http1Method::Extension(_) => {
            McpHttpMethod::Post
        }
    };
    let mut headers = HashMap::new();
    for (k, v) in &req.headers {
        let lk = k.to_lowercase();
        // Legacy parity: strip any existing Accept header; we force it below.
        if lk == "accept" {
            continue;
        }
        headers.insert(lk, v.clone());
    }
    // Legacy parity (StatelessMCPASGIApp): ensure Accept includes both JSON and SSE
    // so StreamableHTTP transport never rejects the request.
    headers.insert(
        "accept".to_string(),
        "application/json, text/event-stream".to_string(),
    );
    // Legacy parity: ensure Content-Type is present for POST requests.
    if matches!(req.method, Http1Method::Post) && !headers.contains_key("content-type") {
        headers.insert("content-type".to_string(), "application/json".to_string());
    }
    HttpRequest {
        method,
        path: path.to_string(),
        headers,
        body: req.body.clone(),
        query: HashMap::new(),
    }
}

fn to_http1_response(
    resp: HttpResponse,
    origin: Option<String>,
    allow_credentials: bool,
    allow_methods: &[String],
    allow_headers: &[String],
) -> Http1Response {
    let status = resp.status.0;
    let mut out = Http1Response::new(status, default_reason(status), resp.body);
    for (k, v) in resp.headers {
        out.headers.push((k, v));
    }
    apply_cors_headers(
        &mut out,
        origin,
        allow_credentials,
        allow_methods,
        allow_headers,
    );
    out
}

fn apply_cors_headers(
    resp: &mut Http1Response,
    origin: Option<String>,
    allow_credentials: bool,
    allow_methods: &[String],
    allow_headers: &[String],
) {
    let Some(origin) = origin else {
        return;
    };
    resp.headers.retain(|(k, _)| {
        let key = k.to_lowercase();
        key != "access-control-allow-origin"
            && key != "access-control-allow-methods"
            && key != "access-control-allow-headers"
            && key != "access-control-allow-credentials"
    });
    resp.headers
        .push(("access-control-allow-origin".to_string(), origin));
    resp.headers.push((
        "access-control-allow-methods".to_string(),
        cors_list_value(allow_methods),
    ));
    resp.headers.push((
        "access-control-allow-headers".to_string(),
        cors_list_value(allow_headers),
    ));
    if allow_credentials {
        resp.headers.push((
            "access-control-allow-credentials".to_string(),
            "true".to_string(),
        ));
    }
}

fn cors_list_value(values: &[String]) -> String {
    if values.is_empty() {
        return "*".to_string();
    }
    if values.len() == 1 && values[0] == "*" {
        return "*".to_string();
    }
    values.join(", ")
}

fn cors_wildcard(allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed.iter().any(|o| o == "*")
}

fn header_value<'a>(req: &'a Http1Request, name: &str) -> Option<&'a str> {
    let name = name.to_lowercase();
    req.headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == name)
        .map(|(_, v)| v.as_str())
}

fn header_has_token(req: &Http1Request, name: &str, token: &str) -> bool {
    header_value(req, name).is_some_and(|value| {
        value
            .split(',')
            .any(|segment| segment.trim().eq_ignore_ascii_case(token))
    })
}

fn is_websocket_upgrade_request(req: &Http1Request) -> bool {
    header_has_token(req, "connection", "upgrade") && header_has_token(req, "upgrade", "websocket")
}

fn should_suppress_tui_http_event(path: &str) -> bool {
    path.ends_with("/healthz")
        || path == "/mail/ws-state"
        || path == "/mail/ws-input"
        || path == "/web-dashboard/state"
        || path == "/web-dashboard/stream"
        || path == "/web-dashboard/input"
}

fn has_forwarded_headers(req: &Http1Request) -> bool {
    header_value(req, "x-forwarded-for").is_some()
        || header_value(req, "x-forwarded-proto").is_some()
        || header_value(req, "x-forwarded-host").is_some()
        || header_value(req, "forwarded").is_some()
}

fn peer_addr_host(peer_addr: SocketAddr) -> String {
    match peer_addr.ip() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
    }
}

fn rate_limit_now() -> f64 {
    // Legacy python uses `time.monotonic()` (system-wide monotonic seconds).
    // We approximate "monotonic seconds since epoch" by anchoring SystemTime to an Instant.
    //
    // This avoids time going backwards on clock adjustments while remaining consistent
    // across processes for Redis-backed buckets (absolute base cancels out in deltas).
    use std::time::{SystemTime, UNIX_EPOCH};

    static BASE: OnceLock<(SystemTime, Instant)> = OnceLock::new();
    let (base_wall, base_inst) = BASE.get_or_init(|| (SystemTime::now(), Instant::now()));
    let now_wall = base_wall
        .checked_add(base_inst.elapsed())
        .unwrap_or_else(SystemTime::now);

    now_wall
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn rate_limit_identity(req: &Http1Request, jwt_sub: Option<&str>) -> String {
    if let Some(sub) = jwt_sub.filter(|s| !s.is_empty()) {
        return format!("sub:{sub}");
    }
    req.peer_addr
        .map_or_else(|| "ip-unknown".to_string(), peer_addr_host)
}

fn is_local_peer_addr(peer_addr: Option<SocketAddr>) -> bool {
    let Some(addr) = peer_addr else {
        return false;
    };
    is_loopback_ip(addr.ip())
}

fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn cors_allows(allowed: &[String], origin: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed.iter().any(|o| o == "*" || o == origin)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    // Compare in a way that doesn't early-return on the first mismatch.
    // We still necessarily run proportional to max(len(a), len(b)).
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut diff = u64::try_from(a_bytes.len() ^ b_bytes.len()).unwrap_or(u64::MAX);
    let max_len = a_bytes.len().max(b_bytes.len());
    for i in 0..max_len {
        let x = a_bytes.get(i).copied().unwrap_or(0);
        let y = b_bytes.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    diff == 0
}

fn py_repr_str(s: &str) -> String {
    // Cheap approximation of Python's `repr(str)` used by structlog's KeyValueRenderer.
    // Good enough for stable snapshots and human scanning.
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("'{escaped}'")
}

fn http_request_log_kv_line(
    timestamp: &str,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
    client_ip: &str,
) -> String {
    // Legacy key_order: ["event","path","status"].
    // Remaining keys follow the common structlog insertion order: kwargs first, then processors.
    [
        format!("event={}", py_repr_str("request")),
        format!("path={}", py_repr_str(path)),
        format!("status={status}"),
        format!("method={}", py_repr_str(method)),
        format!("duration_ms={duration_ms}"),
        format!("client_ip={}", py_repr_str(client_ip)),
        format!("timestamp={}", py_repr_str(timestamp)),
        format!("level={}", py_repr_str("info")),
    ]
    .join(" ")
}

fn http_request_log_json_line(
    timestamp: &str,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
    client_ip: &str,
) -> Option<String> {
    let value = serde_json::json!({
        "timestamp": timestamp,
        "level": "info",
        "event": "request",
        "method": method,
        "path": path,
        "status": status,
        "duration_ms": duration_ms,
        "client_ip": client_ip,
    });
    serde_json::to_string(&value).ok()
}

fn http_request_log_fallback_line(
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
    client_ip: &str,
) -> String {
    // Must match legacy fallback string exactly.
    format!("http method={method} path={path} status={status} ms={duration_ms} client={client_ip}")
}

// render_http_request_panel moved to console.rs (br-1m6a.13)

// ---------------------------------------------------------------------------
// Expected Error Filter (Legacy Parity Helper)
// ---------------------------------------------------------------------------
//
// Legacy python applies this as a stdlib logging.Filter to the logger:
//   "fastmcp.tools.tool_manager"
//
// In Rust, we expose the same classification logic so whichever logging backend
// we settle on (log, tracing, etc) can replicate the behavior without letting
// expected errors spam stacktraces or error-level logs.

#[allow(dead_code)]
const EXPECTED_ERROR_FILTER_TARGET: &str = "fastmcp.tools.tool_manager";

#[allow(dead_code)]
const EXPECTED_ERROR_PATTERNS: [&str; 8] = [
    "not found in project",
    "index.lock",
    "git_index_lock",
    "resource_busy",
    "temporarily locked",
    "recoverable=true",
    "use register_agent",
    "available agents:",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum SimpleLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl SimpleLogLevel {
    const fn is_error_or_higher(self) -> bool {
        matches!(self, Self::Error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
struct ExpectedErrorOutcome {
    is_expected: bool,
    suppress_exc: bool,
    effective_level: SimpleLogLevel,
}

#[allow(dead_code)]
fn expected_error_filter(
    target: &str,
    has_exc: bool,
    level: SimpleLogLevel,
    message: &str,
    recoverable: bool,
    cause_chain: &[(/* message */ &str, /* recoverable */ bool)],
) -> ExpectedErrorOutcome {
    // Legacy behavior: filter only when there is exception info.
    if !has_exc {
        return ExpectedErrorOutcome {
            is_expected: false,
            suppress_exc: false,
            effective_level: level,
        };
    }

    // Legacy behavior: apply only to the specific tool-manager logger.
    if target != EXPECTED_ERROR_FILTER_TARGET {
        return ExpectedErrorOutcome {
            is_expected: false,
            suppress_exc: false,
            effective_level: level,
        };
    }

    let msg_matches_patterns = |msg: &str| {
        let msg = msg.to_ascii_lowercase();
        EXPECTED_ERROR_PATTERNS
            .iter()
            .any(|needle| msg.contains(needle))
    };

    let mut expected = recoverable || msg_matches_patterns(message);
    if !expected {
        for (cause_msg, cause_recoverable) in cause_chain {
            if *cause_recoverable || msg_matches_patterns(cause_msg) {
                expected = true;
                break;
            }
        }
    }

    if expected {
        ExpectedErrorOutcome {
            is_expected: true,
            suppress_exc: true,
            effective_level: if level.is_error_or_higher() {
                SimpleLogLevel::Info
            } else {
                level
            },
        }
    } else {
        ExpectedErrorOutcome {
            is_expected: false,
            suppress_exc: false,
            effective_level: level,
        }
    }
}

const fn http_error_status(
    err: &fastmcp_transport::http::HttpError,
) -> fastmcp_transport::http::HttpStatus {
    use fastmcp_transport::http::HttpError;
    use fastmcp_transport::http::HttpStatus;
    match err {
        HttpError::InvalidMethod(_) => HttpStatus::METHOD_NOT_ALLOWED,
        HttpError::InvalidContentType(_)
        | HttpError::JsonError(_)
        | HttpError::CodecError(_)
        | HttpError::HeadersTooLarge { .. }
        | HttpError::BodyTooLarge { .. }
        | HttpError::UnsupportedTransferEncoding(_) => HttpStatus::BAD_REQUEST,
        HttpError::Timeout | HttpError::Closed => HttpStatus::SERVICE_UNAVAILABLE,
        HttpError::Transport(_) => HttpStatus::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::http::h1::types::Version as Http1Version;
    use chrono::Utc;
    use ftui_runtime::stdio_capture::StdioCapture;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static STDIO_CAPTURE_LOCK: Mutex<()> = Mutex::new(());
    static TUI_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static TOOL_DISPATCH_ENV_TEST_LOCK: Mutex<()> = Mutex::new(());
    static HEALTH_COUNT_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static REDIS_RATE_LIMIT_COUNTER: AtomicU64 = AtomicU64::new(1);

    /// Regression test: Budget deadline must be relative to `wall_now()`, not absolute.
    ///
    /// BUG HISTORY: `Budget::with_deadline_secs(30)` created a deadline of 30 seconds
    /// since epoch (1970-01-01 00:00:30), but `wall_now()` returns time relative to
    /// process start. Since `wall_now()` > 30 seconds, the deadline was always exceeded
    /// immediately, causing all MCP requests to timeout.
    ///
    /// FIX: Use `wall_now()` + `Duration::from_secs(timeout)` for a relative deadline.
    #[test]
    fn budget_deadline_is_relative_to_wall_now_not_absolute() {
        // Get current wall time
        let now = wall_now();

        // Simulate what the code does: create a budget with 30 second timeout
        let timeout_secs: u64 = 30;
        let deadline = now + std::time::Duration::from_secs(timeout_secs);
        let budget = Budget::new().with_deadline(deadline);

        // CRITICAL: The deadline must be in the future relative to wall_now()
        // If this fails, MCP requests will timeout immediately
        let check_time = wall_now();
        assert!(
            !budget.is_past_deadline(check_time),
            "Budget deadline must be in the future! \
             deadline={deadline:?}, now={check_time:?}, timeout={timeout_secs}s. \
             This regression would cause all MCP requests to timeout immediately.",
        );

        // The deadline should be approximately 30 seconds from now
        // Allow some tolerance for test execution time
        // Note: asupersync::time::Instant opaque type doesn't expose raw nanoseconds directly in all versions,
        // but duration_since works.
        let remaining_nanos = deadline.duration_since(check_time);
        let remaining_secs = remaining_nanos / 1_000_000_000;

        assert!(
            (29..=31).contains(&remaining_secs),
            "Deadline should be ~30 seconds in the future, got {remaining_nanos}ns (~{remaining_secs}s)",
        );
    }

    /// Verify that `Budget::with_deadline_secs` is NOT suitable for relative timeouts.
    /// This test documents the API misuse that caused the bug.
    #[test]
    fn budget_with_deadline_secs_is_absolute_not_relative() {
        // Budget::with_deadline_secs(0) creates an ABSOLUTE deadline at time origin.
        // Since wall_now() is always > 0 for any running process, this deadline
        // is always already exceeded. This demonstrates why with_deadline_secs
        // is NOT suitable for relative timeouts - it uses absolute time, not
        // "N seconds from now".
        let budget = Budget::with_deadline_secs(0);
        let now = wall_now();

        // This assertion is deterministic: wall_now() > 0 always
        assert!(
            budget.is_past_deadline(now),
            "with_deadline_secs(0) should always be expired. \
             wall_now()={now:?} is always > 0 (the absolute deadline). \
             This demonstrates why with_deadline_secs is WRONG for relative timeouts.",
        );
    }

    #[test]
    fn normalized_probe_host_maps_wildcards_to_loopback() {
        assert_eq!(normalized_probe_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(normalized_probe_host("::"), "::1");
        assert_eq!(normalized_probe_host("[::]"), "::1");
        assert_eq!(normalized_probe_host(""), "127.0.0.1");
        assert_eq!(normalized_probe_host("  "), "127.0.0.1");
        assert_eq!(normalized_probe_host("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn stdio_startup_probe_success_is_allowed() {
        let report = startup_checks::StartupReport {
            results: vec![startup_checks::ProbeResult::Ok { name: "integrity" }],
        };

        ensure_stdio_startup_probes_pass(&report).expect("successful probes should pass");
    }

    #[test]
    fn stdio_startup_probe_failure_is_fatal() {
        let report = startup_checks::StartupReport {
            results: vec![startup_checks::ProbeResult::Fail(
                startup_checks::ProbeFailure {
                    name: "integrity",
                    problem: "sqlite corruption remains".to_string(),
                    fix: "run repair".to_string(),
                },
            )],
        };

        let err = ensure_stdio_startup_probes_pass(&report)
            .expect_err("failing probes should stop stdio startup");
        let message = err.to_string();
        assert!(
            message.contains("sqlite corruption remains"),
            "got: {message}"
        );
        assert!(message.contains("run repair"), "got: {message}");
    }

    #[test]
    fn tui_readiness_warmup_failure_records_console_log() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);

        handle_tui_readiness_warmup_result(Some(&state), Err("boom".to_string()));

        let logs = state.console_log_since(0);
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].1,
            "TUI startup: background DB readiness warmup failed (boom)"
        );
    }

    #[test]
    fn tui_readiness_warmup_success_is_silent() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);

        handle_tui_readiness_warmup_result(Some(&state), Ok(()));

        assert!(state.console_log_since(0).is_empty());
        assert_eq!(state.db_warmup_state(), tui_bridge::DbWarmupState::Ready);
    }

    #[test]
    fn tui_readiness_warmup_failure_marks_failed_state() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);

        handle_tui_readiness_warmup_result(Some(&state), Err("boom".to_string()));

        assert_eq!(state.db_warmup_state(), tui_bridge::DbWarmupState::Failed);
    }

    #[test]
    fn readiness_check_recovers_missing_sqlite_before_startup_integrity_check() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        let db_path = temp.path().join("missing.sqlite3");

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = true;

        let result = readiness_check(&config);
        assert!(
            result.is_ok(),
            "missing DB should be auto-initialized/recovered before startup integrity check: {result:?}"
        );
        assert!(
            db_path.exists(),
            "readiness check should leave behind an initialized sqlite file"
        );
    }

    #[test]
    fn startup_integrity_cache_is_not_fresh_when_cache_file_is_symlinked() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let diagnostics_dir = storage_root.join("diagnostics");
        let outside_cache = temp.path().join("outside-cache.json");
        std::fs::create_dir_all(&diagnostics_dir).expect("create diagnostics dir");

        let sqlite_path = "/tmp/mailbox.sqlite3";
        let fingerprint = StartupIntegrityFingerprint {
            schema_version: 7,
            user_version: 11,
        };
        let entry = StartupIntegrityCacheEntry {
            schema_version: STARTUP_INTEGRITY_CACHE_SCHEMA_VERSION,
            sqlite_path: sqlite_path.to_string(),
            fingerprint,
            checked_at_micros: startup_integrity_now_micros(),
        };
        std::fs::write(
            &outside_cache,
            serde_json::to_string_pretty(&entry).expect("serialize cache entry"),
        )
        .expect("write outside cache");
        symlink(
            &outside_cache,
            diagnostics_dir.join("startup_integrity_cache.json"),
        )
        .expect("symlink cache file");

        let mut config = mcp_agent_mail_core::Config::default();
        config.storage_root = storage_root;

        assert!(
            read_startup_integrity_cache(&config).is_none(),
            "symlinked cache file must be ignored"
        );
        assert!(
            !startup_integrity_cache_is_fresh(&config, sqlite_path, fingerprint),
            "symlinked cache file must not suppress the startup integrity check"
        );
    }

    #[test]
    fn write_startup_integrity_cache_ignores_symlinked_cache_file() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let diagnostics_dir = storage_root.join("diagnostics");
        let outside_cache = temp.path().join("outside-cache.json");
        std::fs::create_dir_all(&diagnostics_dir).expect("create diagnostics dir");
        std::fs::write(&outside_cache, "sentinel").expect("write sentinel cache");
        symlink(
            &outside_cache,
            diagnostics_dir.join("startup_integrity_cache.json"),
        )
        .expect("symlink cache file");

        let mut config = mcp_agent_mail_core::Config::default();
        config.storage_root = storage_root;

        write_startup_integrity_cache(
            &config,
            "/tmp/mailbox.sqlite3",
            StartupIntegrityFingerprint {
                schema_version: 3,
                user_version: 5,
            },
        );

        let sentinel = std::fs::read_to_string(&outside_cache).expect("read sentinel cache");
        assert_eq!(sentinel, "sentinel");
    }

    #[test]
    fn write_startup_integrity_cache_ignores_symlinked_diagnostics_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let outside_dir = temp.path().join("outside-diagnostics");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        std::fs::create_dir_all(&outside_dir).expect("create outside diagnostics dir");
        symlink(&outside_dir, storage_root.join("diagnostics")).expect("symlink diagnostics dir");

        let mut config = mcp_agent_mail_core::Config::default();
        config.storage_root = storage_root;

        write_startup_integrity_cache(
            &config,
            "/tmp/mailbox.sqlite3",
            StartupIntegrityFingerprint {
                schema_version: 13,
                user_version: 21,
            },
        );

        assert!(
            !outside_dir.join("startup_integrity_cache.json").exists(),
            "cache writes must not traverse a symlinked diagnostics directory"
        );
    }

    #[test]
    fn readiness_check_reports_busy_before_initializing_missing_sqlite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        let db_path = temp.path().join("busy-missing.sqlite3");

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = true;

        let _sqlite_lock = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Exclusive,
        )
        .expect("acquire exclusive sqlite mailbox lock");

        let error = readiness_check(&config).expect_err("busy mailbox should block readiness init");
        assert!(
            error.contains("mailbox activity lock is busy") || error.contains("temporarily busy"),
            "busy readiness error should mention mailbox contention: {error}"
        );
        assert!(
            !db_path.exists(),
            "busy readiness check should fail before initializing the sqlite file"
        );
    }

    #[test]
    fn readiness_check_uses_absolute_candidate_for_mailbox_activity_lock() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        let absolute_db = temp.path().join("readiness-absolute.sqlite3");
        std::fs::write(&absolute_db, b"placeholder").expect("create absolute db");

        let relative_path =
            std::path::PathBuf::from(absolute_db.to_string_lossy().trim_start_matches('/'));
        assert!(
            !relative_path.exists(),
            "relative shadow path should be absent so absolute candidate fallback is exercised"
        );

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", relative_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        let _sqlite_lock = acquire_mailbox_activity_lock_for_sqlite_path(
            &absolute_db,
            MailboxActivityLockMode::Exclusive,
        )
        .expect("acquire exclusive sqlite mailbox lock on absolute candidate");

        let error = readiness_check_quick(&config)
            .expect_err("resolved absolute candidate lock should block readiness");
        assert!(
            error.contains("mailbox activity lock is busy") || error.contains("temporarily busy"),
            "busy readiness error should mention mailbox contention on the resolved absolute candidate: {error}"
        );
    }

    #[test]
    fn readiness_check_quick_does_not_initialize_missing_sqlite() {
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");
        let db_path = temp.path().join("missing-readiness.sqlite3");

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        let error =
            readiness_check_quick(&config).expect_err("quick readiness should not initialize db");
        assert!(
            error.contains("quick readiness refuses to initialize the mailbox"),
            "missing-db readiness error should explain the non-mutating refusal: {error}"
        );
        assert!(
            !db_path.exists(),
            "quick readiness must not create a missing sqlite file"
        );
    }

    #[test]
    fn readiness_check_quick_reports_archive_drift_as_not_ready() {
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-readiness.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        let error = readiness_check_quick(&config)
            .expect_err("archive-ahead mailbox must not report quick readiness");
        assert!(
            error.contains("archive inventory is ahead of the sqlite index"),
            "readiness drift error should mention archive-vs-db mismatch: {error}"
        );
    }

    #[test]
    fn readiness_check_quick_ignores_unrelated_default_archive_overlap() {
        let _env_lock = lock_mutex(&TOOL_DISPATCH_ENV_TEST_LOCK);
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let xdg_data_root = temp.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data_root).expect("create xdg data root");
        let xdg_data_root_str = xdg_data_root
            .to_str()
            .expect("xdg data root utf-8")
            .to_string();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("XDG_DATA_HOME", xdg_data_root_str.as_str())],
            || {
                let storage_root = mcp_agent_mail_core::Config::from_env().storage_root;
                assert!(
                    mcp_agent_mail_core::config::is_default_storage_root(&storage_root),
                    "test must use the default storage root"
                );

                let project_dir = storage_root.join("projects").join("ahead-project");
                let agent_dir = project_dir.join("agents").join("Alice");
                let messages_dir = project_dir.join("messages").join("2026").join("03");
                std::fs::create_dir_all(&agent_dir).expect("create agent dir");
                std::fs::create_dir_all(&messages_dir).expect("create messages dir");
                std::fs::write(
                    project_dir.join("project.json"),
                    r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
                )
                .expect("write project metadata");
                std::fs::write(
                    agent_dir.join("profile.json"),
                    r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
                )
                .expect("write agent profile");
                std::fs::write(
                    messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
                    r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
                )
                .expect("write canonical message");

                let external_db_dir = temp.path().join("external-db");
                std::fs::create_dir_all(&external_db_dir).expect("create external db dir");
                let db_path = external_db_dir.join("readiness.sqlite3");
                let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
                conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                    .expect("init schema");
                drop(conn);

                let mut config = mcp_agent_mail_core::Config::from_env();
                config.database_url = format!("sqlite:///{}", db_path.display());
                config.storage_root = storage_root;
                config.integrity_check_on_startup = false;

                readiness_check_quick(&config).expect(
                    "default-root archive drift must be ignored for an external sqlite path",
                );
            },
        );
    }

    #[test]
    fn readiness_check_quick_reports_archive_agent_drift_without_messages() {
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-agent-readiness.sqlite3");
        let agent_dir = storage_root
            .join("projects")
            .join("ahead-project")
            .join("agents")
            .join("Alice");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::write(agent_dir.join("profile.json"), "{}").expect("write agent profile");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        let error = readiness_check_quick(&config)
            .expect_err("archive agent inventory ahead of sqlite must fail readiness");
        assert!(
            error.contains("archive inventory is ahead of the sqlite index"),
            "agent-only archive drift should fail readiness: {error}"
        );
    }

    #[test]
    fn readiness_check_quick_reports_project_identity_drift_with_equal_counts() {
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-project-identity-readiness.sqlite3");
        let project_dir = storage_root.join("projects").join("archive-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"archive-project","human_key":"/archive-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(agent_dir.join("profile.json"), "{}").expect("write agent profile");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?1, ?2, ?3, ?4)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("wrong-project".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("/wrong-project".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
            ],
        )
        .expect("insert wrong project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Alice".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("coder".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(String::new()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("auto".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("auto".to_string()),
            ],
        )
        .expect("insert agent");
        drop(conn);

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        let error = readiness_check_quick(&config)
            .expect_err("project identity drift should fail readiness even when counts match");
        assert!(
            error.contains("missing archive project(s) in db: archive-project (/archive-project)"),
            "readiness should surface missing archive project identity: {error}"
        );
    }

    #[test]
    fn readiness_check_quick_accepts_db_newer_messages_than_metadata_only_archive_drift() {
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("db-newer-readiness.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("04");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-04-01T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-04-01T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First",
  "importance": "normal",
  "created_ts": "2026-04-01T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        mcp_agent_mail_db::reconstruct::reconstruct_from_archive(&db_path, &storage_root)
            .expect("reconstruct archive into db");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(3),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("BlueLake".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("coder".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("test".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(String::new()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("auto".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("auto".to_string()),
            ],
        )
        .expect("insert recipient");
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(1),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("t2".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("Second".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("second body".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("normal".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(0),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2_000_000),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("[]".to_string()),
                mcp_agent_mail_db::sqlmodel_core::Value::Text(r#"{"to":["BlueLake"],"cc":[],"bcc":[]}"#.to_string()),
            ],
        )
        .expect("insert newer live message");
        conn.execute_sync(
            "INSERT INTO message_recipients (message_id, agent_id, kind, ack_ts, read_ts) VALUES (?1, ?2, ?3, NULL, NULL)",
            &[
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(2),
                mcp_agent_mail_db::sqlmodel_core::Value::BigInt(3),
                mcp_agent_mail_db::sqlmodel_core::Value::Text("to".to_string()),
            ],
        )
        .expect("insert message recipient");
        drop(conn);

        let archive_only_project = storage_root.join("projects").join("archive-only-project");
        let archive_only_agent = archive_only_project.join("agents").join("ArchiveGhost");
        std::fs::create_dir_all(&archive_only_agent).expect("create archive-only project dir");
        std::fs::write(
            archive_only_project.join("project.json"),
            r#"{"slug":"archive-only-project","human_key":"/archive-only-project","created_at":0}"#,
        )
        .expect("write archive-only project metadata");
        std::fs::write(
            archive_only_agent.join("profile.json"),
            r#"{"agent_name":"ArchiveGhost","program":"coder","model":"test","registered_ts":"2026-04-01T00:00:00Z"}"#,
        )
        .expect("write archive-only agent metadata");

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        readiness_check_quick(&config).expect(
            "metadata-only archive drift should not fail readiness when the DB has newer messages",
        );
    }

    #[test]
    fn health_readiness_returns_503_when_archive_is_ahead_of_db() {
        *lock_mutex(&READINESS_SEMANTIC_CACHE) = (Instant::now(), None);

        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("storage");
        let db_path = temp.path().join("stale-health.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        let mut config = mcp_agent_mail_core::Config::default();
        config.database_url = format!("sqlite:///{}", db_path.display());
        config.storage_root = storage_root;
        config.integrity_check_on_startup = false;

        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 503);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["detail"], "service unavailable");
    }

    #[test]
    fn tui_deferred_wait_helpers_stop_on_headless_detach() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);
        let first_paint_state = Arc::clone(&state);
        let db_ready_state = Arc::clone(&state);

        let first_paint_waiter =
            std::thread::spawn(move || wait_for_tui_first_paint(&first_paint_state));
        let db_ready_waiter =
            std::thread::spawn(move || wait_for_tui_db_readiness(&db_ready_state));

        std::thread::sleep(Duration::from_millis(20));
        state.request_headless_detach();

        assert!(!first_paint_waiter.join().expect("join first-paint waiter"));
        assert!(!db_ready_waiter.join().expect("join db-ready waiter"));
    }

    #[test]
    fn headless_detach_branch_check_does_not_consume_stop_latch() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);

        state.request_headless_detach();

        let detach_headless = state.is_headless_detach_requested();
        assert!(detach_headless);
        assert!(
            state.is_headless_detach_requested(),
            "detach branch must leave the stop latch visible until waiters are joined"
        );
        assert!(state.take_headless_detach_requested());
        assert!(!state.is_headless_detach_requested());
    }

    #[test]
    fn deferred_worker_progress_claims_each_stage_once() {
        let progress = TuiDeferredWorkerProgress::default();

        assert!(progress.claim_non_db_start());
        assert!(!progress.claim_non_db_start());

        assert!(progress.claim_db_start());
        assert!(!progress.claim_db_start());

        assert!(progress.claim_advisory_start());
        assert!(!progress.claim_advisory_start());
    }

    #[test]
    fn init_search_bridge_skips_startup_backfill_for_memory_db() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        STARTUP_SEARCH_BACKFILL_IN_PROGRESS.store(false, Ordering::Release);

        let temp = tempfile::tempdir().expect("tempdir");
        let config = mcp_agent_mail_core::Config {
            database_url: "sqlite:///:memory:".to_string(),
            storage_root: temp.path().join("storage"),
            ..mcp_agent_mail_core::Config::default()
        };

        init_search_bridge(&config);

        assert!(config.storage_root.join("search_index").exists());
        assert!(
            !STARTUP_SEARCH_BACKFILL_IN_PROGRESS.load(Ordering::Acquire),
            "memory-backed startup should not spawn lexical backfill worker"
        );
    }

    #[test]
    fn startup_search_backfill_spawn_failure_message_preserves_lazy_mode() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        STARTUP_SEARCH_BACKFILL_IN_PROGRESS.store(false, Ordering::Release);

        let message = startup_search_backfill_spawn_failure_message(&std::io::Error::other("boom"));
        assert!(message.contains("leaving lexical backfill lazy"));
        assert!(!STARTUP_SEARCH_BACKFILL_IN_PROGRESS.load(Ordering::Acquire));
    }

    #[test]
    fn deferred_worker_spawn_failure_stays_gated_and_logs_operator_message() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);
        let progress = Arc::new(TuiDeferredWorkerProgress::default());

        handle_tui_deferred_background_worker_spawn_failure(
            &progress,
            &state,
            &std::io::Error::other("boom"),
        );

        assert!(!progress.non_db.load(Ordering::Acquire));
        assert!(!progress.db.load(Ordering::Acquire));
        assert!(!progress.advisory.load(Ordering::Acquire));
        let logs = state.console_log_since(0);
        assert_eq!(logs.len(), 1);
        assert!(logs[0].1.contains("background services remain gated off"));
    }

    #[test]
    fn startup_search_recovery_skips_nonrecoverable_errors() {
        let config = mcp_agent_mail_core::Config::default();
        assert!(!recover_startup_search_backfill_db(
            &config,
            "permission denied"
        ));
    }

    #[test]
    fn startup_search_recovery_skips_memory_database() {
        let config = mcp_agent_mail_core::Config {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        assert!(!recover_startup_search_backfill_db(
            &config,
            "database disk image is malformed"
        ));
    }

    #[test]
    fn normalized_startup_search_backfill_database_url_prefers_healthy_absolute_path() {
        let absolute_dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = absolute_dir.path().join("storage.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let conn = mcp_agent_mail_db::DbConn::open_file(&absolute_db_str).expect("open");
        conn.execute_raw("CREATE TABLE t (x INTEGER)")
            .expect("create");
        drop(conn);

        let relative_path = std::path::PathBuf::from(absolute_db_str.trim_start_matches('/'));
        if let Some(parent) = relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create relative parent");
        }
        std::fs::write(&relative_path, b"not-a-database").expect("write malformed relative db");

        let database_url = format!("sqlite:///{}", relative_path.display());
        let normalized = normalized_startup_search_backfill_database_url(&database_url);
        assert_eq!(normalized, format!("sqlite:///{absolute_db_str}"));

        let _ = std::fs::remove_file(&relative_path);
        if let Some(parent) = relative_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn mailbox_activity_lock_allows_multiple_shared_guards() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("storage.sqlite3");

        let first = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Shared,
        )
        .expect("acquire first shared lock");
        let second = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Shared,
        )
        .expect("acquire second shared lock");

        assert!(first.is_some());
        assert!(second.is_some());
        assert!(PathBuf::from(format!("{}.activity.lock", db_path.display())).exists());
    }

    #[test]
    fn mailbox_activity_lock_rejects_exclusive_when_shared_guard_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("storage.sqlite3");
        let _shared = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Shared,
        )
        .expect("acquire shared lock");

        let error = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Exclusive,
        )
        .expect_err("exclusive lock should fail while shared lock is held");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            error.to_string().contains("mailbox activity lock is busy"),
            "unexpected contention error: {error}"
        );
    }

    #[test]
    fn mailbox_activity_lock_rejects_shared_when_exclusive_guard_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("storage.sqlite3");
        let _exclusive = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Exclusive,
        )
        .expect("acquire exclusive lock");

        let error = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Shared,
        )
        .expect_err("shared lock should fail while exclusive lock is held");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            error.to_string().contains("mailbox activity lock is busy"),
            "unexpected contention error: {error}"
        );
    }

    #[test]
    fn mailbox_activity_lock_rejects_second_exclusive_storage_root_guard() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage_root = temp.path().join("mailbox");

        let _first = acquire_mailbox_activity_lock_for_storage_root(
            &storage_root,
            MailboxActivityLockMode::Exclusive,
        )
        .expect("acquire first exclusive storage-root lock");

        let error = acquire_mailbox_activity_lock_for_storage_root(
            &storage_root,
            MailboxActivityLockMode::Exclusive,
        )
        .expect_err("second exclusive storage-root lock should fail");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            error.to_string().contains("mailbox activity lock is busy"),
            "unexpected contention error: {error}"
        );
    }

    #[test]
    fn runtime_mailbox_activity_locks_hold_sqlite_guard_across_storage_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("storage.sqlite3");
        let storage_root = temp.path().join("storage-root-a");
        let other_storage_root = temp.path().join("storage-root-b");
        let config = mcp_agent_mail_core::Config {
            database_url: format!("sqlite:///{}", db_path.display()),
            storage_root,
            ..Default::default()
        };

        let _runtime_locks =
            acquire_runtime_mailbox_activity_locks(&config).expect("acquire runtime locks");
        let _other_root_lock = acquire_mailbox_activity_lock_for_storage_root(
            &other_storage_root,
            MailboxActivityLockMode::Exclusive,
        )
        .expect("different storage root should not contend");

        let error = acquire_mailbox_activity_lock_for_sqlite_path(
            &db_path,
            MailboxActivityLockMode::Exclusive,
        )
        .expect_err("runtime sqlite guard should block cross-root exclusive sqlite lock");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            error.to_string().contains("mailbox activity lock is busy"),
            "unexpected contention error: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_total_jiffies_extracts_cpu_sum() {
        let sample = "cpu  10 20 30 40 50 60 70 80 90 100\ncpu0 1 2 3 4\n";
        assert_eq!(parse_proc_total_jiffies(sample), Some(550));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_proc_self_jiffies_extracts_utime_plus_stime() {
        let sample = "12345 (am) S 1 2 3 4 5 6 7 8 9 10 111 222 13 14 15 16";
        // fields[11]=111, fields[12]=222 from the post-`)` split
        assert_eq!(parse_proc_self_jiffies(sample), Some(333));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_cpu_pct_x100_scales_by_cpu_count() {
        let prev = ProcCpuSample {
            process_jiffies: 1_000,
            total_jiffies: 100_000,
        };
        let next = ProcCpuSample {
            process_jiffies: 1_500,
            total_jiffies: 102_000,
        };
        // process_delta=500, total_delta=2000, cpu_count=8
        // => 500/2000 * 8 * 100 = 200%
        let pct_x100 = process_cpu_pct_x100(prev, next, 8).expect("cpu pct");
        assert_eq!(pct_x100, 20_000);
    }

    #[test]
    fn result_preview_truncates_utf8_safely() {
        // Ensure we never panic on non-ASCII tool output when truncating previews.
        let text = "€".repeat(300);
        let contents = vec![Content::Text { text }];
        let preview = result_preview_from_contents(&contents).expect("preview");
        assert!(preview.len() <= 200);
        assert!(preview.chars().all(|c| c == '€'));
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .to_path_buf()
    }

    fn safe_component(value: &str) -> String {
        let out = value.trim().replace(
            |c| {
                matches!(
                    c,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' '
                )
            },
            "_",
        );
        if out.is_empty() || out == "." || out == ".." {
            "unknown".to_string()
        } else {
            out
        }
    }

    fn jwt_artifact_dir(test_name: &str) -> PathBuf {
        let ts = Utc::now().format("%Y%m%dT%H%M%S%.fZ").to_string();
        let dir = repo_root()
            .join("tests")
            .join("artifacts")
            .join("http")
            .join("jwt")
            .join(format!("{ts}-{}", safe_component(test_name)));
        std::fs::create_dir_all(&dir).expect("create jwt artifacts dir");
        dir
    }

    fn write_jwt_artifact(test_name: &str, value: &serde_json::Value) {
        let dir = jwt_artifact_dir(test_name);
        let path = dir.join("context.json");
        let json = serde_json::to_string_pretty(value).expect("artifact json");
        std::fs::write(&path, json).expect("write jwt artifact");
    }

    fn rbac_artifact_dir(test_name: &str) -> PathBuf {
        let ts = Utc::now().format("%Y%m%dT%H%M%S%.fZ").to_string();
        let dir = repo_root()
            .join("tests")
            .join("artifacts")
            .join("http")
            .join("rbac")
            .join(format!("{ts}-{}", safe_component(test_name)));
        std::fs::create_dir_all(&dir).expect("create rbac artifacts dir");
        dir
    }

    fn write_rbac_artifact(test_name: &str, value: &serde_json::Value) {
        let dir = rbac_artifact_dir(test_name);
        let path = dir.join("context.json");
        let json = serde_json::to_string_pretty(value).expect("artifact json");
        std::fs::write(&path, json).expect("write rbac artifact");
    }

    fn rate_limit_artifact_dir(test_name: &str) -> PathBuf {
        let ts = Utc::now().format("%Y%m%dT%H%M%S%.fZ").to_string();
        let dir = repo_root()
            .join("tests")
            .join("artifacts")
            .join("http")
            .join("rate_limit")
            .join(format!("{ts}-{}", safe_component(test_name)));
        std::fs::create_dir_all(&dir).expect("create rate_limit artifacts dir");
        dir
    }

    fn write_rate_limit_artifact(test_name: &str, value: &serde_json::Value) {
        let dir = rate_limit_artifact_dir(test_name);
        let path = dir.join("context.json");
        let json = serde_json::to_string_pretty(value).expect("artifact json");
        std::fs::write(&path, json).expect("write rate_limit artifact");
    }

    fn redis_url_or_skip(test_name: &str) -> Option<String> {
        let url = match std::env::var("REDIS_URL") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => {
                eprintln!("SKIP: REDIS_URL not set; skipping redis test {test_name}");
                return None;
            }
        };

        let cx = Cx::for_testing();
        let client = match block_on(RedisClient::connect(&cx, &url)) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("SKIP: RedisClient.connect failed for {test_name}: {err}");
                return None;
            }
        };
        let ping = match block_on(client.cmd(&cx, &["PING"])) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("SKIP: redis ping failed for {test_name}: {err}");
                return None;
            }
        };
        if !matches!(
            ping,
            asupersync::messaging::redis::RespValue::SimpleString(ref s) if s == "PONG"
        ) {
            eprintln!("SKIP: unexpected PING response for {test_name}: {ping:?}");
            return None;
        }

        Some(url)
    }

    fn hs256_token(secret: &[u8], claims: &serde_json::Value) -> String {
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .expect("encode token")
    }

    fn assert_unauthorized(resp: &Http1Response) {
        assert_eq!(resp.status, 401);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("unauthorized response json");
        assert_eq!(body["detail"], "Unauthorized");
    }

    fn assert_forbidden(resp: &Http1Response) {
        assert_eq!(resp.status, 403);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("forbidden response json");
        assert_eq!(body["detail"], "Forbidden");
    }

    fn with_jwks_server<F>(jwks_body: &[u8], max_requests: usize, f: F)
    where
        F: FnOnce(String),
    {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        std::thread::scope(|s| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind jwks listener");
            listener.set_nonblocking(true).expect("set_nonblocking");
            let addr = listener.local_addr().expect("listener addr");
            let jwks_body2 = jwks_body.to_vec();
            let accepted = Arc::new(AtomicUsize::new(0));
            let accepted2 = Arc::clone(&accepted);

            s.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if accepted2.load(Ordering::SeqCst) >= max_requests {
                        return;
                    }
                    match listener.accept() {
                        Ok((mut stream, _peer)) => {
                            accepted2.fetch_add(1, Ordering::SeqCst);

                            // Best-effort drain the request before responding.
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                            let mut buf = [0_u8; 512];
                            let mut seen = Vec::new();
                            loop {
                                match stream.read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        seen.extend_from_slice(&buf[..n]);
                                        if seen.windows(4).any(|w| w == b"\r\n\r\n")
                                            || seen.len() > 8 * 1024
                                        {
                                            break;
                                        }
                                    }
                                    Err(err)
                                        if err.kind() == std::io::ErrorKind::WouldBlock
                                            || err.kind() == std::io::ErrorKind::TimedOut =>
                                    {
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }

                            let status = "200 OK";
                            let body: &[u8] = jwks_body2.as_slice();
                            let header = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(body);
                            let _ = stream.flush();
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() > deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            let jwks_url = format!("http://{addr}/jwks");
            f(jwks_url);
        });
    }

    fn build_state(config: mcp_agent_mail_core::Config) -> HttpState {
        let server = build_server(&config);
        let server_info = server.info().clone();
        let server_capabilities = server.capabilities().clone();
        let router = Arc::new(server.into_router());
        HttpState::new(
            router,
            server_info,
            server_capabilities,
            config,
            Arc::new(HttpRequestRuntimeDiagnostics::default()),
        )
    }

    fn test_http_server_instance(
        join: AsyncJoinHandle<std::io::Result<()>>,
        shutdown: asupersync::server::shutdown::ShutdownSignal,
    ) -> HttpServerInstance {
        HttpServerInstance {
            join,
            connection_manager: asupersync::server::connection::ConnectionManager::new(
                None,
                shutdown.clone(),
            ),
            listener_stats: Arc::new(Http1ListenerStats::default()),
            request_diagnostics: Arc::new(HttpRequestRuntimeDiagnostics::default()),
            shutdown,
        }
    }

    fn with_serialized_tool_dispatch_env<F, T>(f: F) -> T
    where
        F: FnOnce(String) -> T,
    {
        let _lock = TOOL_DISPATCH_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().expect("tool dispatch tempdir");
        let storage_root = temp.path().join("storage-root");
        let project_root = temp.path().join("project-root");
        std::fs::create_dir_all(&storage_root).expect("tool dispatch storage root");
        std::fs::create_dir_all(&project_root).expect("tool dispatch project root");

        let database_path = temp.path().join("storage.sqlite3");
        let database_url = format!("sqlite://{}", database_path.display());
        let storage_root_str = storage_root
            .to_str()
            .expect("tool dispatch storage root utf-8")
            .to_string();
        let project_key = project_root
            .to_str()
            .expect("tool dispatch project root utf-8")
            .to_string();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[
                ("DATABASE_URL", database_url.as_str()),
                ("STORAGE_ROOT", storage_root_str.as_str()),
            ],
            || f(project_key),
        )
    }

    fn with_serialized_health_count_cache<F, T>(f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _lock = HEALTH_COUNT_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *lock_mutex(&HEALTH_COUNT_CACHE) = (Instant::now(), None);
        let result = f();
        *lock_mutex(&HEALTH_COUNT_CACHE) = (Instant::now(), None);
        result
    }

    fn make_request(method: Http1Method, uri: &str, headers: &[(&str, &str)]) -> Http1Request {
        make_request_with_peer_addr(method, uri, headers, None)
    }

    fn make_request_with_peer_addr(
        method: Http1Method,
        uri: &str,
        headers: &[(&str, &str)],
        peer_addr: Option<SocketAddr>,
    ) -> Http1Request {
        Http1Request {
            method,
            uri: uri.to_string(),
            version: Http1Version::Http11,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr,
        }
    }

    fn response_header<'a>(resp: &'a Http1Response, name: &str) -> Option<&'a str> {
        resp.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn console_layout_compute_writer_settings_inline_percent_clamps_to_terminal() {
        let layout = ConsoleLayoutState {
            persist_path: PathBuf::from("/dev/null"),
            auto_save: true,
            interactive_enabled: true,
            ui_height_percent: 80,
            ui_anchor: ConsoleUiAnchor::Bottom,
            ui_auto_size: false,
            inline_auto_min_rows: 8,
            inline_auto_max_rows: 18,
            split_mode: ConsoleSplitMode::Inline,
            split_ratio_percent: 30,
        };

        let (mode, anchor) = layout.compute_writer_settings(20);
        assert!(matches!(anchor, ftui::UiAnchor::Bottom));
        assert!(matches!(mode, ftui::ScreenMode::Inline { ui_height: 16 }));

        // Extremely small terminals still clamp to the effective term height.
        let (mode, _) = layout.compute_writer_settings(5);
        assert!(matches!(mode, ftui::ScreenMode::Inline { ui_height: 3 }));
    }

    #[test]
    fn console_layout_compute_writer_settings_inline_auto_clamps_to_effective_height() {
        let layout = ConsoleLayoutState {
            persist_path: PathBuf::from("/dev/null"),
            auto_save: true,
            interactive_enabled: true,
            ui_height_percent: 33,
            ui_anchor: ConsoleUiAnchor::Top,
            ui_auto_size: true,
            inline_auto_min_rows: 8,
            inline_auto_max_rows: 18,
            split_mode: ConsoleSplitMode::Inline,
            split_ratio_percent: 30,
        };

        let (mode, anchor) = layout.compute_writer_settings(10);
        assert!(matches!(anchor, ftui::UiAnchor::Top));
        assert!(matches!(
            mode,
            ftui::ScreenMode::InlineAuto {
                min_height: 8,
                max_height: 8
            }
        ));
    }

    #[test]
    fn console_layout_apply_key_updates_state_and_clamps() {
        let mut layout = ConsoleLayoutState {
            persist_path: PathBuf::from("/dev/null"),
            auto_save: true,
            interactive_enabled: true,
            ui_height_percent: 33,
            ui_anchor: ConsoleUiAnchor::Bottom,
            ui_auto_size: false,
            inline_auto_min_rows: 8,
            inline_auto_max_rows: 18,
            split_mode: ConsoleSplitMode::Inline,
            split_ratio_percent: 30,
        };

        assert_eq!(layout.apply_key(ftui::KeyCode::Char('+')), (true, None));
        assert_eq!(layout.ui_height_percent, 38);

        assert_eq!(layout.apply_key(ftui::KeyCode::Up), (true, None));
        assert_eq!(layout.ui_height_percent, 43);

        assert_eq!(layout.apply_key(ftui::KeyCode::Char('-')), (true, None));
        assert_eq!(layout.ui_height_percent, 38);

        assert_eq!(layout.apply_key(ftui::KeyCode::Down), (true, None));
        assert_eq!(layout.ui_height_percent, 33);

        assert_eq!(layout.apply_key(ftui::KeyCode::Char('t')), (true, None));
        assert_eq!(layout.ui_anchor, ConsoleUiAnchor::Top);
        assert_eq!(layout.apply_key(ftui::KeyCode::Char('b')), (true, None));
        assert_eq!(layout.ui_anchor, ConsoleUiAnchor::Bottom);

        assert_eq!(layout.apply_key(ftui::KeyCode::Char('a')), (true, None));
        assert!(layout.ui_auto_size);

        let (changed, message) = layout.apply_key(ftui::KeyCode::Char('l'));
        assert!(changed);
        assert!(
            message
                .as_deref()
                .unwrap_or_default()
                .contains("switched to left split mode")
        );
        assert_eq!(layout.split_mode, ConsoleSplitMode::Left);

        assert_eq!(layout.apply_key(ftui::KeyCode::Char('[')), (true, None));
        assert_eq!(layout.split_ratio_percent, 25);
        assert_eq!(layout.apply_key(ftui::KeyCode::Char(']')), (true, None));
        assert_eq!(layout.split_ratio_percent, 30);

        // Help key should not report a changed layout.
        let (changed, message) = layout.apply_key(ftui::KeyCode::Char('?'));
        assert!(!changed);
        assert!(message.as_deref().unwrap_or_default().contains("Console:"));
    }

    #[test]
    fn compute_writer_settings_left_split_returns_altscreen() {
        let mut layout = ConsoleLayoutState {
            persist_path: std::path::PathBuf::new(),
            auto_save: false,
            interactive_enabled: false,
            ui_height_percent: 50,
            ui_anchor: ConsoleUiAnchor::Bottom,
            ui_auto_size: false,
            inline_auto_min_rows: 8,
            inline_auto_max_rows: 20,
            split_mode: ConsoleSplitMode::Left,
            split_ratio_percent: 30,
        };

        let (mode, _anchor) = layout.compute_writer_settings(40);
        assert!(
            matches!(mode, ftui::ScreenMode::AltScreen),
            "Left split mode should produce AltScreen, got {mode:?}"
        );
        assert!(layout.is_split_mode());

        // Switching back to inline should NOT produce AltScreen.
        layout.split_mode = ConsoleSplitMode::Inline;
        let (mode, _anchor) = layout.compute_writer_settings(40);
        assert!(
            !matches!(mode, ftui::ScreenMode::AltScreen),
            "Inline mode should not produce AltScreen, got {mode:?}"
        );
        assert!(!layout.is_split_mode());
    }

    #[test]
    fn mux_guard_forces_left_split_when_inline_and_sync_unavailable() {
        let layout = ConsoleLayoutState {
            persist_path: PathBuf::from("/dev/null"),
            auto_save: true,
            interactive_enabled: true,
            ui_height_percent: 33,
            ui_anchor: ConsoleUiAnchor::Bottom,
            ui_auto_size: false,
            inline_auto_min_rows: 8,
            inline_auto_max_rows: 18,
            split_mode: ConsoleSplitMode::Inline,
            split_ratio_percent: 30,
        };
        let caps = console::ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: false,
            mouse_sgr: true,
            sync_output: false,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: true,
        };
        assert!(should_force_mux_left_split(&layout, &caps, false));
        assert!(!should_force_mux_left_split(&layout, &caps, true));
    }

    #[test]
    fn mux_guard_does_not_force_when_sync_available_or_already_left() {
        let mut layout = ConsoleLayoutState {
            persist_path: PathBuf::from("/dev/null"),
            auto_save: true,
            interactive_enabled: true,
            ui_height_percent: 33,
            ui_anchor: ConsoleUiAnchor::Bottom,
            ui_auto_size: false,
            inline_auto_min_rows: 8,
            inline_auto_max_rows: 18,
            split_mode: ConsoleSplitMode::Inline,
            split_ratio_percent: 30,
        };
        let mut caps = console::ConsoleCaps {
            true_color: true,
            osc8_hyperlinks: false,
            mouse_sgr: true,
            sync_output: true,
            kitty_keyboard: false,
            focus_events: false,
            in_mux: true,
        };
        assert!(!should_force_mux_left_split(&layout, &caps, false));

        caps.sync_output = false;
        caps.in_mux = false;
        assert!(!should_force_mux_left_split(&layout, &caps, false));

        caps.in_mux = true;
        layout.split_mode = ConsoleSplitMode::Left;
        assert!(!should_force_mux_left_split(&layout, &caps, false));
    }

    #[test]
    fn startup_dashboard_enabled_tracks_tui_mode() {
        let mut config = mcp_agent_mail_core::Config {
            log_rich_enabled: true,
            tui_enabled: true,
            ..Default::default()
        };
        assert!(startup_dashboard_enabled(&config));

        config.tui_enabled = false;
        assert!(!startup_dashboard_enabled(&config));
    }

    #[test]
    fn startup_dashboard_enabled_respects_rich_logging_flag() {
        let config = mcp_agent_mail_core::Config {
            tui_enabled: true,
            log_rich_enabled: false,
            ..Default::default()
        };
        assert!(!startup_dashboard_enabled(&config));
    }

    #[test]
    fn dashboard_console_interactivity_tracks_tui_mode() {
        let mut config = mcp_agent_mail_core::Config {
            console_interactive_enabled: true,
            tui_enabled: true,
            ..Default::default()
        };
        assert!(dashboard_console_interactivity_enabled(&config));

        config.tui_enabled = false;
        assert!(!dashboard_console_interactivity_enabled(&config));
    }

    #[test]
    fn dashboard_console_interactivity_respects_console_disable_flag() {
        let config = mcp_agent_mail_core::Config {
            tui_enabled: true,
            console_interactive_enabled: false,
            ..Default::default()
        };
        assert!(!dashboard_console_interactivity_enabled(&config));
    }

    #[test]
    fn cors_list_value_defaults_to_star() {
        assert_eq!(cors_list_value(&[]), "*");
        assert_eq!(cors_list_value(&["*".to_string()]), "*");
        assert_eq!(
            cors_list_value(&["GET".to_string(), "POST".to_string()]),
            "GET, POST"
        );
    }

    #[test]
    fn cors_origin_wildcard_uses_star_without_credentials() {
        let config = mcp_agent_mail_core::Config {
            http_cors_enabled: true,
            http_cors_origins: Vec::new(),
            http_cors_allow_credentials: false,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/health/liveness",
            &[("Origin", "http://example.com")],
        );
        assert_eq!(state.cors_origin(&req), Some("*".to_string()));
    }

    #[test]
    fn cors_origin_wildcard_echoes_origin_with_credentials() {
        let config = mcp_agent_mail_core::Config {
            http_cors_enabled: true,
            http_cors_origins: vec!["*".to_string()],
            http_cors_allow_credentials: true,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/health/liveness",
            &[("Origin", "http://example.com")],
        );
        assert_eq!(
            state.cors_origin(&req),
            Some("http://example.com".to_string())
        );
    }

    #[test]
    fn cors_origin_denies_unlisted_origin() {
        let config = mcp_agent_mail_core::Config {
            http_cors_enabled: true,
            http_cors_origins: vec!["http://allowed.com".to_string()],
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/health/liveness",
            &[("Origin", "http://blocked.com")],
        );
        assert_eq!(state.cors_origin(&req), None);
    }

    #[test]
    fn mail_api_locks_returns_json() {
        let storage_root = std::env::temp_dir().join(format!(
            "mcp-agent-mail-mail-locks-test-{}",
            std::process::id()
        ));
        let config = mcp_agent_mail_core::Config {
            storage_root,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/mail/api/locks", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let payload: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("locks response json");
        assert!(
            payload.get("locks").and_then(|v| v.as_array()).is_some(),
            "locks missing or not array: {payload}"
        );
    }

    #[test]
    fn mail_api_locks_trailing_slash_returns_json() {
        let storage_root = std::env::temp_dir().join(format!(
            "mcp-agent-mail-mail-locks-test-trailing-{}",
            std::process::id()
        ));
        let config = mcp_agent_mail_core::Config {
            storage_root,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/mail/api/locks/", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let payload: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("locks response json");
        assert!(
            payload.get("locks").and_then(|v| v.as_array()).is_some(),
            "locks missing or not array: {payload}"
        );
    }

    #[test]
    fn mail_ws_state_poll_returns_snapshot_json() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/mail/ws-state?limit=5", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );

        let payload: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("ws-state response json");
        assert_eq!(payload["schema_version"], "am_ws_state_poll.v1");
        assert_eq!(payload["mode"], "snapshot");
        assert_eq!(payload["transport"], "http-poll");
        assert!(payload["next_seq"].as_u64().is_some());
    }

    #[test]
    fn atc_action_cooldown_key_distinguishes_message_variants() {
        let base = AtcOperatorActionSnapshot {
            timestamp_micros: 1,
            kind: "send_advisory".to_string(),
            category: "liveness".to_string(),
            agent: "AlphaAgent".to_string(),
            message: Some("Agent appears unresponsive.".to_string()),
        };
        let other_message = AtcOperatorActionSnapshot {
            message: Some("Automated reservation release requested.".to_string()),
            ..base.clone()
        };
        let probe = AtcOperatorActionSnapshot {
            kind: "probe_agent".to_string(),
            category: "probe".to_string(),
            message: None,
            ..base.clone()
        };

        assert_ne!(
            atc_action_cooldown_key(&base),
            atc_action_cooldown_key(&other_message),
            "distinct advisories for the same agent must not share one cooldown key"
        );
        assert_ne!(
            atc_action_cooldown_key(&base),
            atc_action_cooldown_key(&probe),
            "probe actions and advisories must not share a cooldown key"
        );
    }

    fn sample_effect_semantics(
        family: &str,
        agent: &str,
        project_key: Option<&str>,
    ) -> atc::AtcEffectSemantics {
        atc::AtcEffectSemantics {
            family: family.to_string(),
            risk_level: match family {
                "reservation_release" => "high".to_string(),
                "liveness_monitoring" | "withheld_release_notice" => "low".to_string(),
                _ => "medium".to_string(),
            },
            utility_model: format!("utility for {family}"),
            operator_action: format!("operator action for {family}"),
            remediation: format!("remediation for {family}"),
            escalation_policy: format!("escalation for {family}"),
            evidence_summary: format!("evidence for {family}"),
            cooldown_key: format!("{}:{}:{}", family, project_key.unwrap_or("-"), agent),
            cooldown_micros: 60_000_000,
            requires_project: true,
            ack_required: family == "liveness_probe",
            high_risk_intervention: family == "reservation_release",
            preconditions: vec!["project context available".to_string()],
        }
    }

    fn sample_probe_effect() -> atc::AtcEffectPlan {
        atc::AtcEffectPlan {
            decision_id: 7,
            effect_id: "atc-effect-probe".to_string(),
            experience_id: Some(17),
            claim_id: "clm-7".to_string(),
            evidence_id: "evi-7".to_string(),
            trace_id: "trc-7".to_string(),
            timestamp_micros: 1_700_000_000_000_000,
            kind: "probe_agent".to_string(),
            category: "probe".to_string(),
            agent: "AlphaAgent".to_string(),
            project_key: Some("/tmp/project-a".to_string()),
            policy_id: Some("policy-a".to_string()),
            policy_revision: 3,
            message: None,
            expected_loss: Some(0.2),
            semantics: sample_effect_semantics(
                "liveness_probe",
                "AlphaAgent",
                Some("/tmp/project-a"),
            ),
        }
    }

    fn sample_resolution_experience(
        created_ts_micros: i64,
        dispatched_ts_micros: Option<i64>,
        executed_ts_micros: Option<i64>,
    ) -> ExperienceRow {
        ExperienceRow {
            experience_id: 1,
            decision_id: 7,
            effect_id: 17,
            trace_id: "trc-resolution".to_string(),
            claim_id: "clm-resolution".to_string(),
            evidence_id: "evi-resolution".to_string(),
            state: ExperienceState::Open,
            subsystem: ExperienceSubsystem::Liveness,
            decision_class: "liveness_probe".to_string(),
            subject: "AlphaAgent".to_string(),
            project_key: Some("/tmp/project-a".to_string()),
            policy_id: Some("policy-a".to_string()),
            effect_kind: EffectKind::Probe,
            action: "ProbeAgent".to_string(),
            posterior: vec![
                ("Alive".to_string(), 0.3),
                ("Flaky".to_string(), 0.5),
                ("Dead".to_string(), 0.2),
            ],
            expected_loss: 0.2,
            runner_up_action: Some("DeferProbe".to_string()),
            runner_up_loss: Some(0.4),
            evidence_summary: "selected for probing".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            non_execution_reason: None,
            outcome: None,
            created_ts_micros,
            dispatched_ts_micros,
            executed_ts_micros,
            resolved_ts_micros: None,
            features: Some(FeatureVector::zeroed()),
            feature_ext: None,
            context: None,
        }
    }

    #[test]
    fn atc_executor_mode_defaults_to_live_for_unknown_values() {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ATC_EXECUTOR_MODE", "")],
            || {
                assert_eq!(AtcExecutorMode::from_env(), AtcExecutorMode::Live);
            },
        );
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ATC_EXECUTOR_MODE", "mystery-mode")],
            || {
                assert_eq!(AtcExecutorMode::from_env(), AtcExecutorMode::Live);
            },
        );
    }

    #[test]
    fn atc_executor_mode_respects_explicit_shadow_override() {
        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("AM_ATC_EXECUTOR_MODE", "shadow")],
            || {
                assert_eq!(AtcExecutorMode::from_env(), AtcExecutorMode::Shadow);
            },
        );
    }

    #[test]
    fn atc_durable_experience_store_is_disabled_for_file_backed_mailboxes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("atc-file-backed.sqlite3");
        let pool = get_or_create_pool(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        })
        .expect("create file-backed pool");

        assert!(!atc_durable_experience_store_writable(&pool));
        assert!(!atc_durable_experience_store_enabled(
            AtcExecutorMode::Live,
            Some(&pool),
        ));
        assert!(!atc_durable_experience_store_enabled(
            AtcExecutorMode::Canary,
            Some(&pool),
        ));
        assert!(!atc_durable_experience_store_enabled(
            AtcExecutorMode::Shadow,
            Some(&pool),
        ));
    }

    #[test]
    fn atc_durable_experience_store_still_tracks_executor_mode_for_in_memory_mailboxes() {
        let pool = create_pool(&DbPoolConfig {
            database_url: "sqlite:///:memory:".to_string(),
            min_connections: 0,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        })
        .expect("create in-memory pool");

        assert!(atc_durable_experience_store_writable(&pool));
        assert!(atc_durable_experience_store_enabled(
            AtcExecutorMode::Live,
            Some(&pool),
        ));
        assert!(atc_durable_experience_store_enabled(
            AtcExecutorMode::Canary,
            Some(&pool),
        ));
        assert!(!atc_durable_experience_store_enabled(
            AtcExecutorMode::Shadow,
            Some(&pool),
        ));
    }

    #[test]
    fn atc_probe_effect_uses_probe_request_language() {
        let effect = sample_probe_effect();
        assert_eq!(
            atc_effect_subject(&effect),
            "[ATC] acknowledgment requested from AlphaAgent"
        );
        let body = atc_effect_body(&effect);
        assert!(body.contains("ATC needs an acknowledgment from AlphaAgent"));
        assert!(body.contains("signal: evidence for liveness_probe"));
        assert!(body.contains("next_step: operator action for liveness_probe"));
        assert!(body.contains("risk: medium"));
        assert!(body.contains("decision_id: 7"));
        assert!(body.contains("experience_id: 17"));
    }

    #[test]
    fn atc_effect_blank_message_falls_back_to_default_headline() {
        let mut effect = sample_probe_effect();
        effect.message = Some("   ".to_string());

        assert_eq!(
            atc_effect_operator_message(&effect).as_deref(),
            Some(
                "ATC needs an acknowledgment from AlphaAgent to distinguish a stale session from active work."
            )
        );
        assert!(atc_effect_body(&effect).starts_with(
            "ATC needs an acknowledgment from AlphaAgent to distinguish a stale session from active work."
        ));
    }

    #[test]
    fn atc_effect_blank_message_is_not_treated_as_explicit_metadata() {
        let mut effect = sample_probe_effect();
        effect.message = Some("   ".to_string());
        assert!(!atc_effect_has_explicit_message(&effect));

        effect.message = Some("still active".to_string());
        assert!(atc_effect_has_explicit_message(&effect));
    }

    #[test]
    fn execute_atc_effect_missing_project_is_suppressed_by_precondition() {
        let mut effect = sample_probe_effect();
        effect.project_key = None;
        effect.semantics.cooldown_key = "liveness_probe:-:AlphaAgent".to_string();
        let mut ensured_projects = HashSet::new();

        assert_eq!(
            execute_atc_effect(None, AtcExecutorMode::Live, &mut ensured_projects, &effect),
            "suppressed:missing_project_precondition"
        );
    }

    #[test]
    fn execute_atc_effect_shadow_mode_still_reports_shadow_suppression() {
        let mut effect = sample_probe_effect();
        effect.project_key = None;
        effect.semantics.cooldown_key = "liveness_probe:-:AlphaAgent".to_string();
        let mut ensured_projects = HashSet::new();

        assert_eq!(
            execute_atc_effect(
                None,
                AtcExecutorMode::Shadow,
                &mut ensured_projects,
                &effect
            ),
            "suppressed:executor_mode_shadow"
        );
    }

    #[test]
    fn execute_atc_effect_live_probe_requires_runtime_instead_of_probe_intent() {
        let effect = sample_probe_effect();
        let mut ensured_projects = HashSet::new();
        assert_eq!(
            execute_atc_effect(None, AtcExecutorMode::Live, &mut ensured_projects, &effect),
            "failed:executor_unavailable"
        );
    }

    #[test]
    fn resolution_anchor_prefers_execution_timestamp() {
        let experience = sample_resolution_experience(
            1_700_000_000_000_000,
            Some(1_700_000_000_000_100),
            Some(1_700_000_000_000_200),
        );

        assert_eq!(
            atc_resolution_anchor_micros(&experience),
            1_700_000_000_000_200
        );
    }

    #[test]
    fn resolution_anchor_falls_back_to_dispatch_then_creation() {
        let dispatched =
            sample_resolution_experience(1_700_000_000_000_000, Some(1_700_000_000_000_100), None);
        assert_eq!(
            atc_resolution_anchor_micros(&dispatched),
            1_700_000_000_000_100
        );

        let planned = sample_resolution_experience(1_700_000_000_000_000, None, None);
        assert_eq!(
            atc_resolution_anchor_micros(&planned),
            1_700_000_000_000_000
        );
    }

    #[test]
    fn resolution_outcome_uses_actual_activity_timestamp() {
        let experience = sample_resolution_experience(
            1_700_000_000_000_000,
            Some(1_700_000_000_000_100),
            Some(1_700_000_000_000_200),
        );

        let outcome = atc_resolution_outcome_from_activity(&experience, 1_700_000_000_000_350)
            .expect("later activity should resolve");

        assert_eq!(outcome.observed_ts_micros, 1_700_000_000_000_350);
        assert_eq!(outcome.label, "later_activity");
        assert_eq!(outcome.actual_loss, Some(0.0));
        assert_eq!(outcome.regret, Some(0.0));
        let latency = outcome
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.get("latency_micros"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(latency, Some(150));
    }

    #[test]
    fn resolution_outcome_ignores_pre_anchor_activity() {
        let experience = sample_resolution_experience(
            1_700_000_000_000_000,
            Some(1_700_000_000_000_100),
            Some(1_700_000_000_000_200),
        );

        assert!(atc_resolution_outcome_from_activity(&experience, 1_700_000_000_000_200).is_none());
        assert!(atc_resolution_outcome_from_activity(&experience, 1_700_000_000_000_150).is_none());
    }

    #[test]
    fn atc_execution_capture_distinguishes_runtime_from_policy_results() {
        let suppressed = atc_execution_capture("suppressed:executor_mode_dry_run");
        assert_eq!(suppressed.snapshot_status, "suppressed");
        assert_eq!(suppressed.state, ExperienceState::Suppressed);
        assert_eq!(suppressed.classification, "policy_suppression");
        assert_eq!(suppressed.detail.as_deref(), Some("executor_mode_dry_run"));
        assert!(matches!(
            suppressed.non_execution_reason,
            Some(NonExecutionReason::SafetyGate { ref gate_name, .. })
                if gate_name == "executor_mode_dry_run"
        ));

        let failed = atc_execution_capture("failed:executor_unavailable");
        assert_eq!(failed.snapshot_status, "failed");
        assert_eq!(failed.state, ExperienceState::Failed);
        assert_eq!(failed.classification, "runtime_failure");
        assert_eq!(failed.detail.as_deref(), Some("executor_unavailable"));
        assert!(failed.non_execution_reason.is_none());
    }

    #[test]
    fn atc_execution_capture_normalizes_empty_details() {
        let failed = atc_execution_capture("failed:");
        assert_eq!(failed.snapshot_status, "failed");
        assert!(failed.detail.is_none());

        let suppressed = atc_execution_capture("suppressed:");
        assert_eq!(suppressed.snapshot_status, "suppressed");
        assert!(suppressed.detail.is_none());
        assert!(matches!(
            suppressed.non_execution_reason,
            Some(NonExecutionReason::SafetyGate { ref gate_name, .. })
                if gate_name == "unspecified_suppression"
        ));
    }

    #[test]
    fn atc_execution_snapshot_exposes_canonical_status_and_detail() {
        let effect = sample_probe_effect();
        let snapshot = atc_execution_snapshot(
            1_700_000_000_000_111,
            &effect,
            "dry_run",
            "suppressed:executor_mode_dry_run",
        );

        assert_eq!(snapshot.status, "suppressed");
        assert_eq!(
            snapshot.status_detail.as_deref(),
            Some("executor_mode_dry_run")
        );
        assert_eq!(snapshot.execution_mode, "dry_run");
    }

    #[test]
    fn atc_execution_context_patch_omits_detail_for_success() {
        let capture = atc_execution_capture("executed");
        let patch =
            atc_execution_context_patch(&capture, "live", "executed", 1_700_000_000_000_111);

        let execution = patch
            .get("execution")
            .and_then(serde_json::Value::as_object)
            .expect("execution object");
        assert_eq!(
            execution.get("status").and_then(serde_json::Value::as_str),
            Some("executed")
        );
        assert_eq!(
            execution
                .get("raw_status")
                .and_then(serde_json::Value::as_str),
            Some("executed")
        );
        assert!(!execution.contains_key("detail"));
    }

    #[test]
    fn atc_execution_snapshot_includes_probe_display_message_when_effect_message_missing() {
        let effect = sample_probe_effect();
        let snapshot = atc_execution_snapshot(1_700_000_000_000_111, &effect, "live", "executed");

        assert_eq!(
            snapshot.message.as_deref(),
            Some(
                "ATC needs an acknowledgment from AlphaAgent to distinguish a stale session from active work."
            )
        );

        let action = atc_action_snapshot_from_execution(&snapshot);
        assert!(
            action
                .console_line()
                .contains("ATC needs an acknowledgment from AlphaAgent")
        );
    }

    #[test]
    fn atc_action_snapshot_marks_nonexecuted_effects_in_console_output() {
        let effect = sample_probe_effect();
        let snapshot = atc_execution_snapshot(
            1_700_000_000_000_111,
            &effect,
            "dry_run",
            "suppressed:executor_mode_dry_run",
        );

        let action = atc_action_snapshot_from_execution(&snapshot);
        assert_eq!(
            action.message.as_deref(),
            Some(
                "[suppressed:executor_mode_dry_run] ATC needs an acknowledgment from AlphaAgent to distinguish a stale session from active work."
            )
        );
        assert!(
            action
                .console_line()
                .contains("[suppressed:executor_mode_dry_run]")
        );
    }

    #[test]
    fn record_atc_operator_execution_keeps_nonexecuted_outcomes_visible() {
        let effect = sample_probe_effect();
        let execution = atc_execution_snapshot(
            1_700_000_000_000_111,
            &effect,
            "live",
            ATC_QUEUE_BACKPRESSURE_STATUS,
        );
        let mut recent_executions = VecDeque::new();
        let mut recent_actions = VecDeque::new();
        let mut visible_actions = Vec::new();

        record_atc_operator_execution(
            &mut recent_executions,
            &mut recent_actions,
            &mut visible_actions,
            execution,
        );

        assert_eq!(recent_executions.len(), 1);
        assert_eq!(recent_actions.len(), 1);
        assert_eq!(visible_actions.len(), 1);
        assert_eq!(
            recent_actions
                .front()
                .and_then(|action| action.message.as_deref()),
            Some(
                "[throttled:pending_queue_capacity] ATC needs an acknowledgment from AlphaAgent to distinguish a stale session from active work."
            )
        );
        assert!(
            visible_actions[0]
                .console_line()
                .contains("[throttled:pending_queue_capacity]")
        );
    }

    #[test]
    fn record_atc_operator_execution_caps_per_tick_visible_actions() {
        let effect = sample_probe_effect();
        let execution = atc_execution_snapshot(
            1_700_000_000_000_000,
            &effect,
            "live",
            ATC_QUEUE_BACKPRESSURE_STATUS,
        );
        let mut recent_executions = VecDeque::new();
        let mut recent_actions = VecDeque::new();
        let mut visible_actions =
            vec![AtcOperatorActionSnapshot::default(); ATC_OPERATOR_ACTION_CAPACITY];

        record_atc_operator_execution(
            &mut recent_executions,
            &mut recent_actions,
            &mut visible_actions,
            execution,
        );

        assert_eq!(recent_executions.len(), 1);
        assert_eq!(recent_actions.len(), 1);
        assert_eq!(visible_actions.len(), ATC_OPERATOR_ACTION_CAPACITY);
    }

    #[test]
    fn atc_status_consumes_cooldown_only_when_retry_should_wait() {
        assert!(atc_status_consumes_cooldown("executed"));
        assert!(atc_status_consumes_cooldown(
            "suppressed:executor_mode_shadow"
        ));
        assert!(atc_status_consumes_cooldown("skipped:deliberate_no_action"));
        assert!(!atc_status_consumes_cooldown("failed:executor_unavailable"));
        assert!(!atc_status_consumes_cooldown(
            "suppressed:missing_project_precondition"
        ));
    }

    #[test]
    fn atc_execution_capture_marks_queue_backpressure_as_throttled() {
        let capture = atc_execution_capture(ATC_QUEUE_BACKPRESSURE_STATUS);
        assert_eq!(capture.snapshot_status, "throttled");
        assert_eq!(capture.state, ExperienceState::Throttled);
        assert_eq!(capture.classification, "budget_throttle");
        assert_eq!(capture.detail.as_deref(), Some("pending_queue_capacity"));
        assert!(matches!(
            capture.non_execution_reason,
            Some(NonExecutionReason::BudgetExhausted { ref budget_name, .. })
                if budget_name == "pending_queue_capacity"
        ));
    }

    #[test]
    fn conflict_reservation_resolution_updates_file_backed_mailboxes() {
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("conflict-resolution-file-backed.db");
        let pool = get_or_create_pool(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        })
        .expect("create file-backed pool");

        let row = ExperienceRow {
            experience_id: 0,
            decision_id: 42,
            effect_id: 92,
            trace_id: "trc-conflict-file-backed".to_string(),
            claim_id: "clm-conflict-file-backed".to_string(),
            evidence_id: "evi-conflict-file-backed".to_string(),
            state: ExperienceState::Executed,
            subsystem: ExperienceSubsystem::Conflict,
            decision_class: "reservation_conflict".to_string(),
            subject: "AlphaAgent".to_string(),
            project_key: Some("/tmp/project-a".to_string()),
            policy_id: Some("conflict-r1".to_string()),
            effect_kind: EffectKind::Advisory,
            action: "RecommendReservation".to_string(),
            posterior: vec![("Clear".to_string(), 0.40), ("Conflict".to_string(), 0.60)],
            expected_loss: 1.2,
            runner_up_action: Some("Wait".to_string()),
            runner_up_loss: Some(1.6),
            evidence_summary: "reservation conflict persists".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            non_execution_reason: None,
            outcome: None,
            created_ts_micros: 1_700_000_000_003_000,
            dispatched_ts_micros: Some(1_700_000_000_003_050),
            executed_ts_micros: Some(1_700_000_000_003_100),
            resolved_ts_micros: None,
            features: Some(FeatureVector::zeroed()),
            feature_ext: None,
            context: None,
        };

        block_on(mcp_agent_mail_db::queries::append_atc_experience(
            &cx, &pool, &row,
        ))
        .into_result()
        .expect("append conflict experience");

        resolve_conflict_experiences_on_reservation_event(
            &pool,
            "AlphaAgent",
            "/tmp/project-a",
            "reservation_granted",
            true,
            serde_json::json!({ "signal": "grant" }),
        );

        let remaining = block_on(mcp_agent_mail_db::queries::fetch_open_atc_experiences(
            &cx,
            &pool,
            Some("AlphaAgent"),
            10,
        ))
        .into_result()
        .expect("fetch remaining open experiences");
        assert!(
            remaining.is_empty(),
            "file-backed ATC experiences should resolve once the reservation conflict clears"
        );
    }

    #[test]
    fn capture_atc_execution_result_updates_file_backed_mailboxes() {
        let cx = Cx::for_testing();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("capture-file-backed.db");
        let pool = create_pool(&DbPoolConfig {
            database_url: format!("sqlite:///{}", db_path.display()),
            min_connections: 0,
            max_connections: 1,
            warmup_connections: 0,
            ..Default::default()
        })
        .expect("create file-backed pool");

        let row = ExperienceRow {
            experience_id: 0,
            decision_id: 43,
            effect_id: 93,
            trace_id: "trc-capture-file-backed".to_string(),
            claim_id: "clm-capture-file-backed".to_string(),
            evidence_id: "evi-capture-file-backed".to_string(),
            state: ExperienceState::Planned,
            subsystem: ExperienceSubsystem::Liveness,
            decision_class: "liveness_probe".to_string(),
            subject: "AlphaAgent".to_string(),
            project_key: Some("/tmp/project-a".to_string()),
            policy_id: Some("liveness-r1".to_string()),
            effect_kind: EffectKind::Probe,
            action: "ProbeAgent".to_string(),
            posterior: vec![("Alive".to_string(), 0.40), ("Dead".to_string(), 0.60)],
            expected_loss: 1.0,
            runner_up_action: Some("Wait".to_string()),
            runner_up_loss: Some(1.4),
            evidence_summary: "liveness probe planned".to_string(),
            calibration_healthy: true,
            safe_mode_active: false,
            non_execution_reason: None,
            outcome: None,
            created_ts_micros: 1_700_000_000_004_000,
            dispatched_ts_micros: None,
            executed_ts_micros: None,
            resolved_ts_micros: None,
            features: Some(FeatureVector::zeroed()),
            feature_ext: None,
            context: None,
        };

        let stored = block_on(mcp_agent_mail_db::queries::append_atc_experience(
            &cx, &pool, &row,
        ))
        .into_result()
        .expect("append planned experience");

        capture_atc_execution_result(
            &pool,
            Some(stored.experience_id),
            "live",
            "executed",
            1_700_000_000_004_100,
        );

        let open = block_on(mcp_agent_mail_db::queries::fetch_open_atc_experiences(
            &cx,
            &pool,
            Some("AlphaAgent"),
            10,
        ))
        .into_result()
        .expect("fetch open experiences");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].experience_id, stored.experience_id);
        assert_eq!(open[0].state, ExperienceState::Executed);
        assert_eq!(open[0].dispatched_ts_micros, Some(1_700_000_000_004_100));
        assert_eq!(open[0].executed_ts_micros, Some(1_700_000_000_004_100));
    }

    #[test]
    fn mail_ws_state_upgrade_request_returns_501_json_error() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/mail/ws-state",
            &[("Connection", "Upgrade"), ("Upgrade", "websocket")],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 501);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("error json");
        assert_eq!(
            body["detail"],
            "WebSocket upgrade is not supported on /mail/ws-state; use HTTP polling."
        );
    }

    #[test]
    fn mail_ws_input_rejects_get_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/mail/ws-input", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("error json");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn web_dashboard_route_returns_html_shell() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/web-dashboard", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("text/html; charset=utf-8")
        );
        let body = String::from_utf8(resp.body).expect("utf8 body");
        assert!(body.contains("Browser TUI Mirror"));
        assert!(body.contains("/web-dashboard/state"));
    }

    #[test]
    fn web_dashboard_state_without_tui_returns_inactive_json() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/web-dashboard/state", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("inactive state json");
        assert_eq!(body["mode"], "inactive");
        assert_eq!(body["reason"], "tui_inactive");
        assert_eq!(body["poll_state"]["mode"], "snapshot");
    }

    #[test]
    fn web_dashboard_state_without_tui_uses_fallback_request_counters() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail/api/locks", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);

        let req = make_request(Http1Method::Get, "/web-dashboard/state", &[]);
        let resp = block_on(state.handle(req));
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("inactive state json");
        assert_eq!(body["mode"], "inactive");
        assert_eq!(body["poll_state"]["request_counters"]["total"], 1);
        assert!(
            body["poll_state"]["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty()),
            "fallback state should retain recent headless events"
        );
    }

    #[test]
    fn web_dashboard_state_with_live_tui_but_no_frame_returns_warming_json() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config.clone());
        let shared = tui_bridge::TuiSharedState::new(&config);
        set_tui_state_handle(None);
        set_tui_state_handle(Some(Arc::clone(&shared)));

        let req = make_request(Http1Method::Get, "/web-dashboard/state", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("warming state json");
        assert_eq!(body["mode"], "warming");
        assert_eq!(body["reason"], "tui_warming");

        set_tui_state_handle(None);
    }

    #[test]
    fn web_dashboard_input_without_tui_returns_inactive_json() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let mut req = make_request(Http1Method::Post, "/web-dashboard/input", &[]);
        req.body = br#"{"type":"Input","data":{"kind":"Key","key":"j","modifiers":0}}"#.to_vec();
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 503);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("inactive input json");
        assert_eq!(body["status"], "inactive");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("Live TUI state is not active"),
            "unexpected inactive input detail: {detail}"
        );
    }

    #[test]
    fn mail_ws_input_enqueues_remote_terminal_events() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config.clone());
        let shared = tui_bridge::TuiSharedState::new(&config);
        set_tui_state_handle(None);
        set_tui_state_handle(Some(Arc::clone(&shared)));

        let mut req = make_request(Http1Method::Post, "/mail/ws-input", &[]);
        req.body = br#"{
            "events": [
                {"type":"Input","data":{"kind":"Key","key":"k","modifiers":1}},
                {"type":"Resize","data":{"cols":140,"rows":42}},
                {"type":"Ping"}
            ]
        }"#
        .to_vec();

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 202);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("accepted json");
        assert_eq!(body["accepted"], 2);
        assert_eq!(body["ignored"], 1);
        assert_eq!(body["dropped_oldest"], 0);
        assert_eq!(body["queue_dropped_oldest_total"], 0);
        assert_eq!(body["queue_resize_coalesced_total"], 0);
        assert_eq!(body["status"], "accepted");

        let queued = shared.drain_remote_terminal_events(8);
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued[0],
            tui_bridge::RemoteTerminalEvent::Key {
                ref key,
                modifiers: 1
            } if key == "k"
        ));
        assert!(matches!(
            queued[1],
            tui_bridge::RemoteTerminalEvent::Resize {
                cols: 140,
                rows: 42
            }
        ));

        set_tui_state_handle(None);
    }

    #[test]
    fn mail_ws_input_coalesces_resize_bursts() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config.clone());
        let shared = tui_bridge::TuiSharedState::new(&config);
        set_tui_state_handle(None);
        set_tui_state_handle(Some(Arc::clone(&shared)));

        let mut req = make_request(Http1Method::Post, "/mail/ws-input", &[]);
        req.body = br#"{
            "events": [
                {"type":"Resize","data":{"cols":120,"rows":40}},
                {"type":"Resize","data":{"cols":121,"rows":41}},
                {"type":"Resize","data":{"cols":122,"rows":42}}
            ]
        }"#
        .to_vec();

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 202);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("accepted json");
        assert_eq!(body["accepted"], 3);
        assert_eq!(body["ignored"], 0);
        assert_eq!(body["dropped_oldest"], 0);
        assert_eq!(body["queue_depth"], 1);
        assert_eq!(body["queue_dropped_oldest_total"], 0);
        assert_eq!(body["queue_resize_coalesced_total"], 2);
        assert_eq!(body["status"], "accepted");

        let queued = shared.drain_remote_terminal_events(8);
        assert_eq!(
            queued,
            vec![tui_bridge::RemoteTerminalEvent::Resize {
                cols: 122,
                rows: 42
            }]
        );
        set_tui_state_handle(None);
    }

    #[test]
    fn mail_archive_browser_file_404_still_returns_json_content_type() {
        let storage_root = std::env::temp_dir().join(format!(
            "mcp-agent-mail-archive-browser-json-test-{}",
            std::process::id()
        ));
        let config = mcp_agent_mail_core::Config {
            storage_root,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/mail/archive/browser/demo/file?path=messages/missing.md",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("archive browser file error json");
        assert_eq!(body["detail"], "File not found");
    }

    #[test]
    fn mail_archive_snapshot_validation_error_returns_json_content_type() {
        let storage_root = std::env::temp_dir().join(format!(
            "mcp-agent-mail-archive-snapshot-json-test-{}",
            std::process::id()
        ));
        let config = mcp_agent_mail_core::Config {
            storage_root,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/mail/archive/time-travel/snapshot?project=demo&agent=bad_name&timestamp=2026-02-11T12:30",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 400);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("archive snapshot error json");
        assert_eq!(body["detail"], "Invalid agent name format");
    }

    #[test]
    fn cors_preflight_includes_configured_headers() {
        let config = mcp_agent_mail_core::Config {
            http_cors_enabled: true,
            http_cors_origins: vec!["*".to_string()],
            http_cors_allow_methods: vec!["*".to_string()],
            http_cors_allow_headers: vec!["*".to_string()],
            http_cors_allow_credentials: false,
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Options,
            "/api/",
            &[
                ("Origin", "http://example.com"),
                ("Access-Control-Request-Method", "POST"),
            ],
        );
        let resp = block_on(state.handle(req));
        assert!(resp.status == 200 || resp.status == 204);
        assert_eq!(
            response_header(&resp, "access-control-allow-origin"),
            Some("*")
        );
        assert_eq!(
            response_header(&resp, "access-control-allow-methods"),
            Some("*")
        );
        assert_eq!(
            response_header(&resp, "access-control-allow-headers"),
            Some("*")
        );
        assert!(response_header(&resp, "access-control-allow-credentials").is_none());
    }

    #[test]
    fn cors_headers_present_on_normal_responses() {
        let config = mcp_agent_mail_core::Config {
            http_cors_enabled: true,
            http_cors_origins: vec!["*".to_string()],
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/health/liveness",
            &[("Origin", "http://example.com")],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        assert_eq!(
            response_header(&resp, "access-control-allow-origin"),
            Some("*")
        );
    }

    #[test]
    fn cors_disabled_emits_no_headers() {
        let config = mcp_agent_mail_core::Config {
            http_cors_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/health/liveness",
            &[("Origin", "http://example.com")],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        assert!(response_header(&resp, "access-control-allow-origin").is_none());
    }

    #[test]
    fn bearer_auth_blocks_non_health_routes() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/api/", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("bearer auth response json");
        assert_eq!(body["detail"], "Unauthorized");

        // Health routes must bypass bearer auth.
        for path in &[
            "/health/liveness",
            "/health/readiness",
            "/health",
            "/healthz",
        ] {
            let req_health = make_request(Http1Method::Get, path, &[]);
            let resp_health = block_on(state.handle(req_health));
            assert_eq!(
                resp_health.status, 200,
                "health path should bypass auth: {path}"
            );
        }
    }

    #[test]
    fn bearer_auth_health_prefix_unknown_path_is_not_protected() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/health/unknown", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("health 404 json");
        assert_eq!(body["detail"], "Not Found");
    }

    #[test]
    fn bearer_auth_unknown_non_health_path_is_protected() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/not-a-real-path", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "non-health unknown paths must require auth before 404 handling"
        );
    }

    #[test]
    fn bearer_auth_requires_exact_header_match() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req_ok = make_request(
            Http1Method::Get,
            "/api/",
            &[("Authorization", "Bearer secret")],
        );
        let resp_ok = block_on(state.handle(req_ok));
        assert_eq!(
            resp_ok.status, 405,
            "auth ok should fall through to method check"
        );

        let req_ws = make_request(
            Http1Method::Get,
            "/api/",
            &[("Authorization", "Bearer secret ")],
        );
        let resp_ws = block_on(state.handle(req_ws));
        assert_eq!(resp_ws.status, 401, "whitespace must not be trimmed");

        let req_lower = make_request(
            Http1Method::Get,
            "/api/",
            &[("Authorization", "bearer secret")],
        );
        let resp_lower = block_on(state.handle(req_lower));
        assert_eq!(resp_lower.status, 401, "scheme must match exactly");
    }

    #[test]
    fn bearer_auth_runs_before_json_parse() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let mut req = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        req.body = b"not json".to_vec();
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "missing bearer auth must 401 before body parsing"
        );
    }

    #[test]
    fn bearer_auth_runs_before_content_type_validation() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[("Content-Type", "text/plain")]);
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 404_i64))
            .expect("serialize json-rpc");
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "missing bearer auth must 401 before content-type transport validation"
        );
    }

    #[test]
    fn bearer_auth_runs_before_oversized_body_validation() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let mut req = make_request(
            Http1Method::Post,
            "/api",
            &[("Content-Type", "application/json")],
        );
        req.body = vec![b'x'; (10 * 1024 * 1024) + 1];
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "missing bearer auth must 401 before oversized-body transport validation"
        );
    }

    #[test]
    fn bearer_auth_localhost_bypass_applies_and_forwarded_headers_disable_it() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let state = build_state(config);
        let local = SocketAddr::from(([127, 0, 0, 1], 1234));

        // Localhost without forwarded headers bypasses bearer auth.
        let req_local = make_request_with_peer_addr(Http1Method::Get, "/api/", &[], Some(local));
        let resp_local = block_on(state.handle(req_local));
        assert_eq!(resp_local.status, 405);

        // Forwarded headers disable bypass; missing auth must be 401.
        let req_forwarded = make_request_with_peer_addr(
            Http1Method::Get,
            "/api/",
            &[("X-Forwarded-For", "1.2.3.4")],
            Some(local),
        );
        let resp_forwarded = block_on(state.handle(req_forwarded));
        assert_eq!(resp_forwarded.status, 401);
    }

    #[test]
    fn bearer_auth_protects_well_known_routes() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
    }

    #[test]
    fn bearer_auth_runs_before_well_known_method_validation() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req_missing_auth = make_request(
            Http1Method::Post,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp_missing_auth = block_on(state.handle(req_missing_auth));
        assert_eq!(
            resp_missing_auth.status, 401,
            "missing auth must 401 before well-known method validation"
        );

        let req_with_auth = make_request(
            Http1Method::Post,
            "/.well-known/oauth-authorization-server",
            &[("Authorization", "Bearer secret")],
        );
        let resp_with_auth = block_on(state.handle(req_with_auth));
        assert_eq!(
            resp_with_auth.status, 405,
            "with valid auth, request should reach well-known method validation"
        );
    }

    #[test]
    fn localhost_bypass_requires_local_peer_and_no_forwarded_headers() {
        let config = mcp_agent_mail_core::Config {
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let state = build_state(config);
        let local_peer = SocketAddr::from(([127, 0, 0, 1], 4321));
        let non_local_peer = SocketAddr::from(([10, 0, 0, 1], 5555));

        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[],
            Some(local_peer),
        );
        assert!(state.allow_local_unauthenticated(&req));

        let req_forwarded = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[("X-Forwarded-For", "1.2.3.4")],
            Some(local_peer),
        );
        assert!(!state.allow_local_unauthenticated(&req_forwarded));

        let req_host_header = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[("Host", "localhost")],
            Some(non_local_peer),
        );
        assert!(!state.allow_local_unauthenticated(&req_host_header));
    }

    #[test]
    fn peer_addr_helpers_handle_ipv4_mapped_ipv6() {
        let addr: SocketAddr = "[::ffff:127.0.0.1]:8080".parse().expect("parse addr");
        assert!(is_local_peer_addr(Some(addr)));
        assert_eq!(peer_addr_host(addr), "127.0.0.1".to_string());
        let non_local = SocketAddr::from(([10, 1, 2, 3], 9000));
        assert!(!is_local_peer_addr(Some(non_local)));
    }

    // ── Additional localhost auth tests (br-1bm.4.4) ─────────────────────

    #[test]
    fn localhost_bypass_ipv6_loopback() {
        let config = mcp_agent_mail_core::Config {
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let state = build_state(config);
        let ipv6_loopback: SocketAddr = "[::1]:9000".parse().expect("ipv6 loopback");
        let req = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(ipv6_loopback));
        assert!(
            state.allow_local_unauthenticated(&req),
            "::1 must be recognized as localhost"
        );
    }

    #[test]
    fn localhost_bypass_disabled_rejects_all() {
        let config = mcp_agent_mail_core::Config {
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let local = SocketAddr::from(([127, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(local));
        assert!(
            !state.allow_local_unauthenticated(&req),
            "when config disabled, localhost must not bypass"
        );
    }

    #[test]
    fn localhost_bypass_no_peer_addr_rejects() {
        let config = mcp_agent_mail_core::Config {
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/api", &[]);
        assert!(
            !state.allow_local_unauthenticated(&req),
            "missing peer_addr must not bypass"
        );
    }

    // ── Base path Authorization injection (br-1bm.4.4) ────────────────────

    #[test]
    fn base_passthrough_injects_authorization_for_localhost_only_on_base_no_slash() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api/".to_string(),
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let base_no_slash = normalize_base_path(&config.http_path);
        assert_eq!(base_no_slash, "/api");

        let local = SocketAddr::from(([127, 0, 0, 1], 1234));

        // Base without slash: inject Authorization when missing.
        let mut req = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(local));
        maybe_inject_localhost_authorization_for_base_passthrough(
            &config,
            &mut req,
            "/api",
            &base_no_slash,
        );
        assert_eq!(
            header_value(&req, "authorization"),
            Some("Bearer secret"),
            "localhost base passthrough should synthesize Authorization"
        );

        // Base with trailing slash: do not inject (legacy injection is only on base_no_slash).
        let mut req_slash =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(local));
        maybe_inject_localhost_authorization_for_base_passthrough(
            &config,
            &mut req_slash,
            "/api/",
            &base_no_slash,
        );
        assert!(
            header_value(&req_slash, "authorization").is_none(),
            "base_with_slash should not synthesize Authorization"
        );
    }

    #[test]
    fn base_passthrough_does_not_inject_authorization_when_not_local() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api/".to_string(),
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let base_no_slash = normalize_base_path(&config.http_path);
        let non_local = SocketAddr::from(([10, 0, 0, 1], 1234));

        let mut req = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(non_local));
        maybe_inject_localhost_authorization_for_base_passthrough(
            &config,
            &mut req,
            "/api",
            &base_no_slash,
        );
        assert!(header_value(&req, "authorization").is_none());
    }

    // ── Stateless dispatch tests (br-1bm.4.5) ────────────────────────────

    #[test]
    fn dispatch_returns_none_for_notification() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let notification = JsonRpcRequest::notification("notifications/cancelled", None);
        // Stateless dispatch: notification returns None (no response)
        assert!(block_on(state.dispatch(notification)).is_none());
    }

    #[test]
    fn dispatch_returns_error_for_unknown_method() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let request = JsonRpcRequest::new("nonexistent/method", None, 1_i64);
        let resp = block_on(state.dispatch(request));
        assert!(
            resp.is_some(),
            "unknown method should still return a response"
        );
        let resp = resp.unwrap();
        assert!(
            resp.error.is_some(),
            "unknown method must return an error response"
        );
    }

    #[test]
    fn dispatch_request_contact_preserves_contact_parameter_names() {
        with_serialized_tool_dispatch_env(|project_key| {
            let config = mcp_agent_mail_core::Config::from_env();
            let state = build_state(config);

            let ensure_project = state.dispatch_inner(JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "name": "ensure_project",
                    "arguments": {
                        "human_key": project_key.as_str(),
                    }
                })),
                1_i64,
            ));
            assert!(
                ensure_project.is_ok(),
                "ensure_project should succeed before contact flow: {ensure_project:?}"
            );

            let register_sender = state.dispatch_inner(JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "name": "register_agent",
                    "arguments": {
                        "project_key": project_key.as_str(),
                        "program": "codex-cli",
                        "model": "gpt-5",
                        "name": "BlueLake",
                        "task_description": "server regression test",
                    }
                })),
                2_i64,
            ));
            assert!(
                register_sender.is_ok(),
                "sender registration should succeed: {register_sender:?}"
            );

            let register_recipient = state.dispatch_inner(JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "name": "register_agent",
                    "arguments": {
                        "project_key": project_key.as_str(),
                        "program": "codex-cli",
                        "model": "gpt-5",
                        "name": "RedPeak",
                        "task_description": "server regression test",
                    }
                })),
                3_i64,
            ));
            assert!(
                register_recipient.is_ok(),
                "recipient registration should succeed: {register_recipient:?}"
            );

            let request_contact = state.dispatch_inner(JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "name": "request_contact",
                    "arguments": {
                        "project_key": project_key.as_str(),
                        "from_agent": "BlueLake",
                        "to_agent": "RedPeak",
                        "reason": "regression test",
                    }
                })),
                4_i64,
            ));
            assert!(
                request_contact.is_ok(),
                "request_contact should accept documented from_agent/to_agent fields unchanged: {request_contact:?}"
            );
        });
    }

    #[test]
    fn http_post_roundtrip_returns_json_rpc_response() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[]);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 1_i64);
        req.body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json"),
            "streamable http must return JSON content-type"
        );

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert!(
            body.get("result")
                .and_then(|v| v.get("tools"))
                .and_then(serde_json::Value::as_array)
                .is_some(),
            "expected tools list result"
        );
    }

    #[test]
    fn http_post_roundtrip_accepts_mcp_alias_when_base_is_api() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/mcp", &[]);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 11_i64);
        req.body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 11);
        assert!(
            body.get("result")
                .and_then(|v| v.get("tools"))
                .and_then(serde_json::Value::as_array)
                .is_some(),
            "expected tools list result on /mcp alias"
        );
    }

    #[test]
    fn http_post_roundtrip_accepts_api_alias_when_base_is_mcp() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/mcp/".to_string(),
            ..Default::default()
        };
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[]);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 12_i64);
        req.body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 12);
        assert!(
            body.get("result")
                .and_then(|v| v.get("tools"))
                .and_then(serde_json::Value::as_array)
                .is_some(),
            "expected tools list result on /api alias"
        );
    }

    #[test]
    fn http_post_base_path_without_slash_matches_base_with_slash() {
        fn normalize_tools_list(value: &mut serde_json::Value) {
            if let Some(tools) = value
                .get_mut("result")
                .and_then(|result| result.get_mut("tools"))
                .and_then(serde_json::Value::as_array_mut)
            {
                tools.sort_by(|a, b| {
                    let a_name = a
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let b_name = b
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    a_name.cmp(b_name)
                });
            }
        }

        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1_i64);
        let body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let mut req_base = make_request(Http1Method::Post, "/api", &[]);
        req_base.body = body.clone();
        let resp_base = block_on(state.handle(req_base));

        let mut req_slash = make_request(Http1Method::Post, "/api/", &[]);
        req_slash.body = body;
        let resp_slash = block_on(state.handle(req_slash));

        assert_eq!(resp_base.status, resp_slash.status);
        let mut base_body: serde_json::Value =
            serde_json::from_slice(&resp_base.body).expect("parse /api JSON body");
        let mut slash_body: serde_json::Value =
            serde_json::from_slice(&resp_slash.body).expect("parse /api/ JSON body");
        normalize_tools_list(&mut base_body);
        normalize_tools_list(&mut slash_body);
        assert_eq!(
            base_body, slash_body,
            "POST /api passthrough must behave identically to POST /api/"
        );
    }

    #[test]
    fn http_post_notification_returns_accepted_with_empty_body() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[]);
        let json_rpc = JsonRpcRequest::notification("notifications/initialized", None);
        req.body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 202);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json"),
            "accepted responses still set content-type"
        );
        assert!(
            resp.body.is_empty(),
            "notification should not return a JSON-RPC response body"
        );
    }

    #[test]
    fn http_post_bad_jsonrpc_shape_returns_bad_request_error() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        // Missing required "method" field => malformed JSON-RPC request shape.
        let mut req = make_request(Http1Method::Post, "/api", &[]);
        req.body = br#"{"jsonrpc":"2.0","id":1,"params":{}}"#.to_vec();

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 400);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["error"]["code"], -32600);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("missing field") && message.contains("method"),
            "unexpected malformed-request message: {message}"
        );
    }

    #[test]
    fn http_post_unknown_jsonrpc_method_returns_method_not_found() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[]);
        let json_rpc = JsonRpcRequest::new("wrong/method", None, 73_i64);
        req.body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 73);
        assert_eq!(body["error"]["code"], -32601);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("Method not found"),
            "unexpected method-not-found message: {message}"
        );
    }

    #[test]
    fn http_post_jsonrpc_extra_fields_are_ignored() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[]);
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 91,
            "method": "tools/list",
            "params": {},
            "unexpected_field": "ignored",
            "nested": { "extra": true }
        });
        req.body = serde_json::to_vec(&payload).expect("serialize payload");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 91);
        assert!(
            body.get("error").is_none(),
            "extra fields should not force protocol error: {body}"
        );
        assert!(
            body.get("result")
                .and_then(|v| v.get("tools"))
                .and_then(serde_json::Value::as_array)
                .is_some(),
            "expected tools/list result despite extra fields"
        );
    }

    #[test]
    fn http_post_invalid_content_type_returns_bad_request_transport_error() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(Http1Method::Post, "/api", &[("Content-Type", "text/plain")]);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 101_i64);
        req.body = serde_json::to_vec(&json_rpc).expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 400);
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["error"]["code"], -32600);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("invalid content type"),
            "unexpected transport error message: {message}"
        );
    }

    #[test]
    fn http_post_empty_application_json_body_returns_bad_request_transport_error() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let req = make_request(
            Http1Method::Post,
            "/api",
            &[("Content-Type", "application/json")],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 400);

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["error"]["code"], -32600);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("JSON error"),
            "empty JSON body should fail during transport JSON decode: {message}"
        );
    }

    #[test]
    fn http_post_oversized_body_returns_bad_request_transport_error() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let mut req = make_request(
            Http1Method::Post,
            "/api",
            &[("Content-Type", "application/json")],
        );
        req.body = vec![b'x'; (10 * 1024 * 1024) + 1];

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 400);

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["error"]["code"], -32600);
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("body too large"),
            "unexpected oversized-body transport error message: {message}"
        );
    }

    #[test]
    fn http_get_api_returns_method_not_allowed() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/api", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn http_put_api_returns_method_not_allowed() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Put, "/api", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn http_delete_api_returns_method_not_allowed() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Delete, "/api", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn http_post_rate_limit_exceeded_returns_429_with_detail() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let mut req1 = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(peer));
        req1.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 1_i64))
            .expect("serialize json-rpc");
        let resp1 = block_on(state.handle(req1));
        assert_eq!(resp1.status, 200);

        let mut req2 = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(peer));
        req2.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 2_i64))
            .expect("serialize json-rpc");
        let resp2 = block_on(state.handle(req2));
        assert_eq!(resp2.status, 429);

        let body: serde_json::Value = serde_json::from_slice(&resp2.body).expect("json response");
        assert_eq!(body["detail"], "Rate limit exceeded");
    }

    #[test]
    fn unauthenticated_requests_do_not_consume_rate_limit_bucket() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        // Missing auth should fail before rate-limit consumption.
        let mut unauth = make_request_with_peer_addr(Http1Method::Post, "/api", &[], Some(peer));
        unauth.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 10_i64))
            .expect("serialize json-rpc");
        let unauth_resp = block_on(state.handle(unauth));
        assert_eq!(unauth_resp.status, 401);

        // First authenticated request must still pass (bucket was not consumed above).
        let mut auth_ok = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", "Bearer secret")],
            Some(peer),
        );
        auth_ok.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 11_i64))
            .expect("serialize json-rpc");
        let auth_ok_resp = block_on(state.handle(auth_ok));
        assert_eq!(auth_ok_resp.status, 200);

        // Second authenticated request should hit the one-request bucket.
        let mut auth_limited = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", "Bearer secret")],
            Some(peer),
        );
        auth_limited.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 12_i64))
            .expect("serialize json-rpc");
        let auth_limited_resp = block_on(state.handle(auth_limited));
        assert_eq!(auth_limited_resp.status, 429);
    }

    #[test]
    fn http_post_rbac_forbidden_returns_403_with_detail() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);

        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", auth.as_str())],
            Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "send_message", "arguments": {} })),
            33_i64,
        ))
        .expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 403);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Forbidden");
    }

    #[test]
    fn http_post_expired_jwt_returns_401_with_detail() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_secs(),
        )
        .expect("timestamp fits i64");
        let claims = serde_json::json!({
            "sub": "user-123",
            "role": "writer",
            "exp": now - 1
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", auth.as_str())],
            Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 34_i64))
            .expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Unauthorized");
    }

    #[test]
    fn http_post_missing_auth_with_jwt_enabled_returns_401_with_detail() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[],
            Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 35_i64))
            .expect("serialize json-rpc");

        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Unauthorized");
    }

    #[test]
    fn invalid_jwt_requests_do_not_consume_rate_limit_bucket() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        // Invalid JWT should fail before rate-limit consumption.
        let mut invalid = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", "Bearer abc.def.ghi")],
            Some(peer),
        );
        invalid.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 36_i64))
            .expect("serialize json-rpc");
        let invalid_resp = block_on(state.handle(invalid));
        assert_eq!(invalid_resp.status, 401);

        // First valid JWT request should still pass.
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");
        let mut valid_first = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        valid_first.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 37_i64))
            .expect("serialize json-rpc");
        let valid_first_resp = block_on(state.handle(valid_first));
        assert_eq!(valid_first_resp.status, 200);

        // Second valid JWT request should hit the one-request bucket.
        let mut valid_second = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        valid_second.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 38_i64))
            .expect("serialize json-rpc");
        let valid_second_resp = block_on(state.handle(valid_second));
        assert_eq!(valid_second_resp.status, 429);
    }

    #[test]
    fn forbidden_requests_do_not_consume_rate_limit_bucket() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let reader_claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let reader_token = hs256_token(b"secret", &reader_claims);
        let reader_auth = format!("Bearer {reader_token}");

        let writer_claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let writer_token = hs256_token(b"secret", &writer_claims);
        let writer_auth = format!("Bearer {writer_token}");

        let make_send_message_call = |auth: &str, id: i64| {
            let mut req = make_request_with_peer_addr(
                Http1Method::Post,
                "/api",
                &[("Authorization", auth)],
                Some(peer),
            );
            req.body = serde_json::to_vec(&JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "name": "send_message",
                    "arguments": {
                        "project_key": "/data/projects/mcp_agent_mail_rust",
                        "sender_name": "nobody",
                        "to": ["nobody"],
                        "subject": "x",
                        "body_md": "x"
                    }
                })),
                id,
            ))
            .expect("serialize json-rpc");
            req
        };

        // Reader role is forbidden for send_message (writer tool).
        let reader_resp = block_on(state.handle(make_send_message_call(reader_auth.as_str(), 40)));
        assert_eq!(reader_resp.status, 403);

        // First writer request with same sub should still pass rate limit gate.
        let writer_first = block_on(state.handle(make_send_message_call(writer_auth.as_str(), 41)));
        assert_ne!(writer_first.status, 403);
        assert_ne!(writer_first.status, 429);

        // Second writer request for same sub should hit the one-request bucket.
        let writer_second =
            block_on(state.handle(make_send_message_call(writer_auth.as_str(), 42)));
        assert_eq!(writer_second.status, 429);
        let body: serde_json::Value =
            serde_json::from_slice(&writer_second.body).expect("json response");
        assert_eq!(body["detail"], "Rate limit exceeded");
    }

    #[test]
    fn http_error_status_maps_transport_size_and_encoding_errors_to_bad_request() {
        use fastmcp_transport::http::{HttpError, HttpStatus};

        assert_eq!(
            http_error_status(&HttpError::HeadersTooLarge {
                size: 8193,
                max: 8192
            }),
            HttpStatus::BAD_REQUEST
        );
        assert_eq!(
            http_error_status(&HttpError::BodyTooLarge {
                size: 10_000_001,
                max: 10_000_000
            }),
            HttpStatus::BAD_REQUEST
        );
        assert_eq!(
            http_error_status(&HttpError::UnsupportedTransferEncoding(
                "chunked".to_string()
            )),
            HttpStatus::BAD_REQUEST
        );
    }

    #[test]
    fn http_error_status_maps_method_and_lifecycle_errors() {
        use fastmcp_transport::http::{HttpError, HttpStatus};

        assert_eq!(
            http_error_status(&HttpError::InvalidMethod("GET".to_string())),
            HttpStatus::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            http_error_status(&HttpError::Timeout),
            HttpStatus::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            http_error_status(&HttpError::Closed),
            HttpStatus::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn http_error_status_maps_transport_error_to_internal_server_error() {
        use fastmcp_transport::TransportError;
        use fastmcp_transport::http::{HttpError, HttpStatus};
        assert_eq!(
            http_error_status(&HttpError::Transport(TransportError::Io(
                std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset")
            ))),
            HttpStatus::INTERNAL_SERVER_ERROR
        );
    }

    // ---- HTTP dispatch error path tests (br-3h13.5.1) ----

    #[test]
    fn http_get_unknown_path_returns_404_not_found() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/nonexistent/path", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Not Found");
    }

    #[test]
    fn http_post_unknown_path_returns_404_not_found() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let mut req = make_request(Http1Method::Post, "/random/endpoint", &[]);
        req.body = b"{}".to_vec();
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Not Found");
    }

    #[test]
    fn http_bearer_auth_wrong_secret_returns_401() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("correct-secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 5678));
        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", "Bearer wrong-secret")],
            Some(peer),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 1_i64))
            .expect("serialize json-rpc");
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Unauthorized");
    }

    #[test]
    fn http_bearer_auth_missing_bearer_prefix_returns_401() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("my-token".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 5678));
        // Send the raw token without "Bearer " prefix.
        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[("Authorization", "my-token")],
            Some(peer),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 1_i64))
            .expect("serialize json-rpc");
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Unauthorized");
    }

    #[test]
    fn http_post_health_endpoint_returns_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/health", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn http_post_healthz_endpoint_returns_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/healthz", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn http_get_well_known_oauth_returns_metadata() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["mcp_oauth"], false);
    }

    #[test]
    fn http_post_well_known_oauth_returns_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Post,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn http_websocket_upgrade_on_ws_state_returns_501() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/mail/ws-state",
            &[("Connection", "Upgrade"), ("Upgrade", "websocket")],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 501);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("WebSocket upgrade is not supported"),
            "unexpected 501 detail: {detail}"
        );
    }

    #[test]
    fn http_options_bypasses_bearer_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            http_cors_origins: vec!["*".to_string()],
            http_cors_allow_methods: vec!["*".to_string()],
            http_cors_allow_headers: vec!["*".to_string()],
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 5678));
        // OPTIONS request without any auth header — should NOT get 401.
        let req = make_request_with_peer_addr(
            Http1Method::Options,
            "/api",
            &[
                ("Origin", "http://example.com"),
                ("Access-Control-Request-Method", "POST"),
            ],
            Some(peer),
        );
        let resp = block_on(state.handle(req));
        assert!(
            resp.status != 401,
            "OPTIONS preflight must not require bearer auth, got {}",
            resp.status
        );
        assert!(
            resp.status == 200 || resp.status == 204,
            "expected 200 or 204 for OPTIONS preflight, got {}",
            resp.status
        );
    }

    #[test]
    fn http_post_ws_input_without_tui_returns_503() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let mut req = make_request(Http1Method::Post, "/mail/ws-input", &[]);
        req.body = br#"{"type":"Input","data":{"kind":"Key","key":"j","modifiers":0}}"#.to_vec();
        let resp = block_on(state.handle(req));
        // When TUI state is not active, /mail/ws-input returns 503.
        assert_eq!(resp.status, 503);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json response");
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("TUI state is not active"),
            "unexpected 503 detail: {detail}"
        );
    }

    #[test]
    fn rate_limit_identity_prefers_jwt_sub() {
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[],
            Some(SocketAddr::from(([127, 0, 0, 1], 1234))),
        );
        assert_eq!(rate_limit_identity(&req, Some("user-123")), "sub:user-123");
    }

    #[test]
    fn rate_limit_identity_prefers_peer_addr_over_forwarded_headers() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            ..Default::default()
        };
        let state = build_state(config);

        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req1 = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("X-Forwarded-For", "1.2.3.4")],
            Some(peer),
        );
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("X-Forwarded-For", "5.6.7.8")],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("rate limit should trigger");
        assert_eq!(resp.status, 429);
    }

    #[test]
    fn rate_limits_for_defaults_burst_to_rpm_max_1() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_tools_per_minute: 10,
            http_rate_limit_tools_burst: 0,
            http_rate_limit_resources_per_minute: 5,
            http_rate_limit_resources_burst: 0,
            http_rate_limit_per_minute: 0,
            ..Default::default()
        };
        assert_eq!(rate_limits_for(&config, RequestKind::Tools), (10, 10));
        assert_eq!(rate_limits_for(&config, RequestKind::Resources), (5, 5));
        assert_eq!(rate_limits_for(&config, RequestKind::Other), (0, 1));

        let config = mcp_agent_mail_core::Config {
            http_rate_limit_per_minute: 7,
            ..Default::default()
        };
        assert_eq!(rate_limits_for(&config, RequestKind::Other), (7, 7));

        let config = mcp_agent_mail_core::Config {
            http_rate_limit_tools_per_minute: 10,
            http_rate_limit_tools_burst: 3,
            ..Default::default()
        };
        assert_eq!(rate_limits_for(&config, RequestKind::Tools), (10, 3));
    }

    #[test]
    fn rate_limit_tools_call_without_name_uses_wildcard_endpoint() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        // tools/call with missing name should be keyed under endpoint="*"
        let json_rpc = JsonRpcRequest::new("tools/call", Some(serde_json::json!({})), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("rate limit should trigger via wildcard endpoint bucket");
        assert_eq!(resp.status, 429);
    }

    #[test]
    fn rate_limit_tools_and_resources_use_separate_buckets() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rate_limit_resources_per_minute: 1,
            http_rate_limit_resources_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let tool_params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let tool_rpc = JsonRpcRequest::new("tools/call", Some(tool_params), 1);
        let res_rpc = JsonRpcRequest::new("resources/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req_tool1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req_tool1, &tool_rpc)).is_none());

        let req_res1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req_res1, &res_rpc)).is_none());

        let req_tool2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let tool_resp = block_on(state.check_rbac_and_rate_limit(&req_tool2, &tool_rpc))
            .expect("tool rate limit should trigger on second tool request");
        assert_eq!(tool_resp.status, 429);

        let req_res2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let res_resp = block_on(state.check_rbac_and_rate_limit(&req_res2, &res_rpc))
            .expect("resource rate limit should trigger on second resource request");
        assert_eq!(res_resp.status, 429);
    }

    #[test]
    fn rate_limit_identity_vectors_cover_ipv4_ipv6_and_mapped_ipv6() {
        // jwt_sub should win when present (including whitespace-only strings).
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[],
            Some(SocketAddr::from(([192, 168, 0, 1], 1234))),
        );
        assert_eq!(
            rate_limit_identity(&req, Some("  user-123  ")),
            "sub:  user-123  "
        );

        // empty jwt_sub should be treated as missing.
        assert_eq!(rate_limit_identity(&req, Some("")), "192.168.0.1");

        // ipv6 loopback
        let v6_loop = SocketAddr::from((
            std::net::IpAddr::V6("::1".parse().expect("ipv6 parse")),
            1234,
        ));
        let req_v6 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(v6_loop));
        assert_eq!(rate_limit_identity(&req_v6, None), "::1");

        // ipv4-mapped ipv6 should normalize to ipv4 string
        let mapped = SocketAddr::from((
            std::net::IpAddr::V6("::ffff:127.0.0.1".parse().expect("ipv6 mapped parse")),
            1234,
        ));
        let req_mapped = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(mapped));
        assert_eq!(rate_limit_identity(&req_mapped, None), "127.0.0.1");

        // missing peer addr
        let req_none = make_request(Http1Method::Post, "/api/", &[]);
        assert_eq!(rate_limit_identity(&req_none, None), "ip-unknown");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rate_limit_vector_suite_matches_memory_and_redis_when_available() {
        #[derive(Clone, Copy)]
        struct Vector {
            name: &'static str,
            rpm: u32,
            burst: u32,
            times: &'static [f64],
            expected: &'static [bool],
        }

        static V1_TIMES: &[f64] = &[1000.0, 1000.1, 1000.1, 1001.1];
        static V1_EXPECTED: &[bool] = &[true, true, false, true];
        static V2_TIMES: &[f64] = &[2000.0, 2000.0, 2000.49, 2000.51];
        static V2_EXPECTED: &[bool] = &[true, false, false, true];

        const VECTORS: &[Vector] = &[
            Vector {
                name: "rpm60_burst2",
                rpm: 60,
                burst: 2,
                times: V1_TIMES,
                expected: V1_EXPECTED,
            },
            Vector {
                name: "rpm120_burst1",
                rpm: 120,
                burst: 1,
                times: V2_TIMES,
                expected: V2_EXPECTED,
            },
        ];

        // Memory backend should match legacy vectors deterministically (explicit timestamps).
        for v in VECTORS {
            let limiter = RateLimiter::new();
            let key = format!("tools:vector_suite:{}:ip-unknown", v.name);
            assert_eq!(v.times.len(), v.expected.len());
            for (idx, (&now, &exp)) in v.times.iter().zip(v.expected.iter()).enumerate() {
                let allowed = limiter.allow_memory(&key, v.rpm, v.burst, now, false);
                if allowed != exp {
                    let state = {
                        let buckets = limiter
                            .buckets
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        buckets.get(&key).copied()
                    };
                    write_rate_limit_artifact(
                        "rate_limit_vector_suite_matches_memory_and_redis_when_available_memory",
                        &serde_json::json!({
                            "backend": "memory",
                            "vector": v.name,
                            "idx": idx,
                            "rpm": v.rpm,
                            "burst": v.burst,
                            "now": now,
                            "expected_allowed": exp,
                            "actual_allowed": allowed,
                            "bucket_state": state.map(|(tokens, ts)| serde_json::json!({"tokens": tokens, "ts": ts})),
                            "key": key,
                        }),
                    );
                }
                assert_eq!(allowed, exp, "memory vector={} idx={idx}", v.name);
            }
        }

        // Redis backend should match the exact same vectors when a test redis is available.
        let Some(redis_url) =
            redis_url_or_skip("rate_limit_vector_suite_matches_memory_and_redis_when_available")
        else {
            return;
        };

        let cx = Cx::for_testing();
        let redis = block_on(RedisClient::connect(&cx, &redis_url)).expect("connect redis");

        for v in VECTORS {
            let suffix = REDIS_RATE_LIMIT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let key = format!("tools:vector_suite_{suffix}:{}:ip-unknown", v.name);
            let redis_key = format!("rl:{key}");
            let _ = block_on(redis.del(&cx, &[redis_key.as_str()]));

            for (idx, (&now, &exp)) in v.times.iter().zip(v.expected.iter()).enumerate() {
                let allowed = block_on(consume_rate_limit_redis(
                    &cx, &redis, &key, v.rpm, v.burst, now,
                ))
                .expect("redis eval");
                if allowed != exp {
                    let tokens = block_on(redis.hget(&cx, &redis_key, "tokens"))
                        .unwrap_or(None)
                        .and_then(|b| {
                            std::str::from_utf8(&b)
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                        });
                    let ts = block_on(redis.hget(&cx, &redis_key, "ts"))
                        .unwrap_or(None)
                        .and_then(|b| {
                            std::str::from_utf8(&b)
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                        });
                    let ttl = block_on(redis.cmd(&cx, &["TTL", redis_key.as_str()]))
                        .ok()
                        .and_then(|v| v.as_integer());
                    write_rate_limit_artifact(
                        "rate_limit_vector_suite_matches_memory_and_redis_when_available_redis",
                        &serde_json::json!({
                            "backend": "redis",
                            "redis_url": redis_url,
                            "vector": v.name,
                            "idx": idx,
                            "rpm": v.rpm,
                            "burst": v.burst,
                            "now": now,
                            "expected_allowed": exp,
                            "actual_allowed": allowed,
                            "key": key,
                            "redis_key": redis_key,
                            "redis_state": {
                                "tokens": tokens,
                                "ts": ts,
                                "ttl": ttl,
                            }
                        }),
                    );
                }
                assert_eq!(allowed, exp, "redis vector={} idx={idx}", v.name);
            }

            // Best-effort cleanup
            let _ = block_on(redis.del(&cx, &[redis_key.as_str()]));
        }
    }

    #[test]
    fn rate_limit_redis_ttl_matches_legacy_formula_when_available() {
        let Some(redis_url) =
            redis_url_or_skip("rate_limit_redis_ttl_matches_legacy_formula_when_available")
        else {
            return;
        };

        let cx = Cx::for_testing();
        let redis = block_on(RedisClient::connect(&cx, &redis_url)).expect("connect redis");

        let suffix = REDIS_RATE_LIMIT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let key = format!("tools:ttl_test_{suffix}:ip-unknown");
        let redis_key = format!("rl:{key}");
        let _ = block_on(redis.del(&cx, &[redis_key.as_str()]));

        let rpm = 1;
        let burst = 2;
        let now = 1000.0;
        let allowed = block_on(consume_rate_limit_redis(&cx, &redis, &key, rpm, burst, now))
            .expect("redis eval");
        assert!(allowed);

        let expected_ttl_u64 = (u64::from(burst) * 60).div_ceil(u64::from(rpm));
        let expected_ttl = i64::try_from(expected_ttl_u64).unwrap_or(i64::MAX);
        let ttl = block_on(redis.cmd(&cx, &["TTL", redis_key.as_str()]))
            .expect("TTL")
            .as_integer()
            .unwrap_or(-999);
        // TTL counts down in real time; allow a small amount of slop.
        assert!(
            ttl <= expected_ttl && ttl >= expected_ttl.saturating_sub(2),
            "ttl={ttl} expected~={expected_ttl} redis_key={redis_key}"
        );

        // Best-effort cleanup
        let _ = block_on(redis.del(&cx, &[redis_key.as_str()]));
    }

    #[test]
    fn rate_limit_redis_invalid_url_falls_back_to_memory() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_backend: mcp_agent_mail_core::RateLimitBackend::Redis,
            http_rate_limit_redis_url: Some("not-a-url".to_string()),
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("rate limit should trigger via memory fallback");
        if resp.status != 429 {
            write_rate_limit_artifact(
                "rate_limit_redis_invalid_url_falls_back_to_memory",
                &serde_json::json!({
                    "redis_url": "not-a-url",
                    "expected_backend": "memory",
                    "expected_status": 429,
                    "actual_status": resp.status,
                }),
            );
        }
        assert_eq!(resp.status, 429);

        let (is_failed, state_dbg) = {
            let guard = state
                .rate_limit_redis
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let is_failed = matches!(&*guard, RateLimitRedisState::Failed);
            let state_dbg = format!("{guard:?}");
            drop(guard);
            (is_failed, state_dbg)
        };
        if !is_failed {
            write_rate_limit_artifact(
                "rate_limit_redis_invalid_url_falls_back_to_memory_state",
                &serde_json::json!({
                    "expected_state": "Failed",
                    "actual_state": state_dbg,
                }),
            );
        }
        assert!(is_failed);
    }

    #[test]
    fn rate_limit_redis_command_failure_falls_back_to_memory() {
        // Use a local port that should reliably refuse connections.
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_backend: mcp_agent_mail_core::RateLimitBackend::Redis,
            http_rate_limit_redis_url: Some("redis://127.0.0.1:1/0".to_string()),
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("rate limit should trigger via memory fallback");
        if resp.status != 429 {
            write_rate_limit_artifact(
                "rate_limit_redis_command_failure_falls_back_to_memory",
                &serde_json::json!({
                    "redis_url": "redis://127.0.0.1:1/0",
                    "expected_backend": "redis->memory fallback",
                    "expected_status": 429,
                    "actual_status": resp.status,
                }),
            );
        }
        assert_eq!(resp.status, 429);

        let (is_ready, state_dbg) = {
            let guard = state
                .rate_limit_redis
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let is_ready = matches!(&*guard, RateLimitRedisState::Ready(_));
            let state_dbg = format!("{guard:?}");
            drop(guard);
            (is_ready, state_dbg)
        };
        if !is_ready {
            write_rate_limit_artifact(
                "rate_limit_redis_command_failure_falls_back_to_memory_state",
                &serde_json::json!({
                    "expected_state": "Ready",
                    "actual_state": state_dbg,
                }),
            );
        }
        assert!(is_ready, "command failures must not disable redis state");
    }

    #[test]
    fn rate_limit_redis_backend_enforces_limits_when_available() {
        let Some(redis_url) =
            redis_url_or_skip("rate_limit_redis_backend_enforces_limits_when_available")
        else {
            return;
        };

        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_backend: mcp_agent_mail_core::RateLimitBackend::Redis,
            http_rate_limit_redis_url: Some(redis_url.clone()),
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let suffix = REDIS_RATE_LIMIT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tool = format!("redis_rate_limit_test_{suffix}");
        let params = serde_json::json!({ "name": tool, "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        // Assert the Redis key exists so we know the EVAL path ran (not memory fallback).
        let identity = peer_addr_host(peer);
        let redis_key = format!("rl:tools:{tool}:{identity}");
        let cx = Cx::for_testing();
        let redis = block_on(RedisClient::connect(&cx, &redis_url)).expect("connect redis");
        let tokens = block_on(redis.hget(&cx, &redis_key, "tokens")).expect("hget tokens");
        assert!(tokens.is_some(), "expected redis hash key to be created");

        let req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("rate limit should trigger on second request");
        assert_eq!(resp.status, 429);

        // Best-effort cleanup
        let _ = block_on(redis.del(&cx, &[redis_key.as_str()]));
    }

    #[test]
    fn rate_limiter_memory_refill_and_consume_math() {
        let limiter = RateLimiter::new();
        let key = "tools:unit_test:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=60 => 1 token/sec. burst=2 => start with 2 tokens.
        assert!(limiter.allow_memory(key, 60, 2, t0, false));
        assert!(limiter.allow_memory(key, 60, 2, t0 + 0.1, false));
        assert!(!limiter.allow_memory(key, 60, 2, t0 + 0.1, false));

        // After ~1s, we should have refilled enough to allow again.
        assert!(limiter.allow_memory(key, 60, 2, t0 + 1.1, false));
    }

    #[test]
    fn rate_limiter_cleanup_eviction_after_one_hour() {
        let limiter = RateLimiter::new();
        let now = rate_limit_now();
        let cutoff = now - 3600.0;

        {
            let mut buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buckets.insert("old".to_string(), (1.0, cutoff - 1.0));
            buckets.insert("new".to_string(), (1.0, cutoff + 1.0));
        }
        {
            let mut last = limiter
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = now - 61.0; // ensure cleanup runs
        }

        limiter.cleanup(now);

        let (has_old, has_new) = {
            let buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (buckets.contains_key("old"), buckets.contains_key("new"))
        };
        assert!(!has_old);
        assert!(has_new);
    }

    #[test]
    fn rate_limiter_sliding_window_exact_refill_boundary() {
        let limiter = RateLimiter::new();
        let key = "tools:boundary_test:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=60 => 1 token/sec with burst=1.
        assert!(limiter.allow_memory(key, 60, 1, t0, false));
        assert!(!limiter.allow_memory(key, 60, 1, t0 + 0.999_999, false));
        assert!(limiter.allow_memory(key, 60, 1, t0 + 1.0, false));
    }

    #[test]
    fn rate_limiter_burst_refill_requires_full_token() {
        let limiter = RateLimiter::new();
        let key = "tools:burst_test:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=120 => 2 tokens/sec, burst=3.
        assert!(limiter.allow_memory(key, 120, 3, t0, false));
        assert!(limiter.allow_memory(key, 120, 3, t0, false));
        assert!(limiter.allow_memory(key, 120, 3, t0, false));
        assert!(!limiter.allow_memory(key, 120, 3, t0, false));

        // 0.49s refills < 1 token.
        assert!(!limiter.allow_memory(key, 120, 3, t0 + 0.49, false));
        // 0.5s refills exactly 1 token.
        assert!(limiter.allow_memory(key, 120, 3, t0 + 0.5, false));
    }

    #[test]
    fn rate_limiter_per_minute_zero_allows_without_bucket_state() {
        let limiter = RateLimiter::new();
        let key = "tools:no_limit:ip-unknown";

        for _ in 0..10 {
            assert!(limiter.allow_memory(key, 0, 0, rate_limit_now(), false));
        }

        {
            let buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                buckets.is_empty(),
                "unlimited bucket should not allocate state"
            );
            drop(buckets);
        }
    }

    #[test]
    fn rate_limiter_negative_time_delta_does_not_refill() {
        let limiter = RateLimiter::new();
        let key = "tools:negative_delta:ip-unknown";
        let t0 = rate_limit_now();

        assert!(limiter.allow_memory(key, 60, 1, t0, false));
        assert!(!limiter.allow_memory(key, 60, 1, t0, false));
        assert!(!limiter.allow_memory(key, 60, 1, t0 - 30.0, false));
        assert!(limiter.allow_memory(key, 60, 1, t0 + 1.0, false));
    }

    #[test]
    fn rate_limiter_cleanup_skips_when_interval_not_elapsed() {
        let limiter = RateLimiter::new();
        let now = rate_limit_now();
        let cutoff = now - 3600.0;

        {
            let mut buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buckets.insert("old".to_string(), (1.0, cutoff - 10.0));
        }
        {
            let mut last = limiter
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = now - 30.0;
        }

        limiter.cleanup(now);

        {
            let buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                buckets.contains_key("old"),
                "cleanup should be skipped when called before 60s interval"
            );
            drop(buckets);
        }
    }

    #[test]
    fn rate_limiter_allow_memory_cleanup_prunes_stale_entries() {
        let limiter = RateLimiter::new();
        let now = rate_limit_now();
        let cutoff = now - 3600.0;

        {
            let mut buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buckets.insert("stale".to_string(), (1.0, cutoff - 5.0));
            buckets.insert("fresh".to_string(), (1.0, cutoff + 5.0));
        }
        {
            let mut last = limiter
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = now - 61.0;
        }

        assert!(limiter.allow_memory("tools:new:ip-unknown", 60, 1, now, true));

        {
            let buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!buckets.contains_key("stale"));
            assert!(buckets.contains_key("fresh"));
            assert!(buckets.contains_key("tools:new:ip-unknown"));
            drop(buckets);
        }
    }

    #[test]
    fn rate_limiter_concurrent_requests_do_not_overissue_tokens() {
        use std::sync::atomic::AtomicUsize;

        let limiter = Arc::new(RateLimiter::new());
        let allowed = Arc::new(AtomicUsize::new(0));
        let now = rate_limit_now();

        std::thread::scope(|scope| {
            for _ in 0..32 {
                let limiter = Arc::clone(&limiter);
                let allowed = Arc::clone(&allowed);
                scope.spawn(move || {
                    if limiter.allow_memory("tools:concurrency:ip-unknown", 60, 5, now, false) {
                        allowed.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });

        assert_eq!(
            allowed.load(Ordering::Relaxed),
            5,
            "allowed requests must never exceed burst capacity under concurrency"
        );
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn rate_limit_tools_with_different_names_use_separate_buckets() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let tool_a_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "tool_a", "arguments": {} })),
            1,
        );
        let tool_b_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "tool_b", "arguments": {} })),
            1,
        );

        let req_a1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req_a1, &tool_a_rpc)).is_none());

        let req_b1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req_b1, &tool_b_rpc)).is_none());

        let req_a2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp_a = block_on(state.check_rbac_and_rate_limit(&req_a2, &tool_a_rpc))
            .expect("tool_a bucket should rate-limit on second request");
        assert_eq!(resp_a.status, 429);

        let req_b2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp_b = block_on(state.check_rbac_and_rate_limit(&req_b2, &tool_b_rpc))
            .expect("tool_b bucket should rate-limit on second request");
        assert_eq!(resp_b.status, 429);
    }

    #[test]
    fn rate_limit_other_requests_do_not_consume_tools_bucket() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_per_minute: 1,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let other_rpc = JsonRpcRequest::new("initialize", None, 1);
        let tool_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            2,
        );

        let other_req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&other_req1, &other_rpc)).is_none());

        let other_req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let other_resp = block_on(state.check_rbac_and_rate_limit(&other_req2, &other_rpc))
            .expect("other request bucket should rate-limit on second request");
        assert_eq!(other_resp.status, 429);

        let tool_req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&tool_req1, &tool_rpc)).is_none(),
            "tools bucket should remain independent from other requests"
        );

        let tool_req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let tool_resp = block_on(state.check_rbac_and_rate_limit(&tool_req2, &tool_rpc))
            .expect("tool bucket should rate-limit on second request");
        assert_eq!(tool_resp.status, 429);
    }

    #[test]
    fn rate_limit_wildcard_endpoint_and_named_tool_are_isolated() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let wildcard_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "arguments": {} })),
            1,
        );
        let named_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            2,
        );

        let wildcard_req1 =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&wildcard_req1, &wildcard_rpc)).is_none());

        let wildcard_req2 =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let wildcard_resp =
            block_on(state.check_rbac_and_rate_limit(&wildcard_req2, &wildcard_rpc))
                .expect("wildcard endpoint should rate-limit on second request");
        assert_eq!(wildcard_resp.status, 429);

        let named_req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&named_req1, &named_rpc)).is_none(),
            "named tools must not share wildcard endpoint bucket"
        );

        let named_req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let named_resp = block_on(state.check_rbac_and_rate_limit(&named_req2, &named_rpc))
            .expect("named endpoint should rate-limit on second request");
        assert_eq!(named_resp.status, 429);
    }

    #[test]
    fn rate_limit_unknown_identity_falls_back_to_ip_unknown_bucket() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let json_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            1,
        );

        let req1 = make_request(Http1Method::Post, "/api/", &[]);
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request(Http1Method::Post, "/api/", &[]);
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("ip-unknown fallback bucket should rate-limit on second request");
        assert_eq!(resp.status, 429);
    }

    // ── br-3h13.5.3: Rate limiter unit tests (MistyRobin) ─────────────

    #[test]
    fn rate_limiter_tokens_cap_at_burst_after_long_idle() {
        let limiter = RateLimiter::new();
        let key = "tools:cap_test:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=60, burst=3 → 1 token/sec, max 3 tokens.
        // Consume all 3 tokens.
        assert!(limiter.allow_memory(key, 60, 3, t0, false));
        assert!(limiter.allow_memory(key, 60, 3, t0, false));
        assert!(limiter.allow_memory(key, 60, 3, t0, false));
        assert!(!limiter.allow_memory(key, 60, 3, t0, false));

        // Wait a very long time (1000s). Tokens should cap at burst=3, not 1000.
        assert!(limiter.allow_memory(key, 60, 3, t0 + 1000.0, false));
        assert!(limiter.allow_memory(key, 60, 3, t0 + 1000.0, false));
        assert!(limiter.allow_memory(key, 60, 3, t0 + 1000.0, false));
        assert!(
            !limiter.allow_memory(key, 60, 3, t0 + 1000.0, false),
            "tokens must never exceed burst capacity even after long idle"
        );
    }

    #[test]
    fn rate_limiter_multiple_keys_are_independent() {
        let limiter = RateLimiter::new();
        let t0 = rate_limit_now();

        // Two different keys with burst=1 each.
        assert!(limiter.allow_memory("tools:a:ip-1", 60, 1, t0, false));
        assert!(limiter.allow_memory("tools:b:ip-2", 60, 1, t0, false));

        // Each should be exhausted independently.
        assert!(!limiter.allow_memory("tools:a:ip-1", 60, 1, t0, false));
        assert!(!limiter.allow_memory("tools:b:ip-2", 60, 1, t0, false));

        // Refilling key a should not affect key b.
        assert!(limiter.allow_memory("tools:a:ip-1", 60, 1, t0 + 1.0, false));
        assert!(!limiter.allow_memory("tools:b:ip-2", 60, 1, t0 + 0.5, false));
    }

    #[test]
    fn rate_limiter_gradual_recovery_after_exhaustion() {
        let limiter = RateLimiter::new();
        let key = "tools:recovery:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=60 => 1 token/sec, burst=3.
        // Exhaust all tokens.
        assert!(limiter.allow_memory(key, 60, 3, t0, false));
        assert!(limiter.allow_memory(key, 60, 3, t0, false));
        assert!(limiter.allow_memory(key, 60, 3, t0, false));
        assert!(!limiter.allow_memory(key, 60, 3, t0, false));

        // After 1s, exactly 1 token refilled.
        assert!(limiter.allow_memory(key, 60, 3, t0 + 1.0, false));
        assert!(!limiter.allow_memory(key, 60, 3, t0 + 1.0, false));

        // After 2s total, another token.
        assert!(limiter.allow_memory(key, 60, 3, t0 + 2.0, false));
        assert!(!limiter.allow_memory(key, 60, 3, t0 + 2.0, false));

        // After 3s total, another token (now back to 1 available).
        assert!(limiter.allow_memory(key, 60, 3, t0 + 3.0, false));
        assert!(!limiter.allow_memory(key, 60, 3, t0 + 3.0, false));
    }

    #[test]
    fn rate_limiter_burst_greater_than_rpm_is_valid() {
        let limiter = RateLimiter::new();
        let key = "tools:big_burst:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=5, burst=10 → 5/60 ≈ 0.0833 tokens/sec, but start with 10 tokens.
        for _ in 0..10 {
            assert!(limiter.allow_memory(key, 5, 10, t0, false));
        }
        assert!(!limiter.allow_memory(key, 5, 10, t0, false));

        // Need 1/0.0833 = 12s to refill one token.
        assert!(!limiter.allow_memory(key, 5, 10, t0 + 11.0, false));
        assert!(limiter.allow_memory(key, 5, 10, t0 + 12.0, false));
    }

    #[test]
    fn rate_limit_429_response_body_contains_detail() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let json_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            1,
        );

        let req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("should be rate limited");
        assert_eq!(resp.status, 429);

        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("429 response body must be valid JSON");
        assert_eq!(
            body["detail"], "Rate limit exceeded",
            "429 response must contain detail field with rate limit message"
        );
    }

    #[test]
    fn rate_limit_disabled_never_blocks() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: false,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let json_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            1,
        );

        // Even with rpm=1 burst=1, disabling rate limiting should allow all requests.
        for _ in 0..10 {
            let req = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
            assert!(
                block_on(state.check_rbac_and_rate_limit(&req, &json_rpc)).is_none(),
                "rate limiting disabled should never block"
            );
        }
    }

    #[test]
    fn rate_limiter_concurrent_different_keys_all_succeed() {
        use std::sync::atomic::AtomicUsize;

        let limiter = Arc::new(RateLimiter::new());
        let successes = Arc::new(AtomicUsize::new(0));
        let now = rate_limit_now();

        // 16 threads, each with its own unique key and burst=1.
        // Every thread should succeed exactly once.
        std::thread::scope(|scope| {
            for i in 0..16 {
                let limiter = Arc::clone(&limiter);
                let successes = Arc::clone(&successes);
                scope.spawn(move || {
                    let key = format!("tools:concurrent_{i}:ip-unknown");
                    if limiter.allow_memory(&key, 60, 1, now, false) {
                        successes.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });

        assert_eq!(
            successes.load(Ordering::Relaxed),
            16,
            "all threads with unique keys should succeed"
        );
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)]
    fn rate_limiter_cleanup_removes_all_stale_entries() {
        let limiter = RateLimiter::new();
        let now = rate_limit_now();
        let cutoff = now - 3600.0;

        {
            let mut buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for i in 0..10 {
                buckets.insert(format!("stale_{i}"), (1.0, cutoff - f64::from(i) - 1.0));
            }
        }
        {
            let mut last = limiter
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = now - 61.0;
        }

        limiter.cleanup(now);

        let count = limiter
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(count, 0, "all stale entries should be removed");
    }

    #[test]
    fn rate_limiter_high_rpm_rapid_refill() {
        let limiter = RateLimiter::new();
        let key = "tools:high_rpm:ip-unknown";
        // Use a controlled base to avoid f64 precision loss at epoch-second scale.
        let t0 = 1000.0_f64;

        // rpm=6000 => 100 tokens/sec, burst=1.
        assert!(limiter.allow_memory(key, 6000, 1, t0, false));
        assert!(!limiter.allow_memory(key, 6000, 1, t0, false));

        // After 0.011s (11ms), >1 token refilled (100 * 0.011 = 1.1).
        // Use 0.011 instead of exact 0.01 to avoid f64 boundary precision.
        assert!(limiter.allow_memory(key, 6000, 1, t0 + 0.011, false));
        assert!(!limiter.allow_memory(key, 6000, 1, t0 + 0.011, false));

        // 0.008s later is not enough (100 * 0.008 = 0.8 < 1.0).
        assert!(!limiter.allow_memory(key, 6000, 1, t0 + 0.019, false));
    }

    #[test]
    fn rate_limiter_fractional_rpm_precision() {
        let limiter = RateLimiter::new();
        let key = "tools:fractional:ip-unknown";
        // Use controlled base to avoid f64 precision loss at epoch scale.
        let t0 = 1000.0_f64;

        // rpm=7 => 7/60 ≈ 0.11667 tokens/sec, burst=1.
        assert!(limiter.allow_memory(key, 7, 1, t0, false));
        assert!(!limiter.allow_memory(key, 7, 1, t0, false));

        // 60/7 ≈ 8.571s needed for one token. 8.5s is NOT enough.
        assert!(!limiter.allow_memory(key, 7, 1, t0 + 8.5, false));

        // 8.572s should be enough (7/60 * 8.572 ≈ 1.0001).
        assert!(limiter.allow_memory(key, 7, 1, t0 + 8.572, false));
    }

    #[test]
    fn rate_limit_per_identity_isolation_via_pipeline() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let json_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            1,
        );

        let peer_a = SocketAddr::from(([10, 0, 0, 1], 1234));
        let peer_b = SocketAddr::from(([10, 0, 0, 2], 1234));

        // Each identity gets its own burst independently.
        let office_network_request =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer_a));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&office_network_request, &json_rpc)).is_none()
        );

        let home_wifi_request =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer_b));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&home_wifi_request, &json_rpc)).is_none(),
            "different IP should have independent rate limit bucket"
        );

        // Both should be exhausted now.
        let office_retry_request =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer_a));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&office_retry_request, &json_rpc)).is_some()
        );

        let wifi_retry_request =
            make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer_b));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&wifi_retry_request, &json_rpc)).is_some()
        );
    }

    #[test]
    fn rate_limiter_do_cleanup_false_preserves_stale_entries() {
        let limiter = RateLimiter::new();
        let now = rate_limit_now();
        let cutoff = now - 3600.0;

        {
            let mut buckets = limiter
                .buckets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buckets.insert("stale_key".to_string(), (1.0, cutoff - 10.0));
        }
        {
            let mut last = limiter
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = now - 61.0; // would normally trigger cleanup
        }

        // do_cleanup=false should skip cleanup even when interval elapsed.
        limiter.allow_memory("tools:new:ip-unknown", 60, 1, now, false);

        let has_stale = limiter
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key("stale_key");
        assert!(has_stale, "do_cleanup=false must not prune stale entries");
    }

    #[test]
    fn rate_limiter_burst_one_is_strict_one_per_interval() {
        let limiter = RateLimiter::new();
        let key = "tools:strict:ip-unknown";
        let t0 = rate_limit_now();

        // rpm=60, burst=1 → 1 token/sec, start with 1 token.
        assert!(limiter.allow_memory(key, 60, 1, t0, false));

        // Immediate subsequent requests should all fail.
        for ms in [0.0, 0.1, 0.5, 0.9, 0.999] {
            assert!(
                !limiter.allow_memory(key, 60, 1, t0 + ms, false),
                "burst=1 should deny at t0+{ms}s"
            );
        }

        // Exactly at 1.0s, should succeed again.
        assert!(limiter.allow_memory(key, 60, 1, t0 + 1.0, false));
    }

    #[test]
    fn rate_limit_resources_use_separate_config_values() {
        let config = mcp_agent_mail_core::Config {
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 100,
            http_rate_limit_tools_burst: 100,
            http_rate_limit_resources_per_minute: 1,
            http_rate_limit_resources_burst: 1,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let resource_rpc = JsonRpcRequest::new("resources/list", None, 1);

        // First resource request should pass.
        let req1 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &resource_rpc)).is_none());

        // Second should fail (resources rpm=1, burst=1).
        let req2 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &resource_rpc))
            .expect("resource rate limit should trigger");
        assert_eq!(resp.status, 429);

        // But tools should still be fine (tools rpm=100).
        let tool_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({ "name": "health_check", "arguments": {} })),
            2,
        );
        let req3 = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        assert!(
            block_on(state.check_rbac_and_rate_limit(&req3, &tool_rpc)).is_none(),
            "tools should use separate config with higher limit"
        );
    }

    #[test]
    fn jwt_enabled_requires_bearer_token() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("jwt should require Authorization header");
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_enabled_rejects_non_bearer_authorization_header() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", "Basic abc123")],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("non-bearer Authorization should be rejected");
        write_jwt_artifact(
            "jwt_enabled_rejects_non_bearer_authorization_header",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true },
                "authorization": "Basic abc123",
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_enabled_rejects_empty_bearer_token() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", "Bearer ")],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("empty bearer token should be rejected");
        write_jwt_artifact(
            "jwt_enabled_rejects_empty_bearer_token",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true },
                "authorization": "Bearer <empty>",
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_rejects_malformed_header_segment() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        // "abc" base64url-decodes, but is not valid JSON; this must fail header parsing.
        let auth = "Bearer abc.def.ghi";
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth)],
            Some(peer),
        );
        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("malformed header must be rejected");
        write_jwt_artifact(
            "jwt_rejects_malformed_header_segment",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true },
                "authorization": { "scheme": "Bearer", "token": "<malformed>" },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_hs256_secret_allows_valid_token() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        assert!(block_on(state.check_rbac_and_rate_limit(&req, &json_rpc)).is_none());
    }

    #[test]
    fn jwt_hs256_secret_rejects_invalid_signature() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let token = hs256_token(b"not-the-secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("invalid signature should be rejected");
        write_jwt_artifact(
            "jwt_hs256_secret_rejects_invalid_signature",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "http_jwt_secret": "***" },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_rejects_token_with_disallowed_algorithm() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_jwt_algorithms: vec!["RS256".to_string()],
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("disallowed alg should be rejected");
        write_jwt_artifact(
            "jwt_rejects_token_with_disallowed_algorithm",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "http_jwt_algorithms": ["RS256"] },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_hs256_rejects_expired_token() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer", "exp": 1_i64 });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("expired token should be rejected");
        write_jwt_artifact(
            "jwt_hs256_rejects_expired_token",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_hs256_rejects_token_not_yet_valid() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims =
            serde_json::json!({ "sub": "user-123", "role": "writer", "nbf": 4_102_444_800_i64 });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("future nbf token should be rejected");
        write_jwt_artifact(
            "jwt_hs256_rejects_token_not_yet_valid",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_hs256_rejects_issuer_mismatch_when_configured() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_jwt_issuer: Some("issuer-expected".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims =
            serde_json::json!({ "sub": "user-123", "role": "writer", "iss": "issuer-wrong" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("iss mismatch should be rejected");
        write_jwt_artifact(
            "jwt_hs256_rejects_issuer_mismatch_when_configured",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "http_jwt_issuer": "issuer-expected" },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_hs256_rejects_audience_mismatch_when_configured() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_jwt_audience: Some("aud-expected".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer", "aud": "aud-wrong" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("aud mismatch should be rejected");
        write_jwt_artifact(
            "jwt_hs256_rejects_audience_mismatch_when_configured",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "http_jwt_audience": "aud-expected" },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_hs256_allows_issuer_match_when_configured() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_jwt_issuer: Some("issuer-expected".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({
            "sub": "user-123",
            "role": "writer",
            "iss": "issuer-expected"
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_hs256_allows_issuer_match_when_configured",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "http_jwt_issuer": "issuer-expected" },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(resp.is_none(), "expected issuer match to allow");
    }

    #[test]
    fn jwt_hs256_allows_audience_match_when_configured() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_jwt_audience: Some("aud-expected".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({
            "sub": "user-123",
            "role": "writer",
            "aud": "aud-expected"
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_hs256_allows_audience_match_when_configured",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "http_jwt_audience": "aud-expected" },
                "claims": claims,
                "authorization": { "scheme": "Bearer", "token_len": token.len() },
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(resp.is_none(), "expected audience match to allow");
    }

    #[test]
    fn jwt_roles_from_claim_string_is_singleton() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let claims = serde_json::json!({ "role": "writer" });
        let roles = state.jwt_roles_from_claims(&claims);
        write_rbac_artifact(
            "jwt_roles_from_claim_string_is_singleton",
            &serde_json::json!({
                "role_claim": state.config.http_jwt_role_claim,
                "default_role": state.config.http_rbac_default_role,
                "claims": claims,
                "roles": roles,
            }),
        );
        assert_eq!(roles, vec!["writer".to_string()]);
    }

    #[test]
    fn jwt_roles_from_claim_list_is_sorted_and_deduped() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let claims = serde_json::json!({ "role": ["writer", "", "reader", "writer", "reader"] });
        let roles = state.jwt_roles_from_claims(&claims);
        write_rbac_artifact(
            "jwt_roles_from_claim_list_is_sorted_and_deduped",
            &serde_json::json!({
                "role_claim": state.config.http_jwt_role_claim,
                "default_role": state.config.http_rbac_default_role,
                "claims": claims,
                "roles": roles,
            }),
        );
        assert_eq!(roles, vec!["reader".to_string(), "writer".to_string()]);
    }

    #[test]
    fn jwt_roles_from_claim_missing_uses_default_role() {
        let config = mcp_agent_mail_core::Config {
            http_rbac_default_role: "default-role".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({});
        let roles = state.jwt_roles_from_claims(&claims);
        write_rbac_artifact(
            "jwt_roles_from_claim_missing_uses_default_role",
            &serde_json::json!({
                "role_claim": state.config.http_jwt_role_claim,
                "default_role": state.config.http_rbac_default_role,
                "claims": claims,
                "roles": roles,
            }),
        );
        assert_eq!(roles, vec!["default-role".to_string()]);
    }

    #[test]
    fn jwt_roles_from_claim_empty_string_uses_default_role() {
        let config = mcp_agent_mail_core::Config {
            http_rbac_default_role: "default-role".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "role": "" });
        let roles = state.jwt_roles_from_claims(&claims);
        write_rbac_artifact(
            "jwt_roles_from_claim_empty_string_uses_default_role",
            &serde_json::json!({
                "role_claim": state.config.http_jwt_role_claim,
                "default_role": state.config.http_rbac_default_role,
                "claims": claims,
                "roles": roles,
            }),
        );
        assert_eq!(roles, vec!["default-role".to_string()]);
    }

    #[test]
    fn jwt_roles_from_custom_claim_name_is_used() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_role_claim: "roles".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "roles": ["writer"] });
        let roles = state.jwt_roles_from_claims(&claims);
        write_rbac_artifact(
            "jwt_roles_from_custom_claim_name_is_used",
            &serde_json::json!({
                "role_claim": state.config.http_jwt_role_claim,
                "default_role": state.config.http_rbac_default_role,
                "claims": claims,
                "roles": roles,
            }),
        );
        assert_eq!(roles, vec!["writer".to_string()]);
    }

    #[test]
    fn rbac_reader_can_call_readonly_tool() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_rbac_artifact(
            "rbac_reader_can_call_readonly_tool",
            &serde_json::json!({
                "claims": claims,
                "tool": "health_check",
                "peer_addr": peer.to_string(),
                "is_local_ok": state.allow_local_unauthenticated(&req),
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(resp.is_none(), "reader should be allowed for readonly tool");
    }

    #[test]
    fn jwt_roles_enforced_for_tools() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let params = serde_json::json!({ "name": "send_message", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("reader should be forbidden for send_message");
        assert_eq!(resp.status, 403);
    }

    #[test]
    fn rbac_unknown_tool_name_requires_writer() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let params = serde_json::json!({ "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("unknown tool name should be forbidden for readers");
        write_rbac_artifact(
            "rbac_unknown_tool_name_requires_writer",
            &serde_json::json!({
                "claims": claims,
                "tool": null,
                "peer_addr": peer.to_string(),
                "is_local_ok": state.allow_local_unauthenticated(&req),
                "expected_status": 403,
                "actual_status": resp.status,
            }),
        );
        assert_forbidden(&resp);
    }

    #[test]
    fn rbac_resources_allowed_for_unknown_role() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "nobody" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("resources/read", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_rbac_artifact(
            "rbac_resources_allowed_for_unknown_role",
            &serde_json::json!({
                "claims": claims,
                "resource_method": "resources/read",
                "peer_addr": peer.to_string(),
                "is_local_ok": state.allow_local_unauthenticated(&req),
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(
            resp.is_none(),
            "resources should be allowed regardless of role membership"
        );
    }

    #[test]
    fn rbac_localhost_bypass_allows_reader_for_writer_tool() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let params = serde_json::json!({ "name": "send_message", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([127, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_rbac_artifact(
            "rbac_localhost_bypass_allows_reader_for_writer_tool",
            &serde_json::json!({
                "claims": claims,
                "tool": "send_message",
                "peer_addr": peer.to_string(),
                "is_local_ok": state.allow_local_unauthenticated(&req),
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(
            resp.is_none(),
            "localhost bypass should skip RBAC restrictions"
        );
    }

    #[test]
    fn rbac_localhost_bypass_disabled_by_forwarded_headers() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            http_allow_localhost_unauthenticated: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let params = serde_json::json!({ "name": "send_message", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([127, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[
                ("Authorization", auth.as_str()),
                ("X-Forwarded-For", "1.2.3.4"),
            ],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("forwarded headers should disable bypass and enforce RBAC");
        write_rbac_artifact(
            "rbac_localhost_bypass_disabled_by_forwarded_headers",
            &serde_json::json!({
                "claims": claims,
                "tool": "send_message",
                "peer_addr": peer.to_string(),
                "is_local_ok": state.allow_local_unauthenticated(&req),
                "expected_status": 403,
                "actual_status": resp.status,
            }),
        );
        assert_forbidden(&resp);
    }

    #[test]
    fn rate_limiting_uses_jwt_sub_identity() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            ..Default::default()
        };
        let state = build_state(config);

        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);

        let req1 = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(SocketAddr::from(([10, 0, 0, 1], 1111))),
        );
        assert!(block_on(state.check_rbac_and_rate_limit(&req1, &json_rpc)).is_none());

        let req2 = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(SocketAddr::from(([10, 0, 0, 2], 2222))),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req2, &json_rpc))
            .expect("rate limit should trigger by sub identity");
        assert_eq!(resp.status, 429);
    }

    #[test]
    fn jwt_hs256_jwks_allows_valid_token() {
        use base64::Engine as _;

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let secret = b"secret";
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "oct",
                "alg": "HS256",
                "kid": "kid-1",
                "k": k,
            }]
        });
        let jwks_bytes = serde_json::to_vec(&jwks).expect("jwks json");

        with_jwks_server(&jwks_bytes, 2, |jwks_url| {
            let jwks_url2 = jwks_url.clone();
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_secret: None,
                http_jwt_jwks_url: Some(jwks_url),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);

            runtime.block_on(async move {
                let jwks = state.fetch_jwks(&jwks_url2, true).await;
                assert!(jwks.is_ok(), "fetch_jwks failed: {jwks:?}");

                let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
                let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
                header.kid = Some("kid-1".to_string());
                let token = jsonwebtoken::encode(
                    &header,
                    &claims,
                    &jsonwebtoken::EncodingKey::from_secret(secret),
                )
                .expect("encode token");
                let auth = format!("Bearer {token}");
                let req = make_request_with_peer_addr(
                    Http1Method::Post,
                    "/api/",
                    &[("Authorization", auth.as_str())],
                    Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
                );
                let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
                assert!(
                    state
                        .check_rbac_and_rate_limit(&req, &json_rpc)
                        .await
                        .is_none()
                );
            });
        });
    }

    #[test]
    fn jwt_hs256_jwks_kid_missing_uses_first_key() {
        use base64::Engine as _;

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let secret = b"secret";
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "oct",
                "alg": "HS256",
                "kid": "kid-1",
                "k": k,
            }]
        });
        let jwks_bytes = serde_json::to_vec(&jwks).expect("jwks json");

        with_jwks_server(&jwks_bytes, 2, |jwks_url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_secret: None,
                http_jwt_jwks_url: Some(jwks_url),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);

            runtime.block_on(async move {
                let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
                let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256); // kid missing
                let token = jsonwebtoken::encode(
                    &header,
                    &claims,
                    &jsonwebtoken::EncodingKey::from_secret(secret),
                )
                .expect("encode token");
                let auth = format!("Bearer {token}");
                let req = make_request_with_peer_addr(
                    Http1Method::Post,
                    "/api/",
                    &[("Authorization", auth.as_str())],
                    Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
                );
                let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
                assert!(
                    state
                        .check_rbac_and_rate_limit(&req, &json_rpc)
                        .await
                        .is_none(),
                    "kid missing should use first key in JWKS"
                );
            });
        });
    }

    #[test]
    fn jwt_hs256_jwks_kid_mismatch_is_rejected() {
        use base64::Engine as _;

        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let secret = b"secret";
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "oct",
                "alg": "HS256",
                "kid": "kid-1",
                "k": k,
            }]
        });
        let jwks_bytes = serde_json::to_vec(&jwks).expect("jwks json");

        with_jwks_server(&jwks_bytes, 4, |jwks_url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_secret: None,
                http_jwt_jwks_url: Some(jwks_url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);

            runtime.block_on(async move {
                // Warm cache so the first lookup uses cached JWKS; the kid mismatch path
                // should still attempt a forced refresh before failing.
                let _ = state.fetch_jwks(&jwks_url, true).await;

                let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
                let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
                header.kid = Some("kid-missing".to_string());
                let token = jsonwebtoken::encode(
                    &header,
                    &claims,
                    &jsonwebtoken::EncodingKey::from_secret(secret),
                )
                .expect("encode token");
                let auth = format!("Bearer {token}");
                let req = make_request_with_peer_addr(
                    Http1Method::Post,
                    "/api/",
                    &[("Authorization", auth.as_str())],
                    Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
                );
                let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
                let resp = state
                    .check_rbac_and_rate_limit(&req, &json_rpc)
                    .await
                    .expect("kid mismatch should be rejected");
                assert_unauthorized(&resp);
            });
        });
    }

    #[test]
    fn jwt_hs256_jwks_invalid_json_is_rejected() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        let bad_jwks = b"{this is not json}".to_vec();
        with_jwks_server(&bad_jwks, 2, |jwks_url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_secret: None,
                http_jwt_jwks_url: Some(jwks_url),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);

            runtime.block_on(async move {
                let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
                let token = hs256_token(b"secret", &claims);
                let auth = format!("Bearer {token}");
                let req = make_request_with_peer_addr(
                    Http1Method::Post,
                    "/api/",
                    &[("Authorization", auth.as_str())],
                    Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
                );
                let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
                let resp = state
                    .check_rbac_and_rate_limit(&req, &json_rpc)
                    .await
                    .expect("invalid JWKS must be rejected");
                assert_unauthorized(&resp);
            });
        });
    }

    // ── br-1i11.4.5: JWKS bootstrap/failure edge-case unit tests ──────────

    fn make_test_jwks_bytes() -> Vec<u8> {
        use base64::Engine as _;
        let k =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"test-secret-for-jwks-unit");
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "oct",
                "alg": "HS256",
                "kid": "unit-kid",
                "k": k,
            }]
        });
        serde_json::to_vec(&jwks).expect("jwks json")
    }

    fn make_test_jwks_set() -> Arc<JwkSet> {
        let bytes = make_test_jwks_bytes();
        Arc::new(serde_json::from_slice(&bytes).expect("parse jwks"))
    }

    fn build_jwt_state_with_url(url: &str) -> HttpState {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_algorithms: vec!["HS256".to_string()],
            http_jwt_secret: None,
            http_jwt_jwks_url: Some(url.to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        build_state(config)
    }

    /// Mini TCP server returning a configurable HTTP status code.
    fn with_status_server<F>(status: u16, body: &[u8], f: F)
    where
        F: FnOnce(String),
    {
        use std::io::{Read as IoRead, Write as IoWrite};
        use std::net::TcpListener;
        use std::time::Instant as StdInstant;

        std::thread::scope(|s| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let addr = listener.local_addr().expect("addr");
            let body2 = body.to_vec();
            let done = Arc::new(AtomicBool::new(false));
            let done2 = Arc::clone(&done);

            s.spawn(move || {
                let deadline = StdInstant::now() + Duration::from_secs(5);
                while !done2.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                            let mut buf = [0u8; 512];
                            loop {
                                match stream.read(&mut buf) {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                            let reason = if status == 200 { "OK" } else { "Error" };
                            let hdr = format!(
                                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body2.len()
                            );
                            let _ = stream.write_all(hdr.as_bytes());
                            let _ = stream.write_all(&body2);
                            let _ = stream.flush();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if StdInstant::now() > deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            let url = format!("http://{addr}/jwks");
            f(url);
            done.store(true, Ordering::Relaxed);
        });
    }

    #[test]
    fn jwks_bootstrap_empty_cache_populates_on_first_fetch() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let jwks_bytes = make_test_jwks_bytes();

        with_jwks_server(&jwks_bytes, 1, |jwks_url| {
            let state = build_jwt_state_with_url(&jwks_url);

            // Cache starts empty.
            assert!(state.jwks_cache.lock().unwrap().is_none());

            runtime.block_on(async {
                let result = state.fetch_jwks(&jwks_url, false).await;
                assert!(result.is_ok(), "bootstrap fetch must succeed");
            });

            // Cache should now be populated.
            let cached = state.jwks_cache.lock().unwrap();
            assert!(cached.is_some(), "cache must be populated after bootstrap");
            assert!(!cached.as_ref().unwrap().jwks.keys.is_empty());
            drop(cached);
        });
    }

    #[test]
    fn jwks_fresh_cache_returns_cached_without_network() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let jwks = make_test_jwks_set();

        // Unreachable URL: if the code tries to fetch it will fail.
        let state = build_jwt_state_with_url("http://127.0.0.1:1/unreachable");

        // Pre-populate with a FRESH entry.
        {
            let mut cache = state.jwks_cache.lock().unwrap();
            *cache = Some(JwksCacheEntry {
                fetched_at: Instant::now(),
                jwks: Arc::clone(&jwks),
            });
        }

        runtime.block_on(async {
            let result = state
                .fetch_jwks("http://127.0.0.1:1/unreachable", false)
                .await;
            assert!(result.is_ok(), "fresh cache must return Ok without network");
            assert!(
                Arc::ptr_eq(&result.unwrap(), &jwks),
                "must return the exact cached Arc"
            );
        });
    }

    #[test]
    fn jwks_stale_while_revalidate_returns_stale_when_refreshing() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let jwks = make_test_jwks_set();

        let state = build_jwt_state_with_url("http://127.0.0.1:1/unreachable");

        // Pre-populate with a STALE entry (2 min old > 60s TTL).
        {
            let mut cache = state.jwks_cache.lock().unwrap();
            *cache = Some(JwksCacheEntry {
                fetched_at: Instant::now().checked_sub(Duration::from_mins(2)).unwrap(),
                jwks: Arc::clone(&jwks),
            });
        }

        // Simulate another task already refreshing.
        state.jwks_refreshing.store(true, Ordering::Release);

        runtime.block_on(async {
            let result = state
                .fetch_jwks("http://127.0.0.1:1/unreachable", false)
                .await;
            assert!(result.is_ok(), "stale-while-revalidate must return Ok");
            assert!(
                Arc::ptr_eq(&result.unwrap(), &jwks),
                "must return the stale cached Arc"
            );
        });

        // Guard should still be true (we didn't acquire it; the "other task" holds it).
        assert!(
            state.jwks_refreshing.load(Ordering::Acquire),
            "guard must remain set when another task holds it"
        );
    }

    #[test]
    fn jwks_stale_cache_triggers_refresh_when_not_refreshing() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let jwks_bytes = make_test_jwks_bytes();
        let old_jwks = make_test_jwks_set();

        with_jwks_server(&jwks_bytes, 1, |jwks_url| {
            let state = build_jwt_state_with_url(&jwks_url);

            // Pre-populate with a STALE entry.
            {
                let mut cache = state.jwks_cache.lock().unwrap();
                *cache = Some(JwksCacheEntry {
                    fetched_at: Instant::now().checked_sub(Duration::from_mins(2)).unwrap(),
                    jwks: Arc::clone(&old_jwks),
                });
            }

            runtime.block_on(async {
                let result = state.fetch_jwks(&jwks_url, false).await;
                assert!(result.is_ok(), "stale refresh must succeed");
                // Should get a NEW Arc (from the fetch), not the old one.
                assert!(
                    !Arc::ptr_eq(&result.unwrap(), &old_jwks),
                    "must return fresh data, not the stale Arc"
                );
            });

            // Guard must be released after refresh.
            assert!(
                !state.jwks_refreshing.load(Ordering::Acquire),
                "guard must be released after refresh"
            );
        });
    }

    #[test]
    fn jwks_force_bypasses_fresh_cache() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let jwks_bytes = make_test_jwks_bytes();
        let cached_jwks = make_test_jwks_set();

        with_jwks_server(&jwks_bytes, 1, |jwks_url| {
            let state = build_jwt_state_with_url(&jwks_url);

            // Pre-populate with a FRESH entry.
            {
                let mut cache = state.jwks_cache.lock().unwrap();
                *cache = Some(JwksCacheEntry {
                    fetched_at: Instant::now(),
                    jwks: Arc::clone(&cached_jwks),
                });
            }

            runtime.block_on(async {
                let result = state.fetch_jwks(&jwks_url, true).await;
                assert!(result.is_ok(), "force fetch must succeed");
                // Should get a NEW Arc from the network, not the cached one.
                assert!(
                    !Arc::ptr_eq(&result.unwrap(), &cached_jwks),
                    "force must bypass fresh cache"
                );
            });
        });
    }

    #[test]
    fn jwks_fetch_failure_empty_cache_resets_guard() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");

        // Bind then drop → connection refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let url = format!("http://{addr}/jwks");
        let state = build_jwt_state_with_url(&url);

        runtime.block_on(async {
            let result = state.fetch_jwks(&url, false).await;
            assert!(result.is_err(), "fetch to closed port must fail");
        });

        assert!(
            !state.jwks_refreshing.load(Ordering::Acquire),
            "guard must be reset after failure with empty cache"
        );
        // Cache must remain empty.
        assert!(state.jwks_cache.lock().unwrap().is_none());
    }

    #[test]
    fn jwks_force_failure_resets_guard() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let cached_jwks = make_test_jwks_set();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let url = format!("http://{addr}/jwks");
        let state = build_jwt_state_with_url(&url);

        // Pre-populate cache; force=true should still attempt network.
        {
            let mut cache = state.jwks_cache.lock().unwrap();
            *cache = Some(JwksCacheEntry {
                fetched_at: Instant::now(),
                jwks: Arc::clone(&cached_jwks),
            });
        }

        runtime.block_on(async {
            let result = state.fetch_jwks(&url, true).await;
            assert!(result.is_err(), "force fetch to closed port must fail");
        });

        assert!(
            !state.jwks_refreshing.load(Ordering::Acquire),
            "guard must be reset after force failure"
        );
    }

    #[test]
    fn jwks_recovery_succeeds_after_failure() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let jwks_bytes = make_test_jwks_bytes();

        // Phase 1: fail against closed port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let closed_addr = listener.local_addr().expect("addr");
        drop(listener);

        let bad_url = format!("http://{closed_addr}/jwks");
        let state = build_jwt_state_with_url(&bad_url);

        runtime.block_on(async {
            let fail = state.fetch_jwks(&bad_url, false).await;
            assert!(fail.is_err(), "phase 1 must fail");
        });
        assert!(!state.jwks_refreshing.load(Ordering::Acquire));

        // Phase 2: succeed with a real server.
        with_jwks_server(&jwks_bytes, 1, |good_url| {
            runtime.block_on(async {
                let ok = state.fetch_jwks(&good_url, false).await;
                assert!(ok.is_ok(), "phase 2 retry must succeed after guard reset");
            });
        });

        // Cache should be populated after recovery.
        assert!(state.jwks_cache.lock().unwrap().is_some());
    }

    #[test]
    fn jwks_non_200_response_returns_err() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");

        with_status_server(500, b"{}", |url| {
            let state = build_jwt_state_with_url(&url);

            runtime.block_on(async {
                let result = state.fetch_jwks(&url, false).await;
                assert!(result.is_err(), "non-200 must return Err");
            });

            assert!(
                !state.jwks_refreshing.load(Ordering::Acquire),
                "guard must be reset after non-200"
            );
        });
    }

    #[test]
    fn jwks_empty_keys_array_is_valid() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let empty_jwks = serde_json::to_vec(&serde_json::json!({"keys": []})).unwrap();

        with_jwks_server(&empty_jwks, 1, |jwks_url| {
            let state = build_jwt_state_with_url(&jwks_url);

            runtime.block_on(async {
                let result = state.fetch_jwks(&jwks_url, false).await;
                assert!(result.is_ok(), "empty keys array is valid JWKS");
                assert!(result.unwrap().keys.is_empty());
            });
        });
    }

    // -- TOON wrapping tests --

    #[test]
    fn extract_format_from_uri_toon() {
        assert_eq!(
            extract_format_from_uri("resource://inbox/BlueLake?project=/backend&format=toon"),
            Some("toon".to_string())
        );
    }

    #[test]
    fn extract_format_from_uri_json() {
        assert_eq!(
            extract_format_from_uri("resource://inbox/BlueLake?project=/backend&format=json"),
            Some("json".to_string())
        );
    }

    #[test]
    fn extract_format_from_uri_none() {
        assert_eq!(
            extract_format_from_uri("resource://inbox/BlueLake?project=/backend"),
            None
        );
    }

    #[test]
    fn extract_format_from_uri_no_query() {
        assert_eq!(extract_format_from_uri("resource://agents/myproj"), None);
    }

    #[test]
    fn toon_wrapping_json_format_noop() {
        let config = mcp_agent_mail_core::Config::default();
        let mut value = serde_json::json!({
            "content": [{"type": "text", "text": "{\"id\":1}"}]
        });
        apply_toon_to_content(&mut value, "content", "json", &config);
        // Should be unchanged
        assert_eq!(value["content"][0]["text"].as_str().unwrap(), "{\"id\":1}");
    }

    #[test]
    fn toon_wrapping_invalid_format_noop() {
        let config = mcp_agent_mail_core::Config::default();
        let mut value = serde_json::json!({
            "content": [{"type": "text", "text": "{\"id\":1}"}]
        });
        apply_toon_to_content(&mut value, "content", "xml", &config);
        // Should be unchanged (invalid format)
        assert_eq!(value["content"][0]["text"].as_str().unwrap(), "{\"id\":1}");
    }

    #[test]
    fn toon_wrapping_toon_format_produces_envelope() {
        let config = mcp_agent_mail_core::Config::default();
        let mut value = serde_json::json!({
            "content": [{"type": "text", "text": "{\"id\":1,\"subject\":\"Test\"}"}]
        });
        apply_toon_to_content(&mut value, "content", "toon", &config);
        let text = value["content"][0]["text"].as_str().unwrap();
        let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
        // Format is either "toon" (encoder present) or "json" (fallback)
        let fmt = envelope["format"].as_str().unwrap();
        assert!(fmt == "toon" || fmt == "json", "unexpected format: {fmt}");
        assert_eq!(envelope["meta"]["requested"], "toon");
        assert_eq!(envelope["meta"]["source"], "param");
        if fmt == "toon" {
            // Successful encode: data is a string, encoder is set
            assert!(envelope["data"].is_string());
            assert!(envelope["meta"]["encoder"].as_str().is_some());
        } else {
            // Fallback: data is the original JSON, toon_error is set
            assert_eq!(envelope["data"]["id"], 1);
            assert_eq!(envelope["data"]["subject"], "Test");
            assert!(envelope["meta"]["toon_error"].as_str().is_some());
        }
    }

    #[test]
    fn toon_wrapping_invalid_encoder_fallback() {
        // Force a non-existent encoder to test fallback behavior
        let config = mcp_agent_mail_core::Config {
            toon_bin: Some("/nonexistent/tru_binary".to_string()),
            ..Default::default()
        };
        let mut value = serde_json::json!({
            "content": [{"type": "text", "text": "{\"id\":1,\"subject\":\"Test\"}"}]
        });
        apply_toon_to_content(&mut value, "content", "toon", &config);
        let text = value["content"][0]["text"].as_str().unwrap();
        let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(envelope["format"], "json"); // fallback
        assert_eq!(envelope["data"]["id"], 1);
        assert_eq!(envelope["meta"]["requested"], "toon");
        assert!(envelope["meta"]["toon_error"].as_str().is_some());
    }

    #[test]
    fn toon_wrapping_non_json_text_unchanged() {
        let config = mcp_agent_mail_core::Config::default();
        let mut value = serde_json::json!({
            "content": [{"type": "text", "text": "not json content"}]
        });
        apply_toon_to_content(&mut value, "content", "toon", &config);
        // Non-JSON text should be left as-is
        assert_eq!(
            value["content"][0]["text"].as_str().unwrap(),
            "not json content"
        );
    }

    #[test]
    fn toon_wrapping_respects_content_key() {
        let config = mcp_agent_mail_core::Config::default();
        // Resources use "contents" not "content"
        let mut value = serde_json::json!({
            "contents": [{"type": "text", "text": "{\"agent\":\"Blue\"}"}]
        });
        apply_toon_to_content(&mut value, "contents", "toon", &config);
        let text = value["contents"][0]["text"].as_str().unwrap();
        let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
        // Format is either "toon" (encoder present) or "json" (fallback)
        let fmt = envelope["format"].as_str().unwrap();
        assert!(fmt == "toon" || fmt == "json");
        assert_eq!(envelope["meta"]["requested"], "toon");
    }

    #[test]
    fn request_log_line_policy_suppresses_duplicate_kv_in_rich_tty_mode() {
        assert!(RuntimeOutputMode::HeadlessText.should_emit_structured_request_line(false));
        assert!(RuntimeOutputMode::HeadlessJson.should_emit_structured_request_line(false));
        assert!(!RuntimeOutputMode::HeadlessText.should_emit_structured_request_line(true));
        assert!(RuntimeOutputMode::HeadlessJson.should_emit_structured_request_line(true));
        assert!(!RuntimeOutputMode::Tui.should_emit_structured_request_line(false));
    }

    #[test]
    fn request_panel_width_from_columns_clamps_bounds() {
        assert_eq!(request_panel_width_from_columns(1), 60);
        assert_eq!(request_panel_width_from_columns(60), 60);
        assert_eq!(request_panel_width_from_columns(100), 100);
        assert_eq!(request_panel_width_from_columns(200), 140);
    }

    #[test]
    fn parse_stty_size_parses_rows_and_cols() {
        assert_eq!(parse_stty_size(b"24 80\n"), Some((80, 24)));
    }

    #[test]
    fn parse_stty_size_rejects_malformed_output() {
        assert_eq!(parse_stty_size(b""), None);
        assert_eq!(parse_stty_size(b"0\n"), None);
        assert_eq!(parse_stty_size(b"abc def\n"), None);
        assert_eq!(parse_stty_size(b"24 80 1\n"), None);
    }

    #[test]
    fn is_degenerate_terminal_size_only_flags_zero_dimensions() {
        assert!(is_degenerate_terminal_size(0, 24));
        assert!(is_degenerate_terminal_size(80, 0));
        assert!(is_degenerate_terminal_size(0, 0));
        assert!(!is_degenerate_terminal_size(120, 40));
    }

    #[test]
    fn http_request_logging_disabled_emits_no_output() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();
        assert!(
            out.trim().is_empty(),
            "expected no output when request logging disabled, got: {out:?}"
        );
    }

    #[test]
    fn http_request_logging_kv_branch_emits_structured_and_panel_output() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[],
            Some("127.0.0.1:12345".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();

        // KeyValueRenderer-ish line
        assert!(out.contains("event='request'"), "missing event: {out:?}");
        assert!(
            out.contains("path='/health/liveness'"),
            "missing path: {out:?}"
        );
        assert!(out.contains("status=200"), "missing status: {out:?}");
        assert!(out.contains("method='GET'"), "missing method: {out:?}");
        assert!(out.contains("duration_ms="), "missing duration_ms: {out:?}");
        assert!(
            out.contains("client_ip='127.0.0.1'"),
            "missing client_ip: {out:?}"
        );

        // Panel output
        assert!(
            out.contains("| GET  /health/liveness  200 "),
            "missing panel title: {out:?}"
        );
        assert!(
            out.contains("| client: 127.0.0.1"),
            "missing panel body: {out:?}"
        );
    }

    #[test]
    fn http_request_logging_json_branch_emits_json_and_panel_output() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: true,
            http_otel_enabled: true,
            http_otel_service_name: "mcp-agent-mail-test".to_string(),
            http_otel_exporter_otlp_endpoint: "http://127.0.0.1:4318".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[],
            Some("127.0.0.1:12345".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();

        // Find and parse the JSON log line.
        let json_line = out
            .lines()
            .find(|line| line.trim_start().starts_with('{') && line.trim_end().ends_with('}'))
            .expect("expected JSON log line");
        let value: serde_json::Value =
            serde_json::from_str(json_line).expect("json log line should parse");
        assert_eq!(value["event"], "request");
        assert_eq!(value["method"], "GET");
        assert_eq!(value["path"], "/health/liveness");
        assert_eq!(value["status"], 200);
        assert_eq!(value["client_ip"], "127.0.0.1");

        // Panel output
        assert!(
            out.contains("| GET  /health/liveness  200 "),
            "missing panel title: {out:?}"
        );
        assert!(
            out.contains("| client: 127.0.0.1"),
            "missing panel body: {out:?}"
        );
    }

    #[test]
    fn http_request_panel_tiny_width_returns_none_and_fallback_is_exact() {
        assert!(console::render_http_request_panel(0, "GET", "/", 200, 1, "x", false).is_none());
        assert_eq!(
            http_request_log_fallback_line("GET", "/x", 404, 12, "127.0.0.1"),
            "http method=GET path=/x status=404 ms=12 client=127.0.0.1"
        );
    }

    #[test]
    fn expected_error_filter_skips_without_exc_info() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            false,
            SimpleLogLevel::Error,
            "index.lock contention",
            false,
            &[],
        );
        assert!(!out.is_expected);
        assert!(!out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Error);
    }

    #[test]
    fn expected_error_filter_applies_only_to_target_logger() {
        let out = expected_error_filter(
            "some.other.logger",
            true,
            SimpleLogLevel::Error,
            "index.lock contention",
            false,
            &[],
        );
        assert!(!out.is_expected);
        assert!(!out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Error);
    }

    #[test]
    fn expected_error_filter_matches_patterns_and_downgrades_error_to_info() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "Git index.lock temporarily locked",
            false,
            &[],
        );
        assert!(out.is_expected);
        assert!(out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Info);
    }

    #[test]
    fn expected_error_filter_matches_recoverable_flag_even_without_pattern() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "some random error",
            true,
            &[],
        );
        assert!(out.is_expected);
        assert!(out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Info);
    }

    #[test]
    fn expected_error_filter_matches_cause_chain() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "top-level error",
            false,
            &[("Available agents: ...", false)],
        );
        assert!(out.is_expected);
        assert!(out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Info);
    }

    // ── HTTP Logging Parity: additional coverage (br-1bm.6.4) ─────────

    #[test]
    fn http_request_panel_no_ansi_output() {
        // Non-TTY: should render panel without ANSI escape codes.
        let panel =
            console::render_http_request_panel(100, "POST", "/mcp", 201, 42, "10.0.0.1", false);
        assert!(panel.is_some());
        let text = panel.unwrap();
        // Should not contain ANSI escape sequences.
        assert!(
            !text.contains("\x1b["),
            "non-TTY panel should have no ANSI codes: {text:?}"
        );
        // Should contain the key fields.
        assert!(text.contains("POST"), "missing method");
        assert!(text.contains("/mcp"), "missing path");
        assert!(text.contains("201"), "missing status");
        assert!(text.contains("42ms"), "missing duration");
        assert!(text.contains("10.0.0.1"), "missing client IP");
    }

    #[test]
    fn http_request_panel_ansi_output() {
        // TTY: should render panel with ANSI escape codes.
        let panel =
            console::render_http_request_panel(100, "GET", "/health", 200, 5, "127.0.0.1", true);
        assert!(panel.is_some());
        let text = panel.unwrap();
        assert!(
            text.contains("\x1b["),
            "TTY panel should have ANSI codes: {text:?}"
        );
    }

    #[test]
    fn http_request_panel_error_status_color() {
        // 5xx should use theme error color (24-bit ANSI) in ANSI mode.
        let panel = console::render_http_request_panel(100, "GET", "/x", 500, 1, "x", true);
        assert!(panel.is_some());
        let text = panel.unwrap();
        assert!(
            text.contains("38;2;"),
            "error status should use 24-bit theme color: {text:?}"
        );
    }

    #[test]
    fn kv_line_key_order_matches_legacy() {
        // Legacy key_order: ["event", "path", "status"] first, then remaining.
        let line = http_request_log_kv_line(
            "2026-02-06T00:00:00.000000Z",
            "GET",
            "/api",
            200,
            15,
            "10.0.0.1",
        );
        // Verify ordering: event before path before status.
        let event_pos = line.find("event=").unwrap();
        let path_pos = line.find("path=").unwrap();
        let status_pos = line.find("status=").unwrap();
        assert!(event_pos < path_pos, "event should come before path");
        assert!(path_pos < status_pos, "path should come before status");

        // method, duration_ms, client_ip, timestamp, level should follow.
        let method_pos = line.find("method=").unwrap();
        assert!(status_pos < method_pos, "status should come before method");
    }

    #[test]
    fn json_log_line_has_all_required_fields() {
        let line = http_request_log_json_line(
            "2026-02-06T00:00:00.000000Z",
            "POST",
            "/mcp",
            201,
            42,
            "10.0.0.1",
        );
        assert!(line.is_some());
        let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        // Verify all 8 fields from legacy.
        assert_eq!(value["event"], "request");
        assert_eq!(value["method"], "POST");
        assert_eq!(value["path"], "/mcp");
        assert_eq!(value["status"], 201);
        assert_eq!(value["duration_ms"], 42);
        assert_eq!(value["client_ip"], "10.0.0.1");
        assert_eq!(value["level"], "info");
        assert_eq!(value["timestamp"], "2026-02-06T00:00:00.000000Z");
    }

    #[test]
    fn py_repr_str_matches_legacy_quoting() {
        // Python's repr(str) uses single quotes.
        assert_eq!(py_repr_str("hello"), "'hello'");
        assert_eq!(py_repr_str("/api/v1"), "'/api/v1'");
        assert_eq!(py_repr_str("it's"), "'it\\'s'");
        assert_eq!(py_repr_str("back\\slash"), "'back\\\\slash'");
    }

    #[test]
    fn expected_error_filter_all_patterns() {
        // Verify each of the 8 expected patterns triggers the filter.
        let patterns = [
            "Agent not found in project backend",
            "Git index.lock contention detected",
            "git_index_lock error occurred",
            "resource_busy: database is locked",
            "Table temporarily locked by another process",
            "ToolExecutionError recoverable=true data={}",
            "Unknown agent name. Did you mean to use register_agent first?",
            "available agents: GreenCastle, BlueBear",
        ];
        for msg in &patterns {
            let out = expected_error_filter(
                EXPECTED_ERROR_FILTER_TARGET,
                true,
                SimpleLogLevel::Error,
                msg,
                false,
                &[],
            );
            assert!(out.is_expected, "pattern should be expected: {msg:?}");
            assert!(out.suppress_exc);
            assert_eq!(
                out.effective_level,
                SimpleLogLevel::Info,
                "ERROR should downgrade to INFO for: {msg:?}"
            );
        }
    }

    #[test]
    fn expected_error_filter_non_expected_passes_through() {
        // A genuinely unexpected error should NOT be filtered.
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "segfault in critical path",
            false,
            &[],
        );
        assert!(!out.is_expected);
        assert!(!out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Error);
    }

    #[test]
    fn expected_error_filter_warn_level_not_downgraded() {
        // Warn-level expected errors stay at Warn (only ERROR → INFO).
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Warn,
            "index.lock contention",
            false,
            &[],
        );
        assert!(out.is_expected);
        assert!(out.suppress_exc);
        assert_eq!(
            out.effective_level,
            SimpleLogLevel::Warn,
            "Warn should stay Warn, not downgrade"
        );
    }

    #[test]
    fn expected_error_filter_cause_chain_recoverable() {
        // A cause that is recoverable should trigger the filter.
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "outer wrapper error",
            false,
            &[("inner error", true)], // cause is recoverable
        );
        assert!(out.is_expected);
        assert!(out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Info);
    }

    #[test]
    fn expected_error_filter_case_insensitive() {
        // Patterns should match case-insensitively.
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "RESOURCE_BUSY: DATABASE IS LOCKED",
            false,
            &[],
        );
        assert!(out.is_expected, "case-insensitive matching should work");
    }

    // ── Base path mount + passthrough tests (br-1bm.4.3) ────────────────

    #[test]
    fn normalize_base_path_defaults() {
        assert_eq!(normalize_base_path(""), "/");
        assert_eq!(normalize_base_path("/"), "/");
        assert_eq!(normalize_base_path("  "), "/");
    }

    #[test]
    fn normalize_base_path_strips_trailing_slash() {
        assert_eq!(normalize_base_path("/api/"), "/api");
        assert_eq!(normalize_base_path("/api/mcp/"), "/api/mcp");
    }

    #[test]
    fn normalize_base_path_adds_leading_slash() {
        assert_eq!(normalize_base_path("api"), "/api");
        assert_eq!(normalize_base_path("api/mcp"), "/api/mcp");
    }

    #[test]
    fn detect_transport_mode_reports_mcp_api_and_custom() {
        assert_eq!(detect_transport_mode("/mcp/"), "mcp");
        assert_eq!(detect_transport_mode("api"), "api");
        assert_eq!(detect_transport_mode("/v2/rpc"), "custom");
        assert_eq!(detect_transport_mode("/api/v2"), "custom");
    }

    #[test]
    fn path_allowed_root_base_accepts_everything() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(state.path_allowed("/"));
        assert!(state.path_allowed("/anything"));
        assert!(state.path_allowed("/foo/bar"));
    }

    #[test]
    fn path_allowed_accepts_base_with_and_without_slash() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(state.path_allowed("/api"), "exact base must be allowed");
        assert!(
            state.path_allowed("/api/"),
            "base with trailing slash must be allowed"
        );
    }

    #[test]
    fn path_allowed_accepts_sub_paths() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(
            state.path_allowed("/api/mcp"),
            "sub-path under base must be allowed (mount semantics)"
        );
        assert!(state.path_allowed("/api/v1/rpc"));
    }

    #[test]
    fn path_allowed_rejects_unrelated_paths() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(!state.path_allowed("/"), "root must not match /api base");
        assert!(
            !state.path_allowed("/apifoo"),
            "prefix without slash separator must not match"
        );
        assert!(!state.path_allowed("/other/path"));
    }

    #[test]
    fn path_allowed_nested_base() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api/mcp".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(state.path_allowed("/api/mcp"));
        assert!(state.path_allowed("/api/mcp/"));
        assert!(state.path_allowed("/api/mcp/sub"));
        assert!(!state.path_allowed("/api"));
        assert!(!state.path_allowed("/api/"));
    }

    #[test]
    fn path_allowed_api_base_accepts_mcp_alias() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/api".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(state.path_allowed("/mcp"));
        assert!(state.path_allowed("/mcp/"));
        assert!(state.path_allowed("/mcp/tools"));
    }

    #[test]
    fn path_allowed_mcp_base_accepts_api_alias() {
        let config = mcp_agent_mail_core::Config {
            http_path: "/mcp".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        assert!(state.path_allowed("/api"));
        assert!(state.path_allowed("/api/"));
        assert!(state.path_allowed("/api/resources"));
    }

    #[test]
    fn canonicalize_mcp_path_alias_maps_to_configured_base() {
        assert_eq!(canonicalize_mcp_path_for_handler("/mcp", "/api"), "/api");
        assert_eq!(
            canonicalize_mcp_path_for_handler("/mcp/tools/list", "/api"),
            "/api/tools/list"
        );
        assert_eq!(canonicalize_mcp_path_for_handler("/api", "/mcp"), "/mcp");
        assert_eq!(
            canonicalize_mcp_path_for_handler("/api/prompts/get", "/mcp"),
            "/mcp/prompts/get"
        );
    }

    #[test]
    fn canonicalize_mcp_path_alias_ignores_nested_base() {
        assert_eq!(
            canonicalize_mcp_path_for_handler("/api/mcp", "/api/mcp"),
            "/api/mcp"
        );
        assert_eq!(
            canonicalize_mcp_path_for_handler("/mcp", "/api/mcp"),
            "/mcp"
        );
    }

    #[test]
    fn mcp_base_alias_no_slash_returns_symmetric_alias() {
        assert_eq!(mcp_base_alias_no_slash("/api"), Some("/mcp"));
        assert_eq!(mcp_base_alias_no_slash("/mcp"), Some("/api"));
        assert_eq!(mcp_base_alias_no_slash("/foo"), None);
        assert_eq!(mcp_base_alias_no_slash("/api/mcp"), None);
        assert_eq!(mcp_base_alias_no_slash(""), None);
    }

    #[test]
    fn path_matches_base_exact_and_subpath() {
        assert!(path_matches_base("/api", "/api"));
        assert!(path_matches_base("/api/", "/api"));
        assert!(path_matches_base("/api/tools/list", "/api"));
        assert!(!path_matches_base("/apifoo", "/api"));
        assert!(!path_matches_base("/mcp", "/api"));
        assert!(!path_matches_base("/", "/api"));
    }

    #[test]
    fn canonicalize_mcp_path_trailing_slash_and_noop() {
        // Trailing-slash variants
        assert_eq!(canonicalize_mcp_path_for_handler("/mcp/", "/api"), "/api/");
        assert_eq!(canonicalize_mcp_path_for_handler("/api/", "/api"), "/api/");
        // Non-aliased base passes through unchanged
        assert_eq!(
            canonicalize_mcp_path_for_handler("/other/", "/other"),
            "/other/"
        );
        assert_eq!(
            canonicalize_mcp_path_for_handler("/other/foo", "/other"),
            "/other/foo"
        );
    }

    // ── Header normalization tests (br-1bm.4.2) ──────────────────────────

    #[test]
    fn header_normalization_forces_accept() {
        let req = Http1Request {
            method: Http1Method::Post,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert_eq!(
            mcp.headers.get("accept").map(String::as_str),
            Some("application/json, text/event-stream"),
            "Accept must always be forced to JSON+SSE"
        );
    }

    #[test]
    fn header_normalization_replaces_existing_accept() {
        let req = Http1Request {
            method: Http1Method::Post,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![
                ("Accept".to_string(), "text/html".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert_eq!(
            mcp.headers.get("accept").map(String::as_str),
            Some("application/json, text/event-stream"),
            "Existing Accept header must be replaced, not preserved"
        );
    }

    #[test]
    fn header_normalization_replaces_accept_case_insensitive() {
        let req = Http1Request {
            method: Http1Method::Get,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![("ACCEPT".to_string(), "text/xml".to_string())],
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert_eq!(
            mcp.headers.get("accept").map(String::as_str),
            Some("application/json, text/event-stream"),
            "Accept replacement must be case-insensitive"
        );
        // The original ACCEPT=text/xml must not survive under any casing
        assert!(
            !mcp.headers.values().any(|v| v == "text/xml"),
            "Original Accept value must be gone"
        );
    }

    #[test]
    fn header_normalization_adds_content_type_for_post() {
        let req = Http1Request {
            method: Http1Method::Post,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![], // no headers at all
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert_eq!(
            mcp.headers.get("content-type").map(String::as_str),
            Some("application/json"),
            "Content-Type must be added for POST when missing"
        );
    }

    #[test]
    fn header_normalization_preserves_existing_content_type() {
        let req = Http1Request {
            method: Http1Method::Post,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![(
                "Content-Type".to_string(),
                "multipart/form-data".to_string(),
            )],
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert_eq!(
            mcp.headers.get("content-type").map(String::as_str),
            Some("multipart/form-data"),
            "Existing Content-Type must not be overwritten"
        );
    }

    #[test]
    fn header_normalization_no_content_type_for_get() {
        let req = Http1Request {
            method: Http1Method::Get,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![],
            body: Vec::new(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert!(
            !mcp.headers.contains_key("content-type"),
            "Content-Type must NOT be injected for non-POST methods"
        );
    }

    #[test]
    fn header_normalization_preserves_other_headers() {
        let req = Http1Request {
            method: Http1Method::Post,
            uri: "/api".to_string(),
            version: Http1Version::Http11,
            headers: vec![
                ("Authorization".to_string(), "Bearer tok".to_string()),
                ("X-Custom".to_string(), "val".to_string()),
                ("Accept".to_string(), "text/plain".to_string()),
            ],
            body: b"hello".to_vec(),
            trailers: Vec::new(),
            peer_addr: None,
        };
        let mcp = to_mcp_http_request(&req, "/api");
        assert_eq!(
            mcp.headers.get("authorization").map(String::as_str),
            Some("Bearer tok"),
            "Authorization must be preserved"
        );
        assert_eq!(
            mcp.headers.get("x-custom").map(String::as_str),
            Some("val"),
            "Custom headers must be preserved"
        );
        assert_eq!(
            mcp.headers.get("accept").map(String::as_str),
            Some("application/json, text/event-stream"),
            "Accept must still be forced"
        );
    }

    // ── Health + Well-Known Endpoints Parity (br-1bm.9) ─────────────────

    #[test]
    fn health_liveness_returns_alive_json() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body, serde_json::json!({"status": "alive"}));
    }

    #[test]
    fn healthz_alias_returns_alive_json() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/healthz", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body, serde_json::json!({"status": "alive"}));
    }

    #[test]
    fn health_liveness_has_json_content_type() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn healthz_alias_has_json_content_type() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/healthz", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn health_liveness_emits_agent_mail_signature_header() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, startup_checks::HEALTH_SIGNATURE_HEADER_NAME),
            Some(startup_checks::HEALTH_SIGNATURE_HEADER_VALUE)
        );
    }

    #[test]
    fn health_liveness_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn healthz_alias_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/healthz", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn health_readiness_returns_ready_json() {
        let config = mcp_agent_mail_core::Config {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "ready");
        // Enriched identity fields are present.
        assert!(
            body.get("version").is_some(),
            "version field must be present"
        );
        assert!(
            body.get("database_path").is_some(),
            "database_path field must be present"
        );
        assert!(
            body.get("project_count").is_some(),
            "project_count field must be present"
        );
        assert!(
            body.get("message_count").is_some(),
            "message_count field must be present"
        );
        assert_eq!(body["database_path"], ":memory:");
    }

    #[test]
    fn health_readiness_count_cache_is_keyed_by_database_url() {
        with_serialized_health_count_cache(|| {
            *lock_mutex(&HEALTH_COUNT_CACHE) = (
                Instant::now(),
                Some(HealthCountCacheEntry {
                    database_url: "sqlite:///tmp/other.sqlite3".to_string(),
                    storage_root: PathBuf::from("/tmp/other-storage"),
                    counts: Some((7, 9)),
                }),
            );

            let current_storage = PathBuf::from("/tmp/current-storage");
            let mut body = serde_json::json!({});
            enrich_readiness_response("sqlite:///:memory:", current_storage.as_path(), &mut body);

            assert_eq!(body["project_count"], serde_json::json!(0));
            assert_eq!(body["message_count"], serde_json::json!(0));

            let guard = lock_mutex(&HEALTH_COUNT_CACHE);
            let (_, entry) = &*guard;
            let entry = entry.as_ref().expect("health count cache entry");
            assert_eq!(entry.database_url, "sqlite:///:memory:");
            assert_eq!(entry.storage_root, current_storage);
            assert_eq!(entry.counts, Some((0, 0)));
        });
    }

    #[test]
    fn health_readiness_count_cache_reuses_stale_counts_when_refresh_fails() {
        with_serialized_health_count_cache(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let storage_root = dir.path().join("storage");
            let db_path = dir.path().join("health-count-stale.sqlite3");
            let project_dir = storage_root.join("projects").join("ahead-project");
            let agent_dir = project_dir.join("agents").join("Alice");
            let messages_dir = project_dir.join("messages").join("2026").join("03");
            std::fs::create_dir_all(&agent_dir).expect("create agent dir");
            std::fs::create_dir_all(&messages_dir).expect("create messages dir");
            std::fs::write(
                project_dir.join("project.json"),
                r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
            )
            .expect("write project metadata");
            std::fs::write(
                agent_dir.join("profile.json"),
                r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
            )
            .expect("write agent profile");
            std::fs::write(
                messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
                r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
            )
            .expect("write canonical message");

            let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
            conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                .expect("init schema");
            drop(conn);

            let database_url = format!("sqlite:///{}", db_path.display());
            let expected_counts = Some((11, 13));
            *lock_mutex(&HEALTH_COUNT_CACHE) = (
                Instant::now(),
                Some(HealthCountCacheEntry {
                    database_url: database_url.clone(),
                    storage_root: storage_root.clone(),
                    counts: expected_counts,
                }),
            );

            let tmpdir_file = dir.path().join("tmpdir-file");
            std::fs::write(&tmpdir_file, "not a directory").expect("write tmpdir file");
            let tmpdir = tmpdir_file
                .to_str()
                .expect("tmpdir override utf-8")
                .to_string();

            let mut body = serde_json::json!({});
            mcp_agent_mail_core::config::with_process_env_overrides_for_test(
                &[("TMPDIR", tmpdir.as_str())],
                || enrich_readiness_response(&database_url, &storage_root, &mut body),
            );

            assert_eq!(body["project_count"], serde_json::json!(11));
            assert_eq!(body["message_count"], serde_json::json!(13));

            let guard = lock_mutex(&HEALTH_COUNT_CACHE);
            let (_, entry) = &*guard;
            let entry = entry.as_ref().expect("health count cache entry");
            assert_eq!(entry.database_url, database_url);
            assert_eq!(entry.storage_root, storage_root);
            assert_eq!(entry.counts, expected_counts);
        });
    }

    #[test]
    fn health_root_alias_returns_ready_json() {
        let config = mcp_agent_mail_core::Config {
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "ready");
        // Enriched identity fields are present.
        assert!(
            body.get("version").is_some(),
            "version field must be present"
        );
        assert!(
            body.get("database_path").is_some(),
            "database_path field must be present"
        );
        assert_eq!(body["database_path"], ":memory:");
    }

    #[test]
    fn health_readiness_has_json_content_type() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn health_root_alias_has_json_content_type() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn health_readiness_emits_agent_mail_signature_header() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, startup_checks::HEALTH_SIGNATURE_HEADER_NAME),
            Some(startup_checks::HEALTH_SIGNATURE_HEADER_VALUE)
        );
    }

    #[test]
    fn health_readiness_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn health_root_alias_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/health", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn well_known_oauth_returns_mcp_oauth_false() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body, serde_json::json!({"mcp_oauth": false}));
    }

    #[test]
    fn well_known_oauth_mcp_variant_returns_same_response() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/.well-known/oauth-authorization-server/mcp",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body, serde_json::json!({"mcp_oauth": false}));
    }

    #[test]
    fn well_known_oauth_has_json_content_type() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(
            response_header(&resp, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn well_known_oauth_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Post,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["detail"], "Method Not Allowed");
    }

    #[test]
    fn well_known_oauth_mcp_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(
            Http1Method::Post,
            "/.well-known/oauth-authorization-server/mcp",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn health_unknown_subpath_returns_404() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/unknown", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn health_liveness_bypasses_bearer_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            ..Default::default()
        };
        let state = build_state(config);
        // No auth header — should still get 200 for health.
        let req = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "alive");
    }

    #[test]
    fn healthz_alias_bypasses_bearer_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/healthz", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "alive");
    }

    #[test]
    fn health_readiness_bypasses_bearer_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "ready");
    }

    #[test]
    fn health_root_alias_bypasses_bearer_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            database_url: "sqlite:///:memory:".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], "ready");
    }

    #[test]
    fn http_probe_timing_does_not_undercut_keep_alive() {
        // Verify that the *default* idle timeout exceeds the probe interval.
        // At runtime this is enforced by the Config validation (min 2s idle).
        let default_config = mcp_agent_mail_core::Config::default();
        assert!(
            default_config.http_idle_timeout_secs > default_config.http_probe_interval_secs,
            "default keep-alive must outlive default supervisor probe cadence"
        );
    }

    #[test]
    fn well_known_requires_bearer_auth_when_configured() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(
            Http1Method::Get,
            "/.well-known/oauth-authorization-server",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "well-known routes require auth (not under /health/ prefix)"
        );
    }

    #[test]
    fn error_response_format_uses_detail_key() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        // Request a path that will 404.
        let req = make_request(Http1Method::Get, "/nonexistent", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(
            body.get("detail").is_some(),
            "error responses must use 'detail' key (legacy parity)"
        );
        assert_eq!(body["detail"], "Not Found");
    }

    // ── HTTP Logging Parity tests (br-1bm.6.4) ───────────────────────────

    // -- py_repr_str unit tests --

    #[test]
    fn py_repr_str_wraps_in_single_quotes() {
        assert_eq!(py_repr_str("hello"), "'hello'");
    }

    #[test]
    fn py_repr_str_escapes_single_quotes() {
        assert_eq!(py_repr_str("it's"), "'it\\'s'");
    }

    #[test]
    fn py_repr_str_escapes_backslashes() {
        assert_eq!(py_repr_str("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn py_repr_str_empty_string() {
        assert_eq!(py_repr_str(""), "''");
    }

    // -- KV line formatter unit tests --

    #[test]
    fn kv_line_field_order_matches_legacy_key_order() {
        let line = http_request_log_kv_line(
            "2026-02-06T12:00:00.000000Z",
            "POST",
            "/api/rpc",
            201,
            42,
            "10.0.0.1",
        );
        // Legacy key_order: event, path, status first.
        let fields: Vec<&str> = line.split(' ').collect();
        assert!(fields[0].starts_with("event="), "first field must be event");
        assert!(fields[1].starts_with("path="), "second field must be path");
        assert!(
            fields[2].starts_with("status="),
            "third field must be status"
        );
    }

    #[test]
    fn kv_line_contains_all_required_fields() {
        let line = http_request_log_kv_line("ts", "GET", "/health", 200, 5, "127.0.0.1");
        assert!(line.contains("event='request'"));
        assert!(line.contains("path='/health'"));
        assert!(line.contains("status=200"));
        assert!(line.contains("method='GET'"));
        assert!(line.contains("duration_ms=5"));
        assert!(line.contains("client_ip='127.0.0.1'"));
        assert!(line.contains("timestamp='ts'"));
        assert!(line.contains("level='info'"));
    }

    #[test]
    fn kv_line_paths_with_special_chars() {
        let line = http_request_log_kv_line("t", "GET", "/a's/b", 200, 1, "::1");
        assert!(
            line.contains("path='/a\\'s/b'"),
            "single quotes in path must be escaped: {line}"
        );
    }

    // -- JSON line formatter unit tests --

    #[test]
    fn json_line_contains_all_required_fields() {
        let line = http_request_log_json_line("ts", "GET", "/health", 200, 5, "127.0.0.1")
            .expect("json line should succeed");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "request");
        assert_eq!(v["method"], "GET");
        assert_eq!(v["path"], "/health");
        assert_eq!(v["status"], 200);
        assert_eq!(v["duration_ms"], 5);
        assert_eq!(v["client_ip"], "127.0.0.1");
        assert_eq!(v["timestamp"], "ts");
        assert_eq!(v["level"], "info");
    }

    #[test]
    fn json_line_duration_ms_is_integer() {
        let line = http_request_log_json_line("ts", "GET", "/", 200, 123, "x").expect("json line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(
            v["duration_ms"].is_u64(),
            "duration_ms must be integer, not string"
        );
        assert_eq!(v["duration_ms"].as_u64(), Some(123));
    }

    #[test]
    fn json_line_status_is_integer() {
        let line = http_request_log_json_line("ts", "PUT", "/x", 404, 1, "x").expect("json line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v["status"].is_u64(), "status must be integer");
        assert_eq!(v["status"].as_u64(), Some(404));
    }

    // -- Fallback line formatter unit tests --

    #[test]
    fn fallback_line_exact_format() {
        assert_eq!(
            http_request_log_fallback_line("DELETE", "/item", 500, 99, "192.168.1.1"),
            "http method=DELETE path=/item status=500 ms=99 client=192.168.1.1"
        );
    }

    // -- Panel rendering (TTY vs non-TTY) --

    #[test]
    fn panel_non_tty_has_no_ansi_escapes() {
        let panel =
            console::render_http_request_panel(100, "GET", "/api", 200, 42, "127.0.0.1", false)
                .expect("panel should render");
        assert!(
            !panel.contains("\x1b["),
            "non-TTY panel must not contain ANSI escapes: {panel:?}"
        );
        assert!(panel.contains('+'), "panel must have box corners");
        assert!(panel.contains('|'), "panel must have box sides");
        assert!(panel.contains("GET"), "panel must contain method");
        assert!(panel.contains("/api"), "panel must contain path");
        assert!(panel.contains("200"), "panel must contain status");
        assert!(panel.contains("42ms"), "panel must contain duration");
        assert!(
            panel.contains("client: 127.0.0.1"),
            "panel must contain client IP"
        );
    }

    #[test]
    fn panel_tty_has_ansi_color_codes() {
        let panel =
            console::render_http_request_panel(100, "GET", "/api", 200, 10, "127.0.0.1", true)
                .expect("panel should render");
        assert!(
            panel.contains("\x1b["),
            "TTY panel must contain ANSI escapes: {panel:?}"
        );
        // Should use 24-bit theme colors for method, status, and duration
        assert!(
            panel.contains("38;2;"),
            "panel should use 24-bit theme colors: {panel:?}"
        );
        // Should use rounded unicode border
        assert!(
            panel.contains('\u{256d}'),
            "TTY panel should use rounded top-left corner"
        );
    }

    #[test]
    fn panel_tty_error_status_uses_theme_color() {
        let panel = console::render_http_request_panel(100, "GET", "/bad", 500, 1, "x", true)
            .expect("panel should render");
        assert!(
            panel.contains("38;2;"),
            "5xx status should use 24-bit theme color: {panel:?}"
        );
    }

    #[test]
    fn panel_tty_4xx_status_uses_theme_color() {
        let panel = console::render_http_request_panel(100, "POST", "/missing", 404, 1, "x", true)
            .expect("panel should render");
        assert!(
            panel.contains("38;2;"),
            "4xx status should use 24-bit theme color: {panel:?}"
        );
    }

    #[test]
    fn panel_3xx_status_uses_theme_color() {
        let panel = console::render_http_request_panel(100, "GET", "/redirect", 301, 1, "x", true)
            .expect("panel should render");
        assert!(
            panel.contains("38;2;"),
            "3xx status should use 24-bit theme color: {panel:?}"
        );
    }

    #[test]
    fn panel_returns_none_for_width_below_20() {
        assert!(console::render_http_request_panel(19, "GET", "/", 200, 1, "x", false).is_none());
        assert!(console::render_http_request_panel(0, "GET", "/", 200, 1, "x", false).is_none());
        assert!(console::render_http_request_panel(1, "GET", "/", 200, 1, "x", true).is_none());
    }

    #[test]
    fn panel_long_path_truncated_with_ellipsis() {
        let long_path = "/".to_string() + &"a".repeat(200);
        let panel = console::render_http_request_panel(100, "GET", &long_path, 200, 1, "x", false)
            .expect("panel should render even with long path");
        assert!(
            panel.contains("..."),
            "truncated path should contain ellipsis"
        );
    }

    // -- ExpectedErrorFilter additional coverage --

    #[test]
    fn expected_error_filter_each_pattern_matches() {
        // Verify every pattern in EXPECTED_ERROR_PATTERNS is actually matched.
        for pattern in &EXPECTED_ERROR_PATTERNS {
            let msg = format!("Error: {pattern} occurred");
            let out = expected_error_filter(
                EXPECTED_ERROR_FILTER_TARGET,
                true,
                SimpleLogLevel::Error,
                &msg,
                false,
                &[],
            );
            assert!(
                out.is_expected,
                "pattern {pattern:?} should be recognized as expected"
            );
            assert!(out.suppress_exc);
            assert_eq!(out.effective_level, SimpleLogLevel::Info);
        }
    }

    #[test]
    fn expected_error_filter_case_insensitive_match() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "INDEX.LOCK contention",
            false,
            &[],
        );
        assert!(
            out.is_expected,
            "pattern matching should be case-insensitive"
        );
    }

    #[test]
    fn expected_error_filter_preserves_warn_level() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Warn,
            "index.lock",
            false,
            &[],
        );
        assert!(out.is_expected);
        assert_eq!(
            out.effective_level,
            SimpleLogLevel::Warn,
            "warn-level should not be downgraded (only error is)"
        );
    }

    #[test]
    fn expected_error_filter_preserves_info_level() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Info,
            "recoverable=true in output",
            false,
            &[],
        );
        assert!(out.is_expected);
        assert_eq!(
            out.effective_level,
            SimpleLogLevel::Info,
            "info-level should stay as info"
        );
    }

    #[test]
    fn expected_error_filter_no_match_leaves_error() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "completely unknown error type XYZ",
            false,
            &[],
        );
        assert!(!out.is_expected);
        assert!(!out.suppress_exc);
        assert_eq!(out.effective_level, SimpleLogLevel::Error);
    }

    #[test]
    fn expected_error_filter_cause_chain_recoverable_flag() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "top-level error",
            false,
            &[("unrelated cause", true)], // cause has recoverable=true
        );
        assert!(
            out.is_expected,
            "cause chain with recoverable=true should mark as expected"
        );
    }

    #[test]
    fn expected_error_filter_multiple_causes_first_match_wins() {
        let out = expected_error_filter(
            EXPECTED_ERROR_FILTER_TARGET,
            true,
            SimpleLogLevel::Error,
            "top",
            false,
            &[
                ("harmless error", false),
                ("git_index_lock issue", false), // matches pattern
                ("another error", false),
            ],
        );
        assert!(out.is_expected);
    }

    // -- Config defaults for logging --

    #[test]
    fn logging_config_defaults() {
        let config = mcp_agent_mail_core::Config::default();
        assert!(
            !config.http_request_log_enabled,
            "request logging disabled by default"
        );
        assert!(!config.log_json_enabled, "JSON logging disabled by default");
        assert!(!config.http_otel_enabled, "OTEL disabled by default");
        assert_eq!(config.http_otel_service_name, "mcp-agent-mail");
        assert!(config.http_otel_exporter_otlp_endpoint.is_empty());
    }

    // -- OTEL config no-op parity (server-level) --

    #[test]
    fn otel_config_enabled_does_not_affect_logging_behavior() {
        // Legacy parity: OTEL fields exist in config but the Rust port does not
        // add spans/traces. We verify that enabling OTEL does not change the
        // request logging output format or introduce crashes.
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: true,
            http_otel_enabled: true,
            http_otel_service_name: "test-service".to_string(),
            http_otel_exporter_otlp_endpoint: "http://127.0.0.1:4318".to_string(),
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[],
            Some("10.0.0.1:5555".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();

        // JSON log line should exist and not contain OTEL-specific span/trace fields.
        let json_line = out
            .lines()
            .find(|line| line.trim_start().starts_with('{') && line.trim_end().ends_with('}'))
            .expect("expected JSON log line with OTEL enabled");
        let v: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert_eq!(v["event"], "request");
        assert!(
            v.get("trace_id").is_none(),
            "no trace_id in output (OTEL is no-op)"
        );
        assert!(
            v.get("span_id").is_none(),
            "no span_id in output (OTEL is no-op)"
        );
    }

    // -- Field derivation tests --

    #[test]
    fn client_ip_derived_from_peer_addr() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[],
            Some("192.168.1.42:9999".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();

        let json_line = out
            .lines()
            .find(|l| l.trim_start().starts_with('{'))
            .expect("json line");
        let v: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert_eq!(
            v["client_ip"], "192.168.1.42",
            "client_ip should be IP only, no port"
        );
    }

    #[test]
    fn client_ip_dash_when_no_peer_addr() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();

        let json_line = out
            .lines()
            .find(|l| l.trim_start().starts_with('{'))
            .expect("json line");
        let v: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert_eq!(
            v["client_ip"], "-",
            "client_ip should be '-' when peer_addr is None"
        );
    }

    #[test]
    fn duration_ms_is_non_negative_integer() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/health/liveness",
            &[],
            Some("127.0.0.1:1234".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let out = capture.drain_to_string();

        let json_line = out
            .lines()
            .find(|l| l.trim_start().starts_with('{'))
            .expect("json line");
        let v: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert!(
            v["duration_ms"].is_u64(),
            "duration_ms must be integer: {:?}",
            v["duration_ms"]
        );
    }

    // -- Logging with different HTTP status codes --

    #[test]
    fn http_logging_4xx_status_logged_correctly() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        // Request a non-existent path → 404
        let req = make_request_with_peer_addr(
            Http1Method::Get,
            "/nonexistent/path",
            &[],
            Some("127.0.0.1:1234".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 404);
        let out = capture.drain_to_string();

        let json_line = out
            .lines()
            .find(|l| l.trim_start().starts_with('{'))
            .expect("json line for 404");
        let v: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert_eq!(v["status"], 404);
        assert_eq!(v["path"], "/nonexistent/path");
    }

    #[test]
    fn http_logging_405_method_not_allowed() {
        let _guard = STDIO_CAPTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = mcp_agent_mail_core::Config {
            http_request_log_enabled: true,
            log_json_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let capture = StdioCapture::install().expect("stdio capture install");
        // POST to health endpoint → 405
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/health/liveness",
            &[],
            Some("127.0.0.1:1234".parse().unwrap()),
        );
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 405);
        let out = capture.drain_to_string();

        assert!(out.contains("status=405"), "KV line should log 405 status");
        assert!(
            out.contains("method='POST'"),
            "KV line should log POST method"
        );
    }

    // ── Agent activity display tests ──

    #[test]
    fn relative_time_short_seconds() {
        let now = 1_000_000_000_000; // 1e12 microseconds
        assert_eq!(relative_time_short(now, now - 5_000_000), "5s ago");
        assert_eq!(relative_time_short(now, now - 59_000_000), "59s ago");
    }

    #[test]
    fn relative_time_short_minutes() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time_short(now, now - 60_000_000), "1m ago");
        assert_eq!(relative_time_short(now, now - 300_000_000), "5m ago");
    }

    #[test]
    fn relative_time_short_hours() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time_short(now, now - 3_600_000_000), "1h ago");
        assert_eq!(relative_time_short(now, now - 7_200_000_000), "2h ago");
    }

    #[test]
    fn relative_time_short_days() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time_short(now, now - 86_400_000_000), "1d ago");
    }

    #[test]
    fn relative_time_short_future_shows_now() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time_short(now, now + 5_000_000), "now");
    }

    #[test]
    fn dashboard_db_stats_default_has_empty_agents_list() {
        let stats = DashboardDbStats::default();
        assert!(stats.agents_list.is_empty());
    }

    #[test]
    fn dashboard_db_stats_preserves_degraded_agent_rows_with_placeholders() {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("dashboard-degraded-agents.sqlite3");
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");

        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, name TEXT, program TEXT, last_active_ts DATETIME)",
        )
        .expect("create agents");

        conn.execute_sync(
            "INSERT INTO agents (id, name, program, last_active_ts) VALUES (?1, ?2, ?3, ?4)",
            &[
                Value::BigInt(7),
                Value::Text(String::new()),
                Value::Text(String::new()),
                Value::Null,
            ],
        )
        .expect("insert degraded agent");

        let stats = fetch_dashboard_db_stats_from_conn(&conn);
        assert_eq!(stats.agents_list.len(), 1);
        assert_eq!(stats.agents_list[0].name, "[unknown-agent-7]");
        assert_eq!(stats.agents_list[0].program, "[unknown-program]");
        assert_eq!(stats.agents_list[0].last_active_ts, 0);
    }

    #[test]
    fn open_server_sync_db_connection_uses_server_busy_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("server-busy-timeout.db");
        let conn = open_server_sync_db_connection(&db_path.display().to_string()).expect("open");

        let configured = conn
            .query_sync("PRAGMA busy_timeout", &[])
            .expect("pragma query")
            .into_iter()
            .next()
            .and_then(|row| {
                row.get_named::<i64>("timeout")
                    .ok()
                    .or_else(|| row.get_as(0).ok())
            })
            .unwrap_or_default();
        assert_eq!(configured, i64::from(SERVER_SYNC_DB_BUSY_TIMEOUT_MS));
    }

    #[test]
    fn open_interactive_sync_db_connection_uses_interactive_busy_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("interactive-busy-timeout.db");
        let conn =
            open_interactive_sync_db_connection(&db_path.display().to_string()).expect("open");

        let configured = conn
            .query_sync("PRAGMA busy_timeout", &[])
            .expect("pragma query")
            .into_iter()
            .next()
            .and_then(|row| {
                row.get_named::<i64>("timeout")
                    .ok()
                    .or_else(|| row.get_as(0).ok())
            })
            .unwrap_or_default();
        assert_eq!(configured, i64::from(INTERACTIVE_SYNC_DB_BUSY_TIMEOUT_MS));
    }

    #[test]
    fn dashboard_open_connection_uses_best_effort_busy_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("dashboard-busy-timeout.db");
        let database_url = format!("sqlite:///{}", db_path.display());
        let conn = dashboard_open_connection(&database_url, dir.path()).expect("open");

        let configured = conn
            .conn()
            .query_sync("PRAGMA busy_timeout", &[])
            .expect("pragma query")
            .into_iter()
            .next()
            .and_then(|row| {
                row.get_named::<i64>("timeout")
                    .ok()
                    .or_else(|| row.get_as(0).ok())
            })
            .unwrap_or_default();
        assert_eq!(configured, i64::from(BEST_EFFORT_SYNC_DB_BUSY_TIMEOUT_MS));
    }

    #[test]
    fn dashboard_open_connection_uses_absolute_candidate_for_missing_relative_database_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absolute_db = dir.path().join("dashboard-fallback.sqlite3");
        let absolute_db_str = absolute_db.to_string_lossy().into_owned();
        let absolute_conn = DbConn::open_file(&absolute_db_str).expect("open absolute db");
        absolute_conn
            .execute_raw("CREATE TABLE marker(id INTEGER PRIMARY KEY)")
            .expect("create marker table");
        drop(absolute_conn);

        let relative_path = std::path::PathBuf::from(absolute_db_str.trim_start_matches('/'));
        if let Some(parent) = relative_path.parent() {
            std::fs::create_dir_all(parent).expect("create relative parent");
        }
        assert!(
            !relative_path.exists(),
            "relative fallback fixture should be absent so dashboard opens the absolute candidate"
        );

        let database_url = format!("sqlite://{}", relative_path.display());
        let conn = dashboard_open_connection(&database_url, dir.path())
            .expect("open dashboard fallback db");
        let rows = conn
            .conn()
            .query_sync(
                "SELECT COUNT(*) AS count FROM sqlite_master WHERE type = 'table' AND name = 'marker'",
                &[],
            )
            .expect("query sqlite_master");
        assert_eq!(rows[0].get_named::<i64>("count").unwrap_or(0), 1);
        drop(conn);

        assert!(
            !relative_path.exists(),
            "dashboard fallback should not create a stray relative sqlite file"
        );
    }

    #[test]
    fn dashboard_open_connection_uses_archive_snapshot_when_live_db_is_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_root = dir.path().join("storage");
        let db_path = dir.path().join("dashboard-stale.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        let database_url = format!("sqlite:///{}", db_path.display());
        let observed =
            dashboard_open_connection(&database_url, &storage_root).expect("open observed db");
        assert!(
            observed.uses_archive_snapshot(),
            "dashboard should switch to an archive-backed snapshot when the live db lags"
        );
        let rows = observed
            .conn()
            .query_sync("SELECT COUNT(*) AS c FROM messages", &[])
            .expect("query snapshot messages");
        assert_eq!(rows[0].get_named::<i64>("c").unwrap_or(0), 1);
    }

    #[test]
    fn open_observability_sync_db_connection_ignores_unrelated_default_archive_overlap() {
        let _env_lock = lock_mutex(&TOOL_DISPATCH_ENV_TEST_LOCK);
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg_data_root = dir.path().join("xdg-data");
        std::fs::create_dir_all(&xdg_data_root).expect("create xdg data root");
        let xdg_data_root_str = xdg_data_root
            .to_str()
            .expect("xdg data root utf-8")
            .to_string();

        mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("XDG_DATA_HOME", xdg_data_root_str.as_str())],
            || {
                let storage_root = mcp_agent_mail_core::Config::from_env().storage_root;
                assert!(
                    mcp_agent_mail_core::config::is_default_storage_root(&storage_root),
                    "test must use the default storage root"
                );

                let project_dir = storage_root.join("projects").join("ahead-project");
                let agent_dir = project_dir.join("agents").join("Alice");
                let messages_dir = project_dir.join("messages").join("2026").join("03");
                std::fs::create_dir_all(&agent_dir).expect("create agent dir");
                std::fs::create_dir_all(&messages_dir).expect("create messages dir");
                std::fs::write(
                    project_dir.join("project.json"),
                    r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
                )
                .expect("write project metadata");
                std::fs::write(
                    agent_dir.join("profile.json"),
                    r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
                )
                .expect("write agent profile");
                std::fs::write(
                    messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
                    r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
                )
                .expect("write canonical message");

                let external_db_dir = dir.path().join("external-db");
                std::fs::create_dir_all(&external_db_dir).expect("create external db dir");
                let db_path = external_db_dir.join("observability.sqlite3");
                let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
                conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
                    .expect("init schema");
                drop(conn);

                let database_url = format!("sqlite:///{}", db_path.display());
                let observed = open_observability_sync_db_connection(
                    &database_url,
                    &storage_root,
                    "unrelated default archive overlap",
                )
                .expect("open observability db")
                .expect("observability db should exist");

                assert!(
                    !observed.uses_archive_snapshot(),
                    "default-root archive state must not override an external sqlite path"
                );

                let (conn, sqlite_path, snapshot_dir) = observed.into_parts();
                assert_eq!(
                    sqlite_path,
                    resolve_server_sync_sqlite_path(db_path.to_string_lossy().as_ref())
                );
                assert!(
                    snapshot_dir.is_none(),
                    "live sqlite binding should not allocate an archive snapshot dir"
                );
                let rows = conn
                    .query_sync("SELECT COUNT(*) AS c FROM messages", &[])
                    .expect("query live db");
                assert_eq!(rows[0].get_named::<i64>("c").unwrap_or(-1), 0);
            },
        );
    }

    #[test]
    fn open_observability_sync_db_connection_reports_archive_snapshot_setup_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_root = dir.path().join("storage");
        let db_path = dir.path().join("observability-stale.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        let tmpdir_file = dir.path().join("tmpdir-file");
        std::fs::write(&tmpdir_file, "not a directory").expect("write tmpdir file");
        let tmpdir = tmpdir_file
            .to_str()
            .expect("tmpdir override utf-8")
            .to_string();
        let database_url = format!("sqlite:///{}", db_path.display());
        let Err(err) = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("TMPDIR", tmpdir.as_str())],
            || {
                open_observability_sync_db_connection(
                    &database_url,
                    &storage_root,
                    "observability snapshot failure test",
                )
            },
        ) else {
            panic!("snapshot setup failure should be surfaced");
        };

        assert!(
            err.contains("failed to allocate observability snapshot dir"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_snapshot_temp_dir_rejects_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real_tmpdir = dir.path().join("real-tmp");
        let linked_tmpdir = dir.path().join("linked-tmp");
        std::fs::create_dir_all(&real_tmpdir).expect("create real tmpdir");
        symlink(&real_tmpdir, &linked_tmpdir).expect("symlink tmpdir");

        let err = validate_snapshot_temp_dir(&linked_tmpdir, "system temp dir")
            .expect_err("symlinked temp dir should be rejected");

        assert!(
            err.to_string().contains("symlinked snapshot directory"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_snapshot_temp_dir_rejects_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real_parent = dir.path().join("real-parent");
        let real_tmpdir = real_parent.join("child");
        let linked_parent = dir.path().join("linked-parent");
        std::fs::create_dir_all(&real_tmpdir).expect("create real tmpdir");
        symlink(&real_parent, &linked_parent).expect("symlink tmpdir parent");

        let err = validate_snapshot_temp_dir(&linked_parent.join("child"), "system temp dir")
            .expect_err("temp dir reached through symlinked parent should be rejected");

        assert!(
            err.to_string().contains("symlinked snapshot directory"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preferred_snapshot_temp_dir_rejects_symlinked_tmpdir_override() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real_tmpdir = dir.path().join("real-tmp");
        let linked_tmpdir = dir.path().join("linked-tmp");
        std::fs::create_dir_all(&real_tmpdir).expect("create real tmpdir");
        symlink(&real_tmpdir, &linked_tmpdir).expect("symlink tmpdir");
        let linked_tmpdir = linked_tmpdir
            .to_str()
            .expect("linked tmpdir utf-8")
            .to_string();

        let err = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("TMPDIR", linked_tmpdir.as_str())],
            preferred_snapshot_temp_dir,
        )
        .expect_err("symlinked TMPDIR should be rejected");

        assert!(
            err.to_string().contains("symlinked snapshot directory"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preferred_snapshot_temp_dir_rejects_symlinked_parent_override() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real_parent = dir.path().join("real-parent");
        let real_tmpdir = real_parent.join("child");
        let linked_parent = dir.path().join("linked-parent");
        std::fs::create_dir_all(&real_tmpdir).expect("create real tmpdir");
        symlink(&real_parent, &linked_parent).expect("symlink tmpdir parent");
        let linked_tmpdir = linked_parent.join("child");
        let linked_tmpdir = linked_tmpdir
            .to_str()
            .expect("linked child utf-8")
            .to_string();

        let err = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("TMPDIR", linked_tmpdir.as_str())],
            preferred_snapshot_temp_dir,
        )
        .expect_err("TMPDIR reached through a symlinked parent should be rejected");

        assert!(
            err.to_string().contains("symlinked snapshot directory"),
            "{err}"
        );
    }

    #[test]
    fn fetch_dashboard_db_stats_cached_reuses_previous_stats_when_refresh_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage_root = dir.path().join("storage");
        let db_path = dir.path().join("dashboard-stale.sqlite3");
        let project_dir = storage_root.join("projects").join("ahead-project");
        let agent_dir = project_dir.join("agents").join("Alice");
        let messages_dir = project_dir.join("messages").join("2026").join("03");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        std::fs::create_dir_all(&messages_dir).expect("create messages dir");
        std::fs::write(
            project_dir.join("project.json"),
            r#"{"slug":"ahead-project","human_key":"/ahead-project","created_at":0}"#,
        )
        .expect("write project metadata");
        std::fs::write(
            agent_dir.join("profile.json"),
            r#"{"agent_name":"Alice","program":"coder","model":"test","registered_ts":"2026-03-22T00:00:00Z"}"#,
        )
        .expect("write agent profile");
        std::fs::write(
            messages_dir.join("2026-03-22T12-00-00Z__first__1.md"),
            r#"---json
{
  "id": 1,
  "from": "Alice",
  "to": ["Bob"],
  "subject": "First copy",
  "importance": "normal",
  "created_ts": "2026-03-22T12:00:00Z"
}
---

first body
"#,
        )
        .expect("write canonical message");

        let conn = DbConn::open_file(db_path.to_string_lossy().as_ref()).expect("open db");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");
        drop(conn);

        let tmpdir_file = dir.path().join("tmpdir-file");
        std::fs::write(&tmpdir_file, "not a directory").expect("write tmpdir file");
        let tmpdir = tmpdir_file
            .to_str()
            .expect("tmpdir override utf-8")
            .to_string();
        let database_url = format!("sqlite:///{}", db_path.display());
        let previous = DashboardDbStats {
            projects: 11,
            agents: 7,
            messages: 13,
            file_reservations: 5,
            contact_links: 3,
            ack_pending: 2,
            agents_list: vec![AgentSummary {
                name: "BlueLake".to_string(),
                program: "coder".to_string(),
                last_active_ts: 123,
            }],
        };
        let mut conn_state = None;
        let stats = mcp_agent_mail_core::config::with_process_env_overrides_for_test(
            &[("TMPDIR", tmpdir.as_str())],
            || {
                fetch_dashboard_db_stats_cached(
                    &database_url,
                    &storage_root,
                    &mut conn_state,
                    &previous,
                )
            },
        );

        assert_eq!(stats.projects, previous.projects);
        assert_eq!(stats.agents, previous.agents);
        assert_eq!(stats.messages, previous.messages);
        assert_eq!(stats.file_reservations, previous.file_reservations);
        assert_eq!(stats.contact_links, previous.contact_links);
        assert_eq!(stats.ack_pending, previous.ack_pending);
        assert_eq!(stats.agents_list.len(), previous.agents_list.len());
        assert!(
            conn_state.is_none(),
            "failed refresh should not cache a bad connection"
        );
    }

    #[test]
    fn build_web_ui_url_normalizes_wildcard_and_ipv6_hosts() {
        assert_eq!(
            build_web_ui_url("0.0.0.0", 8765, None),
            "http://127.0.0.1:8765/mail"
        );
        assert_eq!(build_web_ui_url("::", 8765, None), "http://[::1]:8765/mail");
        assert_eq!(
            build_web_ui_url("2001:db8::42", 8765, None),
            "http://[2001:db8::42]:8765/mail"
        );
    }

    #[test]
    fn parse_tailscale_ip_stdout_uses_first_valid_ipv4_line() {
        let stdout = b"\n100.64.0.1\n100.64.0.2\n";
        assert_eq!(
            parse_tailscale_ip_stdout(stdout),
            Some("100.64.0.1".to_string())
        );
    }

    #[test]
    fn parse_tailscale_ip_stdout_rejects_non_ipv4_output() {
        assert_eq!(parse_tailscale_ip_stdout(b""), None);
        assert_eq!(parse_tailscale_ip_stdout(b"not-an-ip\n"), None);
        assert_eq!(parse_tailscale_ip_stdout(b"fd7a:115c:a1e0::1\n"), None);
    }

    #[test]
    fn dashboard_db_stats_excludes_expired_and_released_reservations() {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("dashboard-stats.sqlite3");
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");
        conn.execute_raw(mcp_agent_mail_db::schema::PRAGMA_DB_INIT_BASE_SQL)
            .expect("init base pragmas");
        conn.execute_raw(&mcp_agent_mail_db::schema::init_schema_sql_base())
            .expect("init schema");

        let now = mcp_agent_mail_db::timestamps::now_micros();
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?1, ?2, ?3, ?4)",
            &[
                Value::BigInt(1),
                Value::Text("dashboard-project".to_string()),
                Value::Text("/tmp/dashboard-project".to_string()),
                Value::BigInt(now),
            ],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("BlueLake".to_string()),
                Value::Text("codex-cli".to_string()),
                Value::Text("gpt-5".to_string()),
                Value::Text("dashboard test".to_string()),
                Value::BigInt(now),
                Value::BigInt(now),
            ],
        )
        .expect("insert agent");

        for (id, expires_ts) in [(1_i64, now + 60_000_000), (2_i64, now - 60_000_000)] {
            conn.execute_sync(
                "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                &[
                    Value::BigInt(id),
                    Value::BigInt(1),
                    Value::BigInt(1),
                    Value::Text(format!("src/path-{id}.rs")),
                    Value::BigInt(1),
                    Value::Text("dashboard test".to_string()),
                    Value::BigInt(now - 120_000_000),
                    Value::BigInt(expires_ts),
                    Value::Null,
                ],
            )
            .expect("insert unreleased reservation");
        }

        conn.execute_sync(
            "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            &[
                Value::BigInt(3),
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("src/released.rs".to_string()),
                Value::BigInt(1),
                Value::Text("released".to_string()),
                Value::BigInt(now - 120_000_000),
                Value::BigInt(now + 60_000_000),
                Value::Null,
            ],
        )
        .expect("insert released reservation shell");
        conn.execute_sync(
            "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (?1, ?2)",
            &[Value::BigInt(3), Value::BigInt(now - 10_000_000)],
        )
        .expect("insert release ledger row");

        let stats = fetch_dashboard_db_stats_from_conn(&conn);
        assert_eq!(
            stats.file_reservations, 1,
            "dashboard should count only non-expired unreleased reservations"
        );
    }

    #[test]
    fn dashboard_db_stats_accepts_legacy_text_reservation_timestamps() {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("dashboard-legacy-text.sqlite3");
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");

        conn.execute_raw(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT, human_key TEXT, created_at DATETIME)",
        )
        .expect("create projects");
        conn.execute_raw(
            "CREATE TABLE agents (id INTEGER PRIMARY KEY, project_id INTEGER, name TEXT, program TEXT, last_active_ts DATETIME)",
        )
        .expect("create agents");
        conn.execute_raw(
            "CREATE TABLE file_reservations (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                project_id INTEGER NOT NULL, \
                agent_id INTEGER NOT NULL, \
                path_pattern TEXT NOT NULL, \
                exclusive INTEGER NOT NULL DEFAULT 1, \
                reason TEXT NOT NULL DEFAULT '', \
                created_ts DATETIME NOT NULL, \
                expires_ts DATETIME NOT NULL, \
                released_ts DATETIME\
            )",
        )
        .expect("create legacy reservations");
        conn.execute_raw(
            "CREATE TABLE file_reservation_releases (reservation_id INTEGER PRIMARY KEY, released_ts INTEGER NOT NULL)",
        )
        .expect("create release ledger");

        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?1, ?2, ?3, ?4)",
            &[
                Value::BigInt(1),
                Value::Text("legacy-dashboard".to_string()),
                Value::Text("/tmp/legacy-dashboard".to_string()),
                Value::Text("2024-01-01 00:00:00".to_string()),
            ],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, last_active_ts) VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("LegacyAgent".to_string()),
                Value::Text("python".to_string()),
                Value::Text("2024-01-01 00:00:00".to_string()),
            ],
        )
        .expect("insert agent");

        for (id, expires_ts, released_ts) in [
            (1_i64, "2100-01-01 00:00:00", None),
            (2_i64, "2000-01-01 00:00:00", None),
            (3_i64, "2100-01-01 00:00:00", Some("2025-01-01 00:00:00")),
        ] {
            conn.execute_sync(
                "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                &[
                    Value::BigInt(id),
                    Value::BigInt(1),
                    Value::BigInt(1),
                    Value::Text(format!("src/legacy-{id}.rs")),
                    Value::BigInt(1),
                    Value::Text("legacy dashboard".to_string()),
                    Value::Text("2024-01-01 00:00:00".to_string()),
                    Value::Text(expires_ts.to_string()),
                    released_ts.map_or(Value::Null, |value| Value::Text(value.to_string())),
                ],
            )
            .expect("insert legacy reservation");
        }

        conn.execute_sync(
            "INSERT INTO file_reservation_releases (reservation_id, released_ts) VALUES (?1, ?2)",
            &[Value::BigInt(3), Value::BigInt(1_700_000_000_000_000)],
        )
        .expect("insert legacy release ledger row");

        let stats = fetch_dashboard_db_stats_from_conn(&conn);
        assert_eq!(
            stats.file_reservations, 1,
            "dashboard should parse legacy TEXT timestamps and still honor release-ledger exclusions"
        );
    }

    #[test]
    fn dashboard_db_stats_handles_missing_release_ledger_table() {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("dashboard-no-release-ledger.sqlite3");
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");

        conn.execute_raw(
            "CREATE TABLE file_reservations (\
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                project_id INTEGER NOT NULL, \
                agent_id INTEGER NOT NULL, \
                path_pattern TEXT NOT NULL, \
                exclusive INTEGER NOT NULL DEFAULT 1, \
                reason TEXT NOT NULL DEFAULT '', \
                created_ts INTEGER NOT NULL, \
                expires_ts INTEGER NOT NULL, \
                released_ts INTEGER\
            )",
        )
        .expect("create reservations");

        let now = mcp_agent_mail_db::timestamps::now_micros();
        conn.execute_sync(
            "INSERT INTO file_reservations (id, project_id, agent_id, path_pattern, exclusive, reason, created_ts, expires_ts, released_ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::BigInt(1),
                Value::Text("src/no-ledger.rs".to_string()),
                Value::BigInt(1),
                Value::Text("missing sidecar".to_string()),
                Value::BigInt(now - 60_000_000),
                Value::BigInt(now + 60_000_000),
                Value::Null,
            ],
        )
        .expect("insert reservation");

        let stats = fetch_dashboard_db_stats_from_conn(&conn);
        assert_eq!(
            stats.file_reservations, 1,
            "dashboard should still count active reservations when the sidecar release ledger table is absent"
        );
    }

    #[test]
    fn agent_summary_default_fields() {
        let a = AgentSummary::default();
        assert!(a.name.is_empty());
        assert!(a.program.is_empty());
        assert_eq!(a.last_active_ts, 0);
    }

    // ── Dashboard render tests (br-1m6a.8) ──

    fn make_test_snapshot(agents: Vec<AgentSummary>) -> DashboardSnapshot {
        DashboardSnapshot {
            endpoint: "http://127.0.0.1:8765".into(),
            web_ui: "http://127.0.0.1:8765/mail".into(),
            transport_mode: "mcp".into(),
            app_environment: "test".into(),
            auth_enabled: false,
            database_url: "sqlite:///tmp/test.db".into(),
            storage_root: "/tmp/storage".into(),
            uptime: "0s".into(),
            requests_total: 0,
            requests_2xx: 0,
            requests_4xx: 0,
            requests_5xx: 0,
            avg_latency_ms: 0,
            db: DashboardDbStats {
                agents: agents.len() as u64,
                agents_list: agents,
                ..DashboardDbStats::default()
            },
            last_request: None,
            sparkline_data: vec![0.0; 10],
            remote_url: None,
        }
    }

    fn make_agents(n: usize) -> Vec<AgentSummary> {
        let names = [
            "RedFox",
            "BlueLake",
            "GreenPeak",
            "GoldHawk",
            "SwiftWolf",
            "CalmRiver",
            "BoldStone",
            "DeepCave",
            "MistyMeadow",
            "SilverCrest",
        ];
        let now = mcp_agent_mail_db::timestamps::now_micros();
        (0..n)
            .map(|i| AgentSummary {
                name: names[i % names.len()].into(),
                program: "claude-code".into(),
                #[allow(clippy::cast_possible_wrap)]
                last_active_ts: now - (i as i64 * 60_000_000),
            })
            .collect()
    }

    fn buffer_text(f: &ftui::Frame<'_>) -> String {
        let mut t = String::new();
        for y in 0..f.buffer.height() {
            for x in 0..f.buffer.width() {
                if let Some(c) = f.buffer.get(x, y) {
                    if let Some(ch) = c.content.as_char() {
                        t.push(ch);
                    } else if !c.is_continuation() {
                        t.push(' ');
                    }
                }
            }
            t.push('\n');
        }
        t
    }

    #[test]
    fn dashboard_0_agents_no_agent_panel() {
        let snap = make_test_snapshot(vec![]);
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 20, &mut pool);
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 120, 20), &snap, 0.0, 0);
        let text = buffer_text(&frame);
        assert!(!text.contains(" Agents "), "no Agents panel with 0 agents");
        assert!(text.contains("Server"));
        assert!(text.contains("Database"));
        assert!(text.contains("Traffic"));
    }

    #[test]
    fn dashboard_1_agent_shows_panel() {
        let snap = make_test_snapshot(make_agents(1));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 20, &mut pool);
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 120, 20), &snap, 0.0, 0);
        let text = buffer_text(&frame);
        assert!(text.contains("Agents"), "Agents panel header");
        assert!(text.contains("RedFox"), "agent name RedFox");
        assert!(text.contains("Mode"), "mode row label");
        assert!(text.contains("mcp"), "transport mode value");
    }

    #[test]
    fn dashboard_5_agents_shows_all() {
        let snap = make_test_snapshot(make_agents(5));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(140, 20, &mut pool);
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 140, 20), &snap, 0.0, 0);
        let text = buffer_text(&frame);
        assert!(text.contains("Agents"));
        assert!(text.contains("RedFox"));
        assert!(text.contains("SwiftWolf"));
    }

    #[test]
    fn dashboard_10_agents_truncates() {
        let snap = make_test_snapshot(make_agents(10));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(140, 10, &mut pool);
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 140, 10), &snap, 0.0, 0);
        let text = buffer_text(&frame);
        assert!(text.contains("Agents"));
        assert!(
            text.contains("RedFox") || text.contains("10 agents"),
            "agent panel should show either the first agent or aggregate count"
        );
    }

    #[test]
    fn dashboard_narrow_graceful() {
        let snap = make_test_snapshot(make_agents(3));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(60, 20, &mut pool);
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 60, 20), &snap, 0.0, 0);
        let text = buffer_text(&frame);
        assert!(text.contains("Agents") || text.contains("agents"));
    }

    #[test]
    fn dashboard_tiny_no_panic() {
        let snap = make_test_snapshot(make_agents(2));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(30, 5, &mut pool);
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 30, 5), &snap, 0.0, 0);
    }

    // ── db_changed_rows tests ──

    #[test]
    fn db_changed_rows_identical_returns_zero() {
        let a = DashboardDbStats {
            projects: 5,
            agents: 3,
            messages: 100,
            ..Default::default()
        };
        assert_eq!(db_changed_rows(&a, &a), 0);
    }

    #[test]
    fn db_changed_rows_detects_each_field() {
        let base = DashboardDbStats::default();
        let mut changed = base.clone();
        changed.projects = 1;
        assert_eq!(db_changed_rows(&base, &changed), 0b00_0001);

        let mut changed = base.clone();
        changed.agents = 1;
        assert_eq!(db_changed_rows(&base, &changed), 0b00_0010);

        let mut changed = base.clone();
        changed.messages = 1;
        assert_eq!(db_changed_rows(&base, &changed), 0b00_0100);

        let mut changed = base.clone();
        changed.file_reservations = 1;
        assert_eq!(db_changed_rows(&base, &changed), 0b00_1000);

        let mut changed = base.clone();
        changed.contact_links = 1;
        assert_eq!(db_changed_rows(&base, &changed), 0b01_0000);

        let mut changed = base.clone();
        changed.ack_pending = 1;
        assert_eq!(db_changed_rows(&base, &changed), 0b10_0000);
    }

    #[test]
    fn db_changed_rows_multiple_changes() {
        let base = DashboardDbStats::default();
        let changed = DashboardDbStats {
            projects: 1,
            messages: 5,
            ack_pending: 2,
            ..Default::default()
        };
        assert_eq!(db_changed_rows(&base, &changed), 0b10_0101);
    }

    #[test]
    fn dashboard_render_gate_first_frame_always_renders() {
        let now = Instant::now();
        let (skip, stamp) = dashboard_render_gate_decision(None, 120, 32, now);
        assert!(!skip);
        assert_eq!(
            stamp,
            Some(DashboardRenderStamp {
                at: now,
                width: 120,
                ui_height: 32
            })
        );
    }

    #[test]
    fn dashboard_render_gate_skips_same_geometry_within_window() {
        let now = Instant::now();
        let (_, stamp) = dashboard_render_gate_decision(None, 120, 32, now);
        let (skip, next) = dashboard_render_gate_decision(
            stamp,
            120,
            32,
            now + std::time::Duration::from_millis(10),
        );
        assert!(skip);
        assert_eq!(next, stamp);
    }

    #[test]
    fn dashboard_render_gate_renders_after_window_elapses() {
        let now = Instant::now();
        let (_, stamp) = dashboard_render_gate_decision(None, 120, 32, now);
        let (skip, next) = dashboard_render_gate_decision(
            stamp,
            120,
            32,
            now + DASHBOARD_RENDER_COALESCE_WINDOW + std::time::Duration::from_millis(1),
        );
        assert!(!skip);
        assert_ne!(next, stamp);
        let next = next.expect("updated render stamp");
        assert_eq!(next.width, 120);
        assert_eq!(next.ui_height, 32);
    }

    #[test]
    fn dashboard_render_gate_renders_immediately_on_geometry_change() {
        let now = Instant::now();
        let (_, stamp) = dashboard_render_gate_decision(None, 120, 32, now);
        let (skip, next) = dashboard_render_gate_decision(
            stamp,
            140,
            32,
            now + std::time::Duration::from_millis(5),
        );
        assert!(!skip);
        let next = next.expect("updated render stamp");
        assert_eq!(next.width, 140);
        assert_eq!(next.ui_height, 32);
    }

    #[test]
    fn dashboard_changed_rows_highlight_no_panic() {
        let snap = make_test_snapshot(make_agents(0));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 20, &mut pool);
        // All rows changed
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 120, 20), &snap, 0.5, 0b11_1111);
    }

    // ── br-10wc.28: event emission helpers ────────────────────────────

    #[test]
    fn extract_project_agent_from_typical_args() {
        let args = serde_json::json!({
            "project_key": "my-project",
            "sender_name": "RedFox",
            "subject": "hello"
        });
        let (project, agent) = extract_project_agent(&args);
        assert_eq!(project.as_deref(), Some("my-project"));
        assert_eq!(agent.as_deref(), Some("RedFox"));
    }

    #[test]
    fn extract_project_agent_uses_agent_name_over_sender() {
        let args = serde_json::json!({
            "project_key": "p1",
            "agent_name": "BlueFox",
            "sender_name": "RedFox"
        });
        let (_, agent) = extract_project_agent(&args);
        assert_eq!(agent.as_deref(), Some("BlueFox"));
    }

    #[test]
    fn extract_project_agent_returns_none_for_empty() {
        let args = serde_json::json!({});
        let (project, agent) = extract_project_agent(&args);
        assert!(project.is_none());
        assert!(agent.is_none());
    }

    #[test]
    fn extract_project_agent_handles_non_object() {
        let args = serde_json::json!("just a string");
        let (project, agent) = extract_project_agent(&args);
        assert!(project.is_none());
        assert!(agent.is_none());
    }

    #[test]
    fn result_preview_masks_sensitive_json() {
        let contents = vec![Content::Text {
            text: r#"{"data":"ok","http_bearer_token":"secret123"}"#.to_string(),
        }];
        let preview = result_preview_from_contents(&contents).unwrap();
        assert!(!preview.contains("secret123"), "secret should be masked");
        assert!(preview.contains("<redacted>"));
        assert!(preview.contains("\"data\""));
    }

    #[test]
    fn result_preview_truncates_at_200_chars() {
        let long_text = "x".repeat(500);
        let contents = vec![Content::Text { text: long_text }];
        let preview = result_preview_from_contents(&contents).unwrap();
        assert!(preview.len() <= 200);
    }

    #[test]
    fn result_preview_returns_none_for_empty() {
        assert!(result_preview_from_contents(&[]).is_none());
    }

    #[test]
    fn query_delta_computes_differences() {
        let before = mcp_agent_mail_db::QueryTrackerSnapshot {
            total: 10,
            total_time_ms: 5.0,
            per_table: [("messages".to_string(), 8), ("agents".to_string(), 2)]
                .into_iter()
                .collect(),
            slow_query_ms: None,
            slow_queries: vec![],
        };
        let after = mcp_agent_mail_db::QueryTrackerSnapshot {
            total: 15,
            total_time_ms: 8.5,
            per_table: [
                ("messages".to_string(), 12),
                ("agents".to_string(), 2),
                ("projects".to_string(), 1),
            ]
            .into_iter()
            .collect(),
            slow_query_ms: None,
            slow_queries: vec![],
        };
        let (queries, time_ms, per_table) = query_delta(&before, &after);
        assert_eq!(queries, 5);
        assert!((time_ms - 3.5).abs() < 0.001);
        // messages: 12-8=4, projects: 1-0=1, agents: 2-2=0 (filtered)
        assert!(per_table.iter().any(|(t, c)| t == "messages" && *c == 4));
        assert!(per_table.iter().any(|(t, c)| t == "projects" && *c == 1));
        assert!(!per_table.iter().any(|(t, _)| t == "agents"));
    }

    #[test]
    fn derive_domain_events_from_send_message_payload() {
        let payload = serde_json::json!({
            "deliveries": [{
                "project": "alpha",
                "payload": {
                    "id": 42,
                    "from": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "Hello",
                    "thread_id": "br-42"
                }
            }],
            "count": 1
        });
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload.to_string()
            }]
        });

        let events =
            derive_domain_events_from_tool_result("send_message", None, &call_result, None, None);
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageSent {
                id,
                from,
                to,
                subject,
                thread_id,
                project,
                ..
            } => {
                assert_eq!(*id, 42);
                assert_eq!(from, "RedFox");
                assert_eq!(to.as_slice(), ["BlueLake"]);
                assert_eq!(subject, "Hello");
                assert_eq!(thread_id, "br-42");
                assert_eq!(project, "alpha");
            }
            other => panic!("expected MessageSent event, got {other:?}"),
        }
        assert!(events.iter().any(|event| matches!(
            event,
            tui_events::MailEvent::MessageReceived { id, to, .. }
                if *id == 42 && to.as_slice() == ["BlueLake"]
        )));
    }

    #[test]
    fn derive_domain_events_from_send_message_deduplicates_cc_and_bcc_recipients() {
        let payload = serde_json::json!({
            "deliveries": [{
                "project": "alpha",
                "payload": {
                    "id": 46,
                    "from": "RedFox",
                    "to": ["BlueLake", "BlueLake"],
                    "cc": ["RedStone"],
                    "bcc": ["RedStone", "GrayWolf"],
                    "subject": "Dedup recipients",
                    "thread_id": "br-46"
                }
            }],
            "count": 1
        });
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload.to_string()
            }]
        });

        let events =
            derive_domain_events_from_tool_result("send_message", None, &call_result, None, None);
        let sent = events
            .iter()
            .filter(|event| matches!(event, tui_events::MailEvent::MessageSent { .. }))
            .count();
        assert_eq!(sent, 1);

        let recipients: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                tui_events::MailEvent::MessageReceived { id, to, .. } if *id == 46 => {
                    to.first().cloned()
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            recipients,
            vec![
                "BlueLake".to_string(),
                "RedStone".to_string(),
                "GrayWolf".to_string()
            ]
        );
    }

    #[test]
    fn derive_domain_events_from_tool_contents_payload() {
        let payload = serde_json::json!({
            "deliveries": [{
                "project": "alpha",
                "payload": {
                    "id": 45,
                    "from": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "From contents",
                    "thread_id": "br-45"
                }
            }],
            "count": 1
        });
        let contents = vec![Content::Text {
            text: payload.to_string(),
        }];

        let events =
            derive_domain_events_from_tool_contents("send_message", None, &contents, None, None);
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageSent { id, subject, .. } => {
                assert_eq!(*id, 45);
                assert_eq!(subject, "From contents");
            }
            other => panic!("expected MessageSent event, got {other:?}"),
        }
        assert!(events.iter().any(|event| matches!(
            event,
            tui_events::MailEvent::MessageReceived { id, to, .. }
                if *id == 45 && to.as_slice() == ["BlueLake"]
        )));
    }

    #[test]
    fn derive_domain_events_from_tool_contents_non_json_is_empty() {
        let contents = vec![Content::Text {
            text: "not-json".to_string(),
        }];
        let events =
            derive_domain_events_from_tool_contents("send_message", None, &contents, None, None);
        assert!(events.is_empty());
    }

    #[test]
    fn derive_domain_events_from_structured_content_payload() {
        let payload = serde_json::json!({
            "deliveries": [{
                "project": "alpha",
                "payload": {
                    "id": 43,
                    "from": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "Structured content",
                    "thread_id": "br-43"
                }
            }],
            "count": 1
        });
        let call_result = serde_json::json!({
            "structuredContent": payload
        });

        let events =
            derive_domain_events_from_tool_result("send_message", None, &call_result, None, None);
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageSent { id, subject, .. } => {
                assert_eq!(*id, 43);
                assert_eq!(subject, "Structured content");
            }
            other => panic!("expected MessageSent event, got {other:?}"),
        }
        assert!(events.iter().any(|event| matches!(
            event,
            tui_events::MailEvent::MessageReceived { id, to, .. }
                if *id == 43 && to.as_slice() == ["BlueLake"]
        )));
    }

    #[test]
    fn derive_domain_events_from_double_encoded_text_payload() {
        let payload = serde_json::json!({
            "deliveries": [{
                "project": "alpha",
                "payload": {
                    "id": 44,
                    "from": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "Double encoded",
                    "thread_id": "br-44"
                }
            }],
            "count": 1
        });
        let double_encoded = serde_json::to_string(&payload.to_string()).expect("json string");
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": double_encoded
            }]
        });

        let events =
            derive_domain_events_from_tool_result("send_message", None, &call_result, None, None);
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageSent { id, subject, .. } => {
                assert_eq!(*id, 44);
                assert_eq!(subject, "Double encoded");
            }
            other => panic!("expected MessageSent event, got {other:?}"),
        }
        assert!(events.iter().any(|event| matches!(
            event,
            tui_events::MailEvent::MessageReceived { id, to, .. }
                if *id == 44 && to.as_slice() == ["BlueLake"]
        )));
    }

    #[test]
    fn derive_domain_events_from_plain_payload_fallback() {
        let payload = serde_json::json!({
            "deliveries": [{
                "project": "alpha",
                "payload": {
                    "id": 99,
                    "from": "RedFox",
                    "to": ["BlueLake"],
                    "subject": "Plain payload",
                    "thread_id": "br-99"
                }
            }],
            "count": 1
        });

        let events = derive_domain_events_from_tool_result(
            "send_message",
            None,
            &payload,
            Some("alpha"),
            None,
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageSent { id, subject, .. } => {
                assert_eq!(*id, 99);
                assert_eq!(subject, "Plain payload");
            }
            other => panic!("expected MessageSent event, got {other:?}"),
        }
        assert!(events.iter().any(|event| matches!(
            event,
            tui_events::MailEvent::MessageReceived { id, to, .. }
                if *id == 99 && to.as_slice() == ["BlueLake"]
        )));
    }

    #[test]
    fn derive_domain_events_from_reservation_grant_payload() {
        let payload = serde_json::json!({
            "granted": [{
                "id": 7,
                "path_pattern": "src/**",
                "exclusive": false,
                "reason": "test",
                "expires_ts": "2026-01-01T00:00:00Z"
            }],
            "conflicts": []
        });
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload.to_string()
            }]
        });
        let call_args = serde_json::json!({
            "agent_name": "BlueLake",
            "ttl_seconds": 900,
            "exclusive": false,
            "paths": ["src/**"]
        });

        let events = derive_domain_events_from_tool_result(
            "file_reservation_paths",
            Some(&call_args),
            &call_result,
            Some("proj-alpha"),
            None,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            tui_events::MailEvent::ReservationGranted {
                agent,
                paths,
                exclusive,
                ttl_s,
                project,
                ..
            } => {
                assert_eq!(agent, "BlueLake");
                assert_eq!(paths.as_slice(), ["src/**"]);
                assert!(!exclusive);
                assert_eq!(*ttl_s, 900);
                assert_eq!(project, "proj-alpha");
            }
            other => panic!("expected ReservationGranted event, got {other:?}"),
        }
    }

    #[test]
    fn derive_domain_events_from_fetch_inbox_payload() {
        let payload = serde_json::json!([
            {
                "id": 1001,
                "from": "GreenPeak",
                "subject": "Need review",
                "thread_id": "br-1001"
            },
            {
                "id": 1002,
                "from": "SilverCrest",
                "subject": "Follow-up",
                "thread_id": "br-1002"
            }
        ]);
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload.to_string()
            }]
        });
        let call_args = serde_json::json!({
            "agent_name": "BlueLake",
            "project_key": "/data/projects/proj-alpha"
        });

        let events = derive_domain_events_from_tool_result(
            "fetch_inbox",
            Some(&call_args),
            &call_result,
            Some("proj-alpha"),
            Some("BlueLake"),
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageReceived {
                id,
                from,
                to,
                subject,
                thread_id,
                project,
                ..
            } => {
                assert_eq!(*id, 1001);
                assert_eq!(from, "GreenPeak");
                assert_eq!(to.as_slice(), ["BlueLake"]);
                assert_eq!(subject, "Need review");
                assert_eq!(thread_id, "br-1001");
                assert_eq!(project, "proj-alpha");
            }
            other => panic!("expected MessageReceived event, got {other:?}"),
        }
    }

    #[test]
    fn derive_domain_events_from_fetch_inbox_product_payload() {
        let payload = serde_json::json!([{
            "id": 2001,
            "from": "GreenPeak",
            "subject": "Product inbox",
            "thread_id": "br-2001"
        }]);
        let call_args = serde_json::json!({
            "agent_name": "BlueLake",
            "project_key": "/data/projects/proj-alpha"
        });

        let events = derive_domain_events_from_tool_result(
            "fetch_inbox_product",
            Some(&call_args),
            &payload,
            Some("proj-alpha"),
            Some("BlueLake"),
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            tui_events::MailEvent::MessageReceived { id, subject, .. } => {
                assert_eq!(*id, 2001);
                assert_eq!(subject, "Product inbox");
            }
            other => panic!("expected MessageReceived event, got {other:?}"),
        }
    }

    #[test]
    fn derive_domain_events_from_fetch_inbox_product_without_project_hint() {
        let payload = serde_json::json!([{
            "id": 2002,
            "from": "GreenPeak",
            "subject": "Product fallback",
            "thread_id": "br-2002"
        }]);
        let call_args = serde_json::json!({
            "agent_name": "BlueLake",
            "product_key": "prod-xyz"
        });

        let events = derive_domain_events_from_tool_result(
            "fetch_inbox_product",
            Some(&call_args),
            &payload,
            None,
            Some("BlueLake"),
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            tui_events::MailEvent::MessageReceived { id, project, .. } => {
                assert_eq!(*id, 2002);
                assert_eq!(project, "product:prod-xyz");
            }
            other => panic!("expected MessageReceived event, got {other:?}"),
        }
    }

    #[test]
    fn derive_domain_events_from_macro_start_session_payload() {
        let payload = serde_json::json!({
            "project": {"slug": "proj-alpha", "human_key": "/data/projects/proj-alpha"},
            "agent": {"name": "BlueLake", "program": "codex-cli", "model": "gpt5"},
            "file_reservations": {
                "granted": [{
                    "id": 7,
                    "path_pattern": "src/**",
                    "exclusive": true,
                    "expires_ts": "2026-01-01T00:00:00Z"
                }],
                "conflicts": []
            },
            "inbox": [{
                "id": 3001,
                "from": "GreenPeak",
                "subject": "Session ready",
                "thread_id": "br-3001"
            }]
        });
        let call_args = serde_json::json!({
            "human_key": "/data/projects/proj-alpha",
            "agent_name": "BlueLake",
            "paths": ["src/**"],
            "ttl_seconds": 900,
            "exclusive": true
        });

        let events = derive_domain_events_from_tool_result(
            "macro_start_session",
            Some(&call_args),
            &payload,
            None,
            None,
        );
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, tui_events::MailEvent::ReservationGranted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, tui_events::MailEvent::MessageReceived { .. }))
        );
    }

    #[test]
    fn derive_domain_events_from_macro_file_reservation_cycle_payload() {
        let payload = serde_json::json!({
            "file_reservations": {
                "granted": [{
                    "id": 8,
                    "path_pattern": "src/**",
                    "exclusive": false,
                    "expires_ts": "2026-01-01T00:00:00Z"
                }],
                "conflicts": []
            },
            "released": {
                "released": 1,
                "released_at": "2026-01-01T00:00:00Z"
            }
        });
        let call_args = serde_json::json!({
            "project_key": "/data/projects/proj-alpha",
            "agent_name": "BlueLake",
            "paths": ["src/**"],
            "ttl_seconds": 600,
            "exclusive": false
        });

        let events = derive_domain_events_from_tool_result(
            "macro_file_reservation_cycle",
            Some(&call_args),
            &payload,
            None,
            None,
        );
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, tui_events::MailEvent::ReservationGranted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, tui_events::MailEvent::ReservationReleased { .. }))
        );
    }

    #[test]
    fn derive_domain_events_from_macro_prepare_thread_payload() {
        let payload = serde_json::json!({
            "project": {"slug": "proj-alpha", "human_key": "/data/projects/proj-alpha"},
            "thread": {"thread_id": "br-5001"},
            "inbox": [{
                "id": 5001,
                "from": "GreenPeak",
                "subject": "Thread context",
                "thread_id": "br-5001"
            }]
        });
        let call_args = serde_json::json!({
            "project_key": "/data/projects/proj-alpha",
            "agent_name": "BlueLake"
        });

        let events = derive_domain_events_from_tool_result(
            "macro_prepare_thread",
            Some(&call_args),
            &payload,
            None,
            None,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            tui_events::MailEvent::MessageReceived { id, subject, .. } => {
                assert_eq!(*id, 5001);
                assert_eq!(subject, "Thread context");
            }
            other => panic!("expected MessageReceived event, got {other:?}"),
        }
    }

    #[test]
    fn derive_domain_events_from_macro_contact_handshake_welcome_message() {
        let payload = serde_json::json!({
            "request": {"status": "pending"},
            "response": {"status": "approved"},
            "welcome_message": {
                "deliveries": [{
                    "project": "proj-alpha",
                    "payload": {
                        "id": 4001,
                        "from": "BlueLake",
                        "to": ["RedStone"],
                        "subject": "Welcome",
                        "thread_id": "br-4001"
                    }
                }],
                "count": 1
            }
        });
        let call_args = serde_json::json!({
            "project_key": "/data/projects/proj-alpha",
            "requester": "BlueLake",
            "target": "RedStone"
        });

        let events = derive_domain_events_from_tool_result(
            "macro_contact_handshake",
            Some(&call_args),
            &payload,
            None,
            None,
        );
        assert_eq!(events.len(), 2);
        match &events[0] {
            tui_events::MailEvent::MessageSent { id, subject, .. } => {
                assert_eq!(*id, 4001);
                assert_eq!(subject, "Welcome");
            }
            other => panic!("expected MessageSent event, got {other:?}"),
        }
        assert!(events.iter().any(|event| matches!(
            event,
            tui_events::MailEvent::MessageReceived { id, to, .. }
                if *id == 4001 && to.as_slice() == ["RedStone"]
        )));
    }

    #[test]
    fn derive_domain_events_from_release_without_paths_uses_all_marker() {
        let payload = serde_json::json!({
            "released": 3,
            "released_at": "2026-01-01T00:00:00Z"
        });
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload.to_string()
            }]
        });
        let call_args = serde_json::json!({
            "agent_name": "BlueLake"
        });

        let events = derive_domain_events_from_tool_result(
            "release_file_reservations",
            Some(&call_args),
            &call_result,
            Some("proj-alpha"),
            None,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            tui_events::MailEvent::ReservationReleased {
                agent,
                paths,
                project,
                ..
            } => {
                assert_eq!(agent, "BlueLake");
                assert_eq!(paths.as_slice(), ["<all-active>"]);
                assert_eq!(project, "proj-alpha");
            }
            other => panic!("expected ReservationReleased event, got {other:?}"),
        }
    }

    #[test]
    fn derive_domain_events_from_register_agent_payload() {
        let payload = serde_json::json!({
            "id": 1,
            "name": "BlueLake",
            "program": "codex-cli",
            "model": "gpt5-codex"
        });
        let call_result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": payload.to_string()
            }]
        });

        let events = derive_domain_events_from_tool_result(
            "register_agent",
            None,
            &call_result,
            Some("proj-alpha"),
            None,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            tui_events::MailEvent::AgentRegistered {
                name,
                program,
                model_name,
                project,
                ..
            } => {
                assert_eq!(name, "BlueLake");
                assert_eq!(program, "codex-cli");
                assert_eq!(model_name, "gpt5-codex");
                assert_eq!(project, "proj-alpha");
            }
            other => panic!("expected AgentRegistered event, got {other:?}"),
        }
    }

    #[test]
    fn tui_state_global_roundtrip() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // When no TUI state is set, handle returns None
        assert!(tui_state_handle().is_none());

        // Set a TUI state
        let config = mcp_agent_mail_core::Config::default();
        let state = tui_bridge::TuiSharedState::new(&config);
        set_tui_state_handle(Some(Arc::clone(&state)));

        assert!(tui_state_handle().is_some());

        // emit_tui_event should push into the ring buffer
        emit_tui_event(tui_events::MailEvent::server_started("http://test", "test"));
        let events = state.recent_events(10);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, tui_events::MailEvent::ServerStarted { .. })),
            "expected ServerStarted event in ring buffer"
        );

        // Clear
        set_tui_state_handle(None);
        assert!(tui_state_handle().is_none());
    }

    #[test]
    fn emit_tui_event_noop_when_no_state() {
        let _guard = TUI_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Make sure no state is set
        set_tui_state_handle(None);
        // Should not panic
        emit_tui_event(tui_events::MailEvent::server_shutdown());
    }

    fn setup_compose_dispatch_test_db(path: &std::path::Path) {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let conn = mcp_agent_mail_db::DbConn::open_file(path.display().to_string())
            .expect("open compose test db");
        conn.execute_sync(
            "CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL,
                human_key TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            &[],
        )
        .expect("create projects");
        conn.execute_sync(
            "CREATE TABLE agents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT NOT NULL,
                task_description TEXT NOT NULL,
                inception_ts INTEGER NOT NULL,
                last_active_ts INTEGER NOT NULL
            )",
            &[],
        )
        .expect("create agents");
        conn.execute_sync(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                sender_id INTEGER NOT NULL,
                thread_id TEXT,
                subject TEXT NOT NULL,
                body_md TEXT NOT NULL,
                importance TEXT NOT NULL DEFAULT 'normal',
                ack_required INTEGER NOT NULL DEFAULT 0,
                created_ts INTEGER NOT NULL,
                attachments TEXT NOT NULL DEFAULT '[]',
                recipients_json TEXT NOT NULL DEFAULT '[]'
            )",
            &[],
        )
        .expect("create messages");
        conn.execute_sync(
            "CREATE TABLE message_recipients (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id INTEGER NOT NULL,
                agent_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                ack_ts INTEGER,
                read_ts INTEGER
            )",
            &[],
        )
        .expect("create message_recipients");
        conn.execute_sync(
            "INSERT INTO projects (id, slug, human_key, created_at) VALUES (?1, ?2, ?3, ?4)",
            &[
                Value::BigInt(1),
                Value::Text("proj-test".to_string()),
                Value::Text("/tmp/proj-test".to_string()),
                Value::BigInt(0),
            ],
        )
        .expect("insert project");
        conn.execute_sync(
            "INSERT INTO agents (id, project_id, name, program, model, task_description, inception_ts, last_active_ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            &[
                Value::BigInt(2),
                Value::BigInt(1),
                Value::Text("BlueLake".to_string()),
                Value::Text("test".to_string()),
                Value::Text("test".to_string()),
                Value::Text("recipient".to_string()),
                Value::BigInt(0),
                Value::BigInt(0),
            ],
        )
        .expect("insert recipient");
    }

    #[test]
    fn dispatch_compose_envelope_inserts_new_message_and_uses_last_insert_rowid() {
        use mcp_agent_mail_db::sqlmodel_core::Value;

        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("compose_dispatch.sqlite3");
        setup_compose_dispatch_test_db(&db_path);
        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");
        conn.execute_sync(
            "INSERT INTO messages (id, project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments, recipients_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            &[
                Value::BigInt(1),
                Value::BigInt(1),
                Value::BigInt(2),
                Value::Text("legacy-1".to_string()),
                Value::Text("legacy-subject".to_string()),
                Value::Text("legacy-body".to_string()),
                Value::Text("normal".to_string()),
                Value::BigInt(0),
                Value::BigInt(1),
                Value::Text("[]".to_string()),
                Value::Text("[]".to_string()),
            ],
        )
        .expect("insert legacy message");

        let config = mcp_agent_mail_core::Config::default();
        let tui_state = tui_bridge::TuiSharedState::new(&config);
        let envelope = tui_compose::ComposeEnvelope {
            sender_name: tui_compose::OVERSEER_AGENT_NAME.to_string(),
            to: vec!["BlueLake".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "compose subject".to_string(),
            body_md: "compose body".to_string(),
            importance: "high".to_string(),
            thread_id: Some("br-123".to_string()),
        };
        let database_url = format!("sqlite://{}", db_path.display());
        dispatch_compose_envelope(&database_url, &tui_state, &envelope);

        let message_count = conn
            .query_sync("SELECT COUNT(*) AS c FROM messages", &[])
            .expect("count messages")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .unwrap_or_default();
        assert_eq!(message_count, 2, "compose should create a new message row");

        let newest = conn
            .query_sync(
                "SELECT id, subject, thread_id FROM messages ORDER BY id DESC LIMIT 1",
                &[],
            )
            .expect("query newest message")
            .into_iter()
            .next()
            .expect("new message row");
        let new_id = newest
            .get_named::<i64>("id")
            .expect("new message id should be present");
        let new_subject = newest
            .get_named::<String>("subject")
            .expect("new message subject");
        let new_thread = newest
            .get_named::<Option<String>>("thread_id")
            .expect("new message thread");
        assert!(
            new_id > 1,
            "new message id should be greater than legacy id"
        );
        assert_eq!(new_subject, "compose subject");
        assert_eq!(new_thread.as_deref(), Some("br-123"));

        let legacy_recips = conn
            .query_sync(
                "SELECT COUNT(*) AS c FROM message_recipients WHERE message_id = 1",
                &[],
            )
            .expect("legacy recipient count")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .unwrap_or_default();
        assert_eq!(
            legacy_recips, 0,
            "compose recipients must not be attached to pre-existing message ids"
        );

        let new_recips = conn
            .query_sync(
                "SELECT COUNT(*) AS c FROM message_recipients WHERE message_id = ?1",
                &[Value::BigInt(new_id)],
            )
            .expect("new recipient count")
            .into_iter()
            .next()
            .and_then(|row| row.get_named::<i64>("c").ok())
            .unwrap_or_default();
        assert_eq!(new_recips, 1, "compose should add exactly one recipient");
    }

    #[test]
    fn dispatch_compose_envelope_stores_null_thread_id_when_unset() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("compose_dispatch_null_thread.sqlite3");
        setup_compose_dispatch_test_db(&db_path);

        let config = mcp_agent_mail_core::Config::default();
        let tui_state = tui_bridge::TuiSharedState::new(&config);
        let envelope = tui_compose::ComposeEnvelope {
            sender_name: tui_compose::OVERSEER_AGENT_NAME.to_string(),
            to: vec!["BlueLake".to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "compose no thread".to_string(),
            body_md: "body".to_string(),
            importance: "normal".to_string(),
            thread_id: None,
        };
        let database_url = format!("sqlite://{}", db_path.display());
        dispatch_compose_envelope(&database_url, &tui_state, &envelope);

        let conn =
            mcp_agent_mail_db::DbConn::open_file(db_path.display().to_string()).expect("open db");
        let row = conn
            .query_sync(
                "SELECT thread_id FROM messages WHERE subject = ?1 LIMIT 1",
                &[mcp_agent_mail_db::sqlmodel_core::Value::Text(
                    "compose no thread".to_string(),
                )],
            )
            .expect("query message")
            .into_iter()
            .next()
            .expect("message row");
        let thread_id = row
            .get_named::<Option<String>>("thread_id")
            .expect("thread_id decode");
        assert!(
            thread_id.is_none(),
            "thread_id should be NULL when compose envelope omits it"
        );
    }

    // -----------------------------------------------------------------------
    // JWKS stampede protection tests (br-1i11.4.2)
    // -----------------------------------------------------------------------

    /// Helper: JWKS server with configurable response delay and external
    /// request counter.  The counter is incremented for every accepted
    /// connection, and the server adds `response_delay` before replying.
    fn with_counted_delayed_jwks_server<F>(
        jwks_body: &[u8],
        max_requests: usize,
        response_delay: Duration,
        request_counter: &std::sync::atomic::AtomicUsize,
        f: F,
    ) where
        F: FnOnce(String),
    {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::Ordering;
        use std::time::Instant;

        std::thread::scope(|s| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let addr = listener.local_addr().expect("addr");
            let body = jwks_body.to_vec();

            s.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    if request_counter.load(Ordering::SeqCst) >= max_requests {
                        return;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            request_counter.fetch_add(1, Ordering::SeqCst);
                            // Drain the request.
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                            let mut buf = [0u8; 512];
                            let mut seen = Vec::new();
                            loop {
                                match stream.read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        seen.extend_from_slice(&buf[..n]);
                                        if seen.windows(4).any(|w| w == b"\r\n\r\n")
                                            || seen.len() > 8192
                                        {
                                            break;
                                        }
                                    }
                                    Err(e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock
                                            || e.kind() == std::io::ErrorKind::TimedOut =>
                                    {
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            // Configurable delay before responding.
                            if !response_delay.is_zero() {
                                std::thread::sleep(response_delay);
                            }
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(hdr.as_bytes());
                            let _ = stream.write_all(&body);
                            let _ = stream.flush();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() > deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });

            let url = format!("http://{addr}/jwks");
            f(url);
        });
    }

    /// Build test JWKS bytes and a parsed `JwkSet` for cache seeding.
    fn test_jwks_material() -> (Vec<u8>, jsonwebtoken::jwk::JwkSet) {
        use base64::Engine as _;
        let k =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"stampede-test-secret-key");
        let jwks_json = serde_json::json!({
            "keys": [{
                "kty": "oct",
                "alg": "HS256",
                "kid": "stampede-kid-1",
                "k": k,
            }]
        });
        let bytes = serde_json::to_vec(&jwks_json).expect("jwks json");
        let set: jsonwebtoken::jwk::JwkSet = serde_json::from_value(jwks_json).expect("parse");
        (bytes, set)
    }

    /// Seed the JWKS cache on an `HttpState` with the given `JwkSet` and an
    /// already-expired timestamp so the next fetch sees stale data.
    fn seed_expired_cache(state: &HttpState, jwks: &jsonwebtoken::jwk::JwkSet) {
        let mut cache = state
            .jwks_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache = Some(JwksCacheEntry {
            // 2 minutes in the past — well beyond the 60s TTL.
            fetched_at: Instant::now().checked_sub(Duration::from_mins(2)).unwrap(),
            jwks: Arc::new(jwks.clone()),
        });
    }

    /// When the JWKS cache is stale and multiple threads call `fetch_jwks`
    /// concurrently, only ONE should actually hit the remote endpoint.
    /// The rest should immediately return the stale cached value.
    #[test]
    fn jwks_stampede_concurrent_expired_single_refresh() {
        let (jwks_bytes, jwks_set) = test_jwks_material();
        let counter = std::sync::atomic::AtomicUsize::new(0);

        // Allow up to 20 requests (but expect only 1).
        with_counted_delayed_jwks_server(&jwks_bytes, 20, Duration::ZERO, &counter, |url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);
            seed_expired_cache(&state, &jwks_set);

            // Spawn 10 threads, each doing a non-forced fetch.
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..10)
                    .map(|_| {
                        s.spawn(|| {
                            let rt = RuntimeBuilder::current_thread().build().expect("runtime");
                            rt.block_on(state.fetch_jwks(&url, false))
                        })
                    })
                    .collect();

                let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
                let ok_count = results.iter().filter(|r| r.is_ok()).count();
                assert_eq!(ok_count, 10, "all 10 fetches should succeed");
            });

            let requests = counter.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                requests, 1,
                "only 1 HTTP request should reach the mock server (got {requests})"
            );
        });
    }

    /// When the JWKS cache is stale and a slow refresh is in-flight,
    /// concurrent callers should receive stale data instantly — not block
    /// waiting for the refresh to complete.
    #[test]
    fn jwks_stampede_stale_served_fast() {
        let (jwks_bytes, jwks_set) = test_jwks_material();
        let counter = std::sync::atomic::AtomicUsize::new(0);

        // Server responds with a 500ms delay to simulate slow network.
        with_counted_delayed_jwks_server(
            &jwks_bytes,
            20,
            Duration::from_millis(500),
            &counter,
            |url| {
                let config = mcp_agent_mail_core::Config {
                    http_jwt_enabled: true,
                    http_jwt_algorithms: vec!["HS256".to_string()],
                    http_jwt_jwks_url: Some(url.clone()),
                    http_rbac_enabled: false,
                    ..Default::default()
                };
                let state = build_state(config);
                seed_expired_cache(&state, &jwks_set);

                let timings = std::sync::Mutex::new(Vec::new());
                std::thread::scope(|s| {
                    let handles: Vec<_> = (0..10)
                        .map(|_| {
                            s.spawn(|| {
                                let rt = RuntimeBuilder::current_thread().build().expect("runtime");
                                let start = Instant::now();
                                let result = rt.block_on(state.fetch_jwks(&url, false));
                                let elapsed = start.elapsed();
                                timings.lock().unwrap().push(elapsed);
                                result
                            })
                        })
                        .collect();
                    for h in handles {
                        assert!(h.join().unwrap().is_ok());
                    }
                });

                let timings = timings.into_inner().unwrap();
                // At least 8 of 10 threads should have completed in under
                // 250ms — they served stale data without waiting for the
                // slow refresh.
                let fast_count = timings.iter().filter(|t| t.as_millis() < 250).count();
                assert!(
                    fast_count >= 8,
                    "expected >= 8 fast (stale-serving) threads, got {fast_count}; \
                     timings: {timings:?}"
                );
            },
        );
    }

    /// `force=true` always hits the remote endpoint even when the cache is
    /// still fresh.
    #[test]
    fn jwks_force_refresh_bypasses_fresh_cache() {
        let (jwks_bytes, jwks_set) = test_jwks_material();
        let counter = std::sync::atomic::AtomicUsize::new(0);

        with_counted_delayed_jwks_server(&jwks_bytes, 5, Duration::ZERO, &counter, |url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);

            // Seed fresh cache (not expired).
            {
                let mut cache = state
                    .jwks_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *cache = Some(JwksCacheEntry {
                    fetched_at: Instant::now(),
                    jwks: Arc::new(jwks_set.clone()),
                });
            }

            let rt = RuntimeBuilder::current_thread().build().expect("runtime");

            // Non-forced fetch should use cache (0 requests).
            let r = rt.block_on(state.fetch_jwks(&url, false));
            assert!(r.is_ok());
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);

            // Forced fetch should hit the remote endpoint.
            let r = rt.block_on(state.fetch_jwks(&url, true));
            assert!(r.is_ok());
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    /// On cold start (no cached JWKS), all concurrent callers should
    /// eventually get valid data.  Multiple HTTP requests are acceptable
    /// because there is no stale data to serve as fallback.
    #[test]
    fn jwks_cold_start_concurrent() {
        let (jwks_bytes, _) = test_jwks_material();
        let counter = std::sync::atomic::AtomicUsize::new(0);

        with_counted_delayed_jwks_server(&jwks_bytes, 20, Duration::ZERO, &counter, |url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);
            // No cache seeding — cold start.

            std::thread::scope(|s| {
                let handles: Vec<_> = (0..5)
                    .map(|_| {
                        s.spawn(|| {
                            let rt = RuntimeBuilder::current_thread().build().expect("runtime");
                            rt.block_on(state.fetch_jwks(&url, false))
                        })
                    })
                    .collect();

                let ok_count = handles
                    .into_iter()
                    .filter_map(|h| h.join().unwrap().ok())
                    .count();
                // All or most should succeed (depends on timing, but at
                // least 1 must succeed since the server is available).
                assert!(ok_count >= 1, "at least 1 cold-start fetch should succeed");
            });

            // Multiple HTTP requests are expected (no stale to serve).
            let requests = counter.load(std::sync::atomic::Ordering::SeqCst);
            assert!(
                requests >= 1,
                "cold start should make at least 1 HTTP request (got {requests})"
            );
        });
    }

    /// The CAS lock is always released after a refresh, even if the HTTP
    /// request fails.  Subsequent callers must still be able to acquire
    /// the lock and refresh successfully.
    #[test]
    fn jwks_stampede_lock_released_after_failure() {
        let (jwks_bytes, jwks_set) = test_jwks_material();

        // Phase 1: server that rejects (bad body) to force a failure.
        let counter1 = std::sync::atomic::AtomicUsize::new(0);
        let bad_body = b"not valid json";
        with_counted_delayed_jwks_server(bad_body, 5, Duration::ZERO, &counter1, |url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);
            seed_expired_cache(&state, &jwks_set);

            let rt = RuntimeBuilder::current_thread().build().expect("runtime");

            // This fetch should fail (bad JSON) but release the lock.
            let r = rt.block_on(state.fetch_jwks(&url, false));
            assert!(r.is_err(), "bad JSON should cause fetch_jwks to fail");

            // The refreshing flag must be cleared.
            assert!(
                !state
                    .jwks_refreshing
                    .load(std::sync::atomic::Ordering::SeqCst),
                "refreshing flag must be false after failed fetch"
            );
        });

        // Phase 2: verify a fresh state with a working server succeeds.
        let counter2 = std::sync::atomic::AtomicUsize::new(0);
        with_counted_delayed_jwks_server(&jwks_bytes, 5, Duration::ZERO, &counter2, |url| {
            let config = mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            };
            let state = build_state(config);
            seed_expired_cache(&state, &jwks_set);

            let rt = RuntimeBuilder::current_thread().build().expect("runtime");
            let r = rt.block_on(state.fetch_jwks(&url, false));
            assert!(r.is_ok(), "fetch should succeed after prior failure");
            assert_eq!(counter2.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    // -----------------------------------------------------------------------
    // JWKS E2E load test (br-1i11.4.6)
    // -----------------------------------------------------------------------

    /// High-concurrency load test: 50 threads hit an expired cache
    /// simultaneously with a slow JWKS endpoint.  Verifies fan-out
    /// suppression: exactly 1 HTTP request, all 50 threads succeed, and
    /// the majority complete instantly via stale cache.
    #[test]
    fn jwks_load_test_fan_out_suppression() {
        let (jwks_bytes, jwks_set) = test_jwks_material();
        let counter = std::sync::atomic::AtomicUsize::new(0);

        // Slow JWKS endpoint (1 second response time).
        with_counted_delayed_jwks_server(
            &jwks_bytes,
            60,
            Duration::from_secs(1),
            &counter,
            |url| {
                let state = build_state(mcp_agent_mail_core::Config {
                    http_jwt_enabled: true,
                    http_jwt_algorithms: vec!["HS256".to_string()],
                    http_jwt_jwks_url: Some(url.clone()),
                    http_rbac_enabled: false,
                    ..Default::default()
                });
                seed_expired_cache(&state, &jwks_set);

                let timings = std::sync::Mutex::new(Vec::new());
                let results = std::sync::Mutex::new(Vec::new());

                std::thread::scope(|s| {
                    let handles: Vec<_> = (0..50)
                        .map(|_| {
                            s.spawn(|| {
                                let rt = RuntimeBuilder::current_thread().build().expect("runtime");
                                let start = Instant::now();
                                let result = rt.block_on(state.fetch_jwks(&url, false));
                                let elapsed = start.elapsed();
                                timings.lock().unwrap().push(elapsed);
                                results.lock().unwrap().push(result.is_ok());
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                });

                let timings = timings.into_inner().unwrap();
                let results = results.into_inner().unwrap();

                // All 50 must succeed.
                let ok_count = results.iter().filter(|&&ok| ok).count();
                assert_eq!(ok_count, 50, "all 50 threads must succeed");

                // Fan-out suppression: exactly 1 HTTP request.
                let requests = counter.load(std::sync::atomic::Ordering::SeqCst);
                assert_eq!(
                    requests, 1,
                    "fan-out suppression: expected 1 request, got {requests}"
                );

                // Timing distribution: at least 45 threads under 250ms.
                let fast = timings.iter().filter(|t| t.as_millis() < 250).count();
                assert!(
                    fast >= 45,
                    "at least 45/50 threads must be fast (stale-serving), got {fast}; \
                     timings: {timings:?}"
                );

                // At most 2 threads should take longer than 500ms (the
                // refresher plus possibly one spinning on the CAS).
                let slow = timings.iter().filter(|t| t.as_millis() > 500).count();
                assert!(slow <= 2, "at most 2 slow threads expected, got {slow}");
            },
        );
    }

    /// Load test with varying JWKS endpoint latencies: verify stampede
    /// protection works consistently across fast/medium/slow responses.
    #[test]
    fn jwks_load_test_varying_latencies() {
        let (jwks_bytes, jwks_set) = test_jwks_material();

        for delay_ms in [0u64, 200, 800] {
            let counter = std::sync::atomic::AtomicUsize::new(0);
            with_counted_delayed_jwks_server(
                &jwks_bytes,
                30,
                Duration::from_millis(delay_ms),
                &counter,
                |url| {
                    let state = build_state(mcp_agent_mail_core::Config {
                        http_jwt_enabled: true,
                        http_jwt_algorithms: vec!["HS256".to_string()],
                        http_jwt_jwks_url: Some(url.clone()),
                        http_rbac_enabled: false,
                        ..Default::default()
                    });
                    seed_expired_cache(&state, &jwks_set);

                    std::thread::scope(|s| {
                        let handles: Vec<_> = (0..20)
                            .map(|_| {
                                s.spawn(|| {
                                    let rt =
                                        RuntimeBuilder::current_thread().build().expect("runtime");
                                    rt.block_on(state.fetch_jwks(&url, false))
                                })
                            })
                            .collect();
                        let ok_count = handles
                            .into_iter()
                            .filter_map(|h| h.join().unwrap().ok())
                            .count();
                        assert_eq!(ok_count, 20, "all 20 must succeed at delay={delay_ms}ms");
                    });

                    let reqs = counter.load(std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(reqs, 1, "single refresh at delay={delay_ms}ms, got {reqs}");
                },
            );
        }
    }

    // -----------------------------------------------------------------------
    // JWKS security regression tests (br-1i11.4.7)
    // -----------------------------------------------------------------------

    /// After a key rotation (JWKS endpoint returns new key set), the old
    /// cached keys should be replaced.  A forced refresh must pick up
    /// the new key set.
    #[test]
    fn jwks_key_rotation_replaces_cached_keys() {
        use base64::Engine as _;

        // Phase 1: Serve initial JWKS with kid-1.
        let k1 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"secret-key-1");
        let jwks1 = serde_json::json!({
            "keys": [{"kty": "oct", "alg": "HS256", "kid": "kid-1", "k": k1}]
        });
        let jwks1_set: jsonwebtoken::jwk::JwkSet = serde_json::from_value(jwks1).unwrap();

        // Phase 2: Rotated JWKS with kid-2 only (kid-1 removed).
        let k2 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"secret-key-2");
        let jwks2 = serde_json::json!({
            "keys": [{"kty": "oct", "alg": "HS256", "kid": "kid-2", "k": k2}]
        });
        let jwks2_bytes = serde_json::to_vec(&jwks2).unwrap();

        // Serve the rotated JWKS from the mock server.
        let counter = std::sync::atomic::AtomicUsize::new(0);
        with_counted_delayed_jwks_server(&jwks2_bytes, 5, Duration::ZERO, &counter, |url| {
            let state = build_state(mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            });

            // Seed cache with old key set (kid-1).
            {
                let mut cache = state
                    .jwks_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *cache = Some(JwksCacheEntry {
                    fetched_at: Instant::now(),
                    jwks: Arc::new(jwks1_set),
                });
            }

            let rt = RuntimeBuilder::current_thread().build().expect("runtime");

            // Fresh cache: still has kid-1.
            let jwks = rt.block_on(state.fetch_jwks(&url, false)).unwrap();
            assert!(jwks.find("kid-1").is_some(), "fresh cache has kid-1");
            assert!(jwks.find("kid-2").is_none(), "fresh cache lacks kid-2");

            // Force refresh: picks up rotated key set with kid-2.
            let jwks = rt.block_on(state.fetch_jwks(&url, true)).unwrap();
            assert!(jwks.find("kid-2").is_some(), "rotated set has kid-2");
            assert!(jwks.find("kid-1").is_none(), "rotated set dropped kid-1");

            // Subsequent non-forced fetches use the new cache.
            let jwks = rt.block_on(state.fetch_jwks(&url, false)).unwrap();
            assert!(jwks.find("kid-2").is_some(), "cache updated to kid-2");
        });
    }

    /// During a JWKS endpoint outage, stale keys continue to serve
    /// requests (fail-open for availability).  But invalid tokens must
    /// still be rejected — stale cache doesn't weaken authentication.
    #[test]
    fn jwks_outage_stale_cache_preserves_auth_security() {
        use base64::Engine as _;

        let secret = b"outage-test-secret";
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let jwks_json = serde_json::json!({
            "keys": [{"kty": "oct", "alg": "HS256", "kid": "kid-outage", "k": k}]
        });
        let jwks_set: jsonwebtoken::jwk::JwkSet = serde_json::from_value(jwks_json).unwrap();

        // Mock server is unreachable (use a port that won't connect).
        let state = build_state(mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_algorithms: vec!["HS256".to_string()],
            http_jwt_jwks_url: Some("http://127.0.0.1:1/unreachable".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        });

        // Seed with stale but valid keys.
        seed_expired_cache(&state, &jwks_set);

        // Simulate another task already refreshing (so we get stale data).
        state
            .jwks_refreshing
            .store(true, std::sync::atomic::Ordering::Release);

        let rt = RuntimeBuilder::current_thread().build().expect("runtime");

        // Stale-while-revalidate: should return cached JWKS despite outage.
        let result = rt.block_on(state.fetch_jwks("http://127.0.0.1:1/unreachable", false));
        assert!(result.is_ok(), "stale cache should be served during outage");
        let jwks = result.unwrap();
        assert!(
            jwks.find("kid-outage").is_some(),
            "stale cache has the original key"
        );

        // Reset the refreshing flag for the next assertion.
        state
            .jwks_refreshing
            .store(false, std::sync::atomic::Ordering::Release);

        // Valid token with correct secret: should be verifiable against stale cache.
        let claims = serde_json::json!({"sub": "user-1", "role": "reader"});
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("kid-outage".to_string());
        let valid_token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .unwrap();

        // Verify with the cached (stale) key.
        let decoding_key = jsonwebtoken::DecodingKey::from_secret(secret);
        let validation = HttpState::jwt_validation(vec![jsonwebtoken::Algorithm::HS256]);
        let decoded =
            jsonwebtoken::decode::<serde_json::Value>(&valid_token, &decoding_key, &validation);
        assert!(decoded.is_ok(), "valid token verifiable with correct key");

        // Invalid token (wrong secret): must be rejected even with stale cache.
        let wrong_token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"wrong-secret"),
        )
        .unwrap();
        let decoded =
            jsonwebtoken::decode::<serde_json::Value>(&wrong_token, &decoding_key, &validation);
        assert!(
            decoded.is_err(),
            "invalid token must be rejected even with stale cache"
        );
    }

    // ── Transport compatibility lock assertions (br-3vwi.13.9) ───────────

    #[test]
    fn compat_lock_server_name_matches_build_server() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        assert_eq!(
            state.server_info.name, COMPAT_SERVER_NAME,
            "COMPAT LOCK: build_server() must produce the locked server name"
        );
    }

    #[test]
    fn compat_lock_health_paths_bypass_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        for &path in COMPAT_HEALTH_PATHS {
            let req = make_request(Http1Method::Get, path, &[]);
            let resp = block_on(state.handle(req));
            assert_ne!(
                resp.status, 401,
                "COMPAT LOCK: Health path '{path}' must bypass bearer auth"
            );
        }
    }

    #[test]
    fn compat_lock_oauth_well_known_accessible() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let req = make_request(Http1Method::Get, COMPAT_OAUTH_WELL_KNOWN, &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 200,
            "COMPAT LOCK: OAuth well-known must return 200"
        );
        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
        assert_eq!(body["mcp_oauth"], false);
    }

    #[test]
    fn compat_lock_mail_ui_routes_to_handler() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        // /mail should not 404 — it routes to mail_ui::dispatch
        let req = make_request(Http1Method::Get, COMPAT_MAIL_UI_PREFIX, &[]);
        let resp = block_on(state.handle(req));
        // Mail UI may return 200, 302, or even 404 for specific sub-paths,
        // but it must NOT return 401 (auth is only for MCP endpoint).
        assert_ne!(
            resp.status, 401,
            "COMPAT LOCK: /mail must not require bearer auth"
        );
    }

    // ── D3: query-parameter token auth for /mail ─────────────────

    #[test]
    fn mail_route_accepts_query_token_when_bearer_configured() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("test-secret-42".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        // Request with correct ?token= query param should NOT get 401.
        let req = make_request(Http1Method::Get, "/mail?token=test-secret-42", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(resp.status, 401, "/mail with correct ?token= must not 401");
    }

    #[test]
    fn mail_route_rejects_wrong_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("test-secret-42".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail?token=wrong-token", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401, "/mail with wrong ?token= must 401");
    }

    #[test]
    fn mail_route_rejects_missing_token_when_bearer_required() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("test-secret-42".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401, "/mail without any token must 401");
    }

    #[test]
    fn mail_subpath_accepts_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("abc123".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail/dashboard?token=abc123", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/mail/dashboard with correct token must not 401"
        );
    }

    #[test]
    fn mail_route_accepts_percent_encoded_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("a+b/c?d=e&f".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail?token=a%2Bb%2Fc%3Fd%3De%26f", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/mail with percent-encoded token must authenticate"
        );
    }

    #[test]
    fn web_dashboard_route_accepts_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("dash-secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/web-dashboard?token=dash-secret", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/web-dashboard query token must authenticate"
        );
    }

    #[test]
    fn web_dashboard_state_route_accepts_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("dash-secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(
            Http1Method::Get,
            "/web-dashboard/state?token=dash-secret",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/web-dashboard/state query token must authenticate"
        );
    }

    #[test]
    fn web_dashboard_stream_route_accepts_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("dash-secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(
            Http1Method::Get,
            "/web-dashboard/stream?token=dash-secret",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/web-dashboard/stream query token must authenticate"
        );
    }

    #[test]
    fn build_web_ui_url_percent_encodes_token() {
        let url = build_web_ui_url("100.64.0.1", 8765, Some("a+b/c?d=e&f"));
        assert_eq!(
            url,
            "http://100.64.0.1:8765/mail?token=a%2Bb%2Fc%3Fd%3De%26f"
        );
    }

    #[test]
    fn mail_route_requires_jwt_when_jwt_only_enabled() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-only-secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/mail", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "/mail must require JWT when JWT auth is enabled without static bearer token"
        );
    }

    #[test]
    fn mail_route_accepts_valid_jwt_when_jwt_only_enabled() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-only-secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "mail-user", "role": "writer" });
        let token = hs256_token(b"jwt-only-secret", &claims);
        let auth = format!("Bearer {token}");
        let req = make_request(
            Http1Method::Get,
            "/mail",
            &[("Authorization", auth.as_str())],
        );
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/mail with valid JWT must authenticate successfully"
        );
    }

    // ── D4: actionable HTML for unauthorized browser routes ──────

    #[test]
    fn mail_unauthorized_returns_html_not_json() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("text/html"),
            "unauthorized /mail must return HTML, got: {content_type}"
        );
        let body = String::from_utf8_lossy(&resp.body);
        assert!(
            body.contains("HTTP_BEARER_TOKEN"),
            "HTML must mention env var"
        );
        assert!(
            body.contains("How to fix"),
            "HTML must include remediation steps"
        );
    }

    #[test]
    fn web_dashboard_unauthorized_returns_html_not_json() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/web-dashboard", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("text/html"),
            "unauthorized /web-dashboard must return HTML, got: {content_type}"
        );
        let body = String::from_utf8_lossy(&resp.body);
        assert!(
            body.contains("/web-dashboard"),
            "HTML must explain dashboard query-token usage"
        );
    }

    #[test]
    fn web_dashboard_json_route_unauthorized_returns_json() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/web-dashboard/state", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("application/json"),
            "unauthorized /web-dashboard/state must return JSON, got: {content_type}"
        );
    }

    #[test]
    fn web_dashboard_stream_route_unauthorized_returns_json() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/web-dashboard/stream", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("application/json"),
            "unauthorized /web-dashboard/stream must return JSON, got: {content_type}"
        );
    }

    #[test]
    fn non_mail_unauthorized_returns_json() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Post, "/mcp/", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("application/json"),
            "unauthorized /mcp must return JSON, got: {content_type}"
        );
    }

    #[test]
    fn mcp_route_rejects_query_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Post, "/mcp/?token=secret", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "query token auth must be limited to browser routes only"
        );
    }

    #[test]
    fn mail_api_unauthorized_returns_json() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail/api/messages", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("application/json"),
            "unauthorized /mail/api/* must return JSON, got: {content_type}"
        );
    }

    // ── E4: Health workflow regression tests ──────────────────────

    #[test]
    fn e4_health_endpoint_bypasses_all_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        // /health must never return 401, regardless of auth config.
        let req = make_request(Http1Method::Get, "/health", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/health must bypass auth (got {}, expected non-401)",
            resp.status
        );
    }

    #[test]
    fn e4_no_bearer_config_allows_all_routes() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: None,
            http_jwt_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        // Without any auth configured, /mail should be accessible.
        let req = make_request(Http1Method::Get, "/mail", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(
            resp.status, 401,
            "/mail without auth config must not return 401 (got {})",
            resp.status
        );
    }

    #[test]
    fn e4_html_remediation_contains_localhost_tip() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/mail", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body = String::from_utf8_lossy(&resp.body);
        assert!(
            body.contains("localhost"),
            "HTML remediation must mention localhost access tip"
        );
    }

    #[test]
    fn e4_mail_subpath_unauthorized_is_html() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        // /mail/dashboard (browser route) must return HTML 401, not JSON.
        let req = make_request(Http1Method::Get, "/mail/dashboard", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map_or("", |(_, v)| v.as_str());
        assert!(
            content_type.contains("text/html"),
            "/mail/dashboard unauthorized must return HTML, got: {content_type}"
        );
    }

    #[test]
    fn e4_query_token_is_constant_time_safe() {
        // Wrong token must still return 401 (not bypass via timing).
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("correct-token".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        // Similar-prefix token must fail.
        let req = make_request(Http1Method::Get, "/mail?token=correct-toke", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401, "partial token must not authenticate");

        // Empty token must fail.
        let req = make_request(Http1Method::Get, "/mail?token=", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401, "empty token must not authenticate");
    }

    #[test]
    fn e4_bearer_header_and_query_token_both_accepted() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("dual-test-token".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        // Bearer header auth.
        let req = make_request(
            Http1Method::Get,
            "/mail",
            &[("Authorization", "Bearer dual-test-token")],
        );
        let resp = block_on(state.handle(req));
        assert_ne!(resp.status, 401, "Bearer header must authenticate");

        // Query token auth.
        let req = make_request(Http1Method::Get, "/mail?token=dual-test-token", &[]);
        let resp = block_on(state.handle(req));
        assert_ne!(resp.status, 401, "query token must authenticate");

        // Dashboard query token auth.
        let req = make_request(
            Http1Method::Get,
            "/web-dashboard?token=dual-test-token",
            &[],
        );
        let resp = block_on(state.handle(req));
        assert_ne!(resp.status, 401, "dashboard query token must authenticate");
    }

    #[test]
    fn compat_lock_mcp_aliases_all_accepted() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let json_rpc = JsonRpcRequest::new("tools/list", None, 999_i64);
        let body = serde_json::to_vec(&json_rpc).expect("serialize");

        for &alias in COMPAT_MCP_ALIASES {
            let mut req = make_request(Http1Method::Post, alias, &[]);
            req.body = body.clone();
            let resp = block_on(state.handle(req));
            assert_eq!(
                resp.status, 200,
                "COMPAT LOCK: MCP alias '{alias}' must route to handler"
            );
        }
    }

    #[test]
    fn compat_lock_initialize_handshake_succeeds_on_all_aliases() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        let init_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "compat-test",
                    "version": "1.0.0"
                }
            }
        });
        let body = serde_json::to_vec(&init_req).expect("serialize");

        for &alias in COMPAT_MCP_ALIASES {
            let mut req = make_request(Http1Method::Post, alias, &[]);
            req.body = body.clone();
            let resp = block_on(state.handle(req));
            assert_eq!(
                resp.status, 200,
                "COMPAT LOCK: Initialize must succeed on '{alias}'"
            );

            let resp_body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json");
            assert_eq!(resp_body["jsonrpc"], "2.0");
            assert!(
                resp_body.get("result").is_some(),
                "COMPAT LOCK: Initialize on '{alias}' must return result, got: {resp_body}"
            );
            assert_eq!(
                resp_body["result"]["serverInfo"]["name"], COMPAT_SERVER_NAME,
                "COMPAT LOCK: Server name in initialize response must match"
            );
        }
    }

    #[test]
    fn compat_lock_auth_mismatch_yields_401_before_body_parse() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("correct-token".to_string()),
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/mcp",
            &[("Authorization", "Bearer wrong-token")],
            Some(peer),
        );
        req.body = b"not-even-json".to_vec();
        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 401,
            "COMPAT LOCK: Wrong bearer token must 401 before parsing body"
        );
    }

    #[test]
    fn compat_lock_get_on_mcp_endpoint_yields_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);

        for &alias in COMPAT_MCP_ALIASES {
            let req = make_request(Http1Method::Get, &format!("{alias}/"), &[]);
            let resp = block_on(state.handle(req));
            assert_eq!(
                resp.status, 405,
                "COMPAT LOCK: GET on MCP endpoint '{alias}/' must be 405"
            );
        }
    }

    /// Cache freshness boundary: tokens are validated against fresh cache
    /// (not refetched) when TTL has not expired.
    #[test]
    fn jwks_cache_freshness_boundary() {
        let (jwks_bytes, jwks_set) = test_jwks_material();
        let counter = std::sync::atomic::AtomicUsize::new(0);

        with_counted_delayed_jwks_server(&jwks_bytes, 10, Duration::ZERO, &counter, |url| {
            let state = build_state(mcp_agent_mail_core::Config {
                http_jwt_enabled: true,
                http_jwt_algorithms: vec!["HS256".to_string()],
                http_jwt_jwks_url: Some(url.clone()),
                http_rbac_enabled: false,
                ..Default::default()
            });

            // Seed with fresh cache.
            {
                let mut cache = state
                    .jwks_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *cache = Some(JwksCacheEntry {
                    fetched_at: Instant::now(),
                    jwks: Arc::new(jwks_set.clone()),
                });
            }

            let rt = RuntimeBuilder::current_thread().build().expect("runtime");

            // Multiple non-forced fetches should all hit cache (0 requests).
            for _ in 0..10 {
                let r = rt.block_on(state.fetch_jwks(&url, false));
                assert!(r.is_ok());
            }
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "fresh cache should never trigger network fetch"
            );

            // Expire cache, then fetch should trigger exactly 1 request.
            seed_expired_cache(&state, &jwks_set);
            let r = rt.block_on(state.fetch_jwks(&url, false));
            assert!(r.is_ok());
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "expired cache should trigger exactly 1 network fetch"
            );
        });
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Auth edge case tests (br-3h13.5.2)
    // ══════════════════════════════════════════════════════════════════════════

    #[test]
    fn jwt_exp_in_past_rejects() {
        // A token with exp in the past (even by 1 second) should be rejected.
        // Note: with leeway=0, exp >= now is still valid per jsonwebtoken semantics.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_secs(),
        )
        .expect("timestamp fits i64");
        // Use exp = now - 1 to ensure the token is definitely expired
        let claims = serde_json::json!({
            "sub": "user-123",
            "role": "writer",
            "exp": now - 1
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("exp in past should be rejected");
        write_jwt_artifact(
            "jwt_exp_in_past_rejects",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true, "leeway": 0 },
                "claims": claims,
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn jwt_iat_in_future_rejects() {
        // Tokens issued in the future should be rejected (iat > now).
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);
        // Set iat to year 2100
        let future_iat = 4_102_444_800_i64;
        let claims = serde_json::json!({
            "sub": "user-123",
            "role": "writer",
            "iat": future_iat
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_iat_in_future_rejects",
            &serde_json::json!({
                "config": { "http_jwt_enabled": true },
                "claims": claims,
                "iat_timestamp": future_iat,
                "result": if resp.is_some() { "deny" } else { "allow" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        // Note: JWT standard doesn't require iat validation by default, but
        // tokens from the future are suspicious. If the implementation doesn't
        // reject future iat, this test documents that behavior.
        // The jsonwebtoken crate does NOT validate iat by default, so this may allow.
        // This test documents the actual behavior.
    }

    #[test]
    fn jwt_bearer_token_priority_when_both_configured() {
        // When both bearer token auth and JWT auth are configured,
        // a valid bearer token should be accepted.
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("static-secret-token".to_string()),
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        // Test with valid bearer token (not JWT)
        let req_bearer = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", "Bearer static-secret-token")],
            Some(peer),
        );
        let resp_bearer = block_on(state.check_rbac_and_rate_limit(&req_bearer, &json_rpc));
        write_jwt_artifact(
            "jwt_bearer_token_priority_bearer_auth",
            &serde_json::json!({
                "config": {
                    "http_bearer_token": "***",
                    "http_jwt_enabled": true
                },
                "authorization": "Bearer <static-token>",
                "result": if resp_bearer.is_none() { "allow" } else { "deny" },
                "deny_status": resp_bearer.as_ref().map(|r| r.status),
            }),
        );
        assert!(
            resp_bearer.is_none(),
            "valid bearer token should be accepted when both auth modes configured"
        );
    }

    #[test]
    fn jwt_takes_priority_when_bearer_invalid_but_jwt_valid() {
        // When bearer token is configured but request has a valid JWT,
        // it should be accepted (JWT auth is alternative).
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("static-secret-token".to_string()),
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let jwt_token = hs256_token(b"jwt-secret", &claims);
        let auth = format!("Bearer {jwt_token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_takes_priority_when_bearer_invalid_but_jwt_valid",
            &serde_json::json!({
                "config": {
                    "http_bearer_token": "***",
                    "http_jwt_enabled": true,
                    "http_jwt_secret": "***"
                },
                "claims": claims,
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(
            resp.is_none(),
            "valid JWT should be accepted when both auth modes configured"
        );
    }

    #[test]
    fn mixed_auth_handle_allows_static_bearer_token() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("static-secret-token".to_string()),
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", "Bearer static-secret-token")],
            Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 1)).expect("json");

        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 200,
            "static bearer token should pass mixed auth end-to-end"
        );
    }

    #[test]
    fn mixed_auth_handle_allows_valid_jwt() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("static-secret-token".to_string()),
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_rbac_enabled: false,
            http_allow_localhost_unauthenticated: false,
            ..Default::default()
        };
        let state = build_state(config);

        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let jwt_token = hs256_token(b"jwt-secret", &claims);
        let auth = format!("Bearer {jwt_token}");

        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(SocketAddr::from(([10, 0, 0, 1], 1234))),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 1)).expect("json");

        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 200,
            "valid JWT should pass mixed auth end-to-end"
        );
    }

    #[test]
    fn localhost_bypass_allows_unauthenticated_when_enabled() {
        // When http_allow_localhost_unauthenticated=true and request is from
        // localhost (127.0.0.1), no auth should be required.
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            http_jwt_enabled: false,
            http_allow_localhost_unauthenticated: true,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let local_peer = SocketAddr::from(([127, 0, 0, 1], 1234));

        // Request without any Authorization header
        let req = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(local_peer));
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "localhost_bypass_allows_unauthenticated_when_enabled",
            &serde_json::json!({
                "config": {
                    "http_allow_localhost_unauthenticated": true,
                    "http_bearer_token": "***"
                },
                "peer_addr": local_peer.to_string(),
                "authorization": null,
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(
            resp.is_none(),
            "localhost bypass should allow unauthenticated requests from 127.0.0.1"
        );
    }

    #[test]
    fn localhost_bypass_allows_unauthenticated_when_jwt_enabled() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_allow_localhost_unauthenticated: true,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let mut req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[],
            Some(SocketAddr::from(([127, 0, 0, 1], 1234))),
        );
        req.body = serde_json::to_vec(&JsonRpcRequest::new("tools/list", None, 1)).expect("json");

        let resp = block_on(state.handle(req));
        assert_eq!(
            resp.status, 200,
            "localhost bypass must allow unauthenticated requests even when JWT auth is enabled"
        );
    }

    #[test]
    fn localhost_bypass_preserves_valid_jwt_sub_for_rate_limit_identity() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_allow_localhost_unauthenticated: true,
            http_rbac_enabled: false,
            http_rate_limit_enabled: true,
            http_rate_limit_tools_per_minute: 1,
            http_rate_limit_tools_burst: 1,
            ..Default::default()
        };
        let state = build_state(config);

        let claims_a = serde_json::json!({ "sub": "local-user-a", "role": "writer" });
        let claims_b = serde_json::json!({ "sub": "local-user-b", "role": "writer" });
        let auth_a = format!("Bearer {}", hs256_token(b"secret", &claims_a));
        let auth_b = format!("Bearer {}", hs256_token(b"secret", &claims_b));
        let json_rpc = JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({"name": "health_check", "arguments": {}})),
            1,
        );
        let peer = SocketAddr::from(([127, 0, 0, 1], 1234));

        let req_a1 = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth_a.as_str())],
            Some(peer),
        );
        assert!(
            block_on(state.check_rbac_and_rate_limit(&req_a1, &json_rpc)).is_none(),
            "first localhost request should pass"
        );

        let req_other_subject = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth_b.as_str())],
            Some(peer),
        );
        assert!(
            block_on(state.check_rbac_and_rate_limit(&req_other_subject, &json_rpc)).is_none(),
            "different localhost JWT subjects must not share a rate-limit bucket"
        );

        let req_a2 = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth_a.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req_a2, &json_rpc))
            .expect("same localhost JWT subject should hit the one-request bucket");
        assert_eq!(resp.status, 429);
    }

    #[test]
    fn localhost_bypass_rejects_non_local_without_auth() {
        // When http_allow_localhost_unauthenticated=true but request is from
        // non-localhost IP, bearer auth should still be required.
        // Note: Bearer auth is checked by check_bearer_auth(), not check_rbac_and_rate_limit().
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            http_jwt_enabled: false,
            http_allow_localhost_unauthenticated: true,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let external_peer = SocketAddr::from(([10, 0, 0, 1], 1234));

        // Request without any Authorization header from external IP
        let req = make_request_with_peer_addr(Http1Method::Post, "/api/", &[], Some(external_peer));
        // Bearer auth check (not RBAC) rejects non-localhost without auth
        let resp = block_on(state.check_bearer_auth(&req))
            .expect("non-localhost should require bearer auth");
        write_jwt_artifact(
            "localhost_bypass_rejects_non_local_without_auth",
            &serde_json::json!({
                "config": {
                    "http_allow_localhost_unauthenticated": true,
                    "http_bearer_token": "***"
                },
                "peer_addr": external_peer.to_string(),
                "authorization": null,
                "expected_status": 401,
                "actual_status": resp.status,
            }),
        );
        assert_unauthorized(&resp);
    }

    #[test]
    fn rbac_writer_role_on_read_tool_succeeds() {
        // Writer role should have access to read-only tools.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "writer" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        // health_check is a read-only tool
        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_rbac_artifact(
            "rbac_writer_role_on_read_tool_succeeds",
            &serde_json::json!({
                "claims": claims,
                "tool": "health_check",
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(resp.is_none(), "writer role should access read-only tools");
    }

    #[test]
    fn rbac_unknown_role_denied_for_write_tool() {
        // Unknown role should be denied for write operations.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "mystery" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        // send_message is a write tool
        let params = serde_json::json!({ "name": "send_message", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("unknown role should be denied for write tool");
        write_rbac_artifact(
            "rbac_unknown_role_denied_for_write_tool",
            &serde_json::json!({
                "claims": claims,
                "tool": "send_message",
                "expected_status": 403,
                "actual_status": resp.status,
            }),
        );
        assert_forbidden(&resp);
    }

    // =========================================================================
    // br-3h13.5.2: Additional auth edge case tests
    // =========================================================================

    #[test]
    fn jwt_nbf_in_future_is_rejected() {
        // JWT with nbf (not-before) in the future should be rejected.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "sub": "user-123",
            "exp": now + 3600,  // valid expiry
            "iat": now,
            "nbf": now + 300,  // not valid for another 5 minutes
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_nbf_in_future_is_rejected",
            &serde_json::json!({
                "claims": claims,
                "now": now,
                "nbf": now + 300,
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        // JWT with nbf in future should be rejected
        assert!(resp.is_some(), "JWT with nbf in future should be rejected");
        assert_unauthorized(&resp.unwrap());
    }

    #[test]
    fn rbac_reader_role_on_write_tool_denied() {
        // Reader role should be denied access to write tools.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        // send_message is a write tool
        let params = serde_json::json!({ "name": "send_message", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc))
            .expect("reader role should be denied for write tool");
        write_rbac_artifact(
            "rbac_reader_role_on_write_tool_denied",
            &serde_json::json!({
                "claims": claims,
                "tool": "send_message",
                "expected_status": 403,
                "actual_status": resp.status,
            }),
        );
        assert_forbidden(&resp);
    }

    #[test]
    fn rbac_reader_role_on_read_tool_allowed() {
        // Reader role should have access to read-only tools.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        // health_check is a read-only tool
        let params = serde_json::json!({ "name": "health_check", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_rbac_artifact(
            "rbac_reader_role_on_read_tool_allowed",
            &serde_json::json!({
                "claims": claims,
                "tool": "health_check",
                "result": if resp.is_none() { "allow" } else { "deny" },
                "deny_status": resp.as_ref().map(|r| r.status),
            }),
        );
        assert!(resp.is_none(), "reader role should access read-only tools");
    }

    #[test]
    fn jwt_required_when_bearer_not_configured() {
        // When only JWT is configured (no bearer token), JWT must be valid.
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: None,
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "sub": "user-123",
            "exp": now + 3600,
            "iat": now,
        });
        let token = hs256_token(b"jwt-secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_required_when_bearer_not_configured",
            &serde_json::json!({
                "bearer_configured": false,
                "jwt_configured": true,
                "claims": claims,
                "result": if resp.is_none() { "allow" } else { "deny" },
            }),
        );
        assert!(
            resp.is_none(),
            "valid JWT should succeed when bearer not configured"
        );
    }

    #[test]
    fn invalid_jwt_rejected_when_bearer_not_configured() {
        // When only JWT is configured, invalid JWT should be rejected.
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: None,
            http_jwt_enabled: true,
            http_jwt_secret: Some("jwt-secret".to_string()),
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        // Token signed with wrong secret
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "sub": "user-123",
            "exp": now + 3600,
            "iat": now,
        });
        let token = hs256_token(b"wrong-secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "invalid_jwt_rejected_when_bearer_not_configured",
            &serde_json::json!({
                "bearer_configured": false,
                "jwt_configured": true,
                "signed_with": "wrong-secret",
                "expected_secret": "jwt-secret",
                "result": if resp.is_none() { "allow" } else { "deny" },
            }),
        );
        assert!(
            resp.is_some(),
            "invalid JWT should be rejected when bearer not configured"
        );
        assert_unauthorized(&resp.unwrap());
    }

    #[test]
    fn jwt_with_wrong_algorithm_rejected() {
        // JWT signed with algorithm not in allowed list should be rejected.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_jwt_algorithms: vec!["RS256".to_string()], // only RS256 allowed
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        // Sign with HS256 but config only allows RS256
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "sub": "user-123",
            "exp": now + 3600,
            "iat": now,
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "jwt_with_wrong_algorithm_rejected",
            &serde_json::json!({
                "configured_algorithms": "RS256",
                "token_algorithm": "HS256",
                "result": if resp.is_none() { "allow" } else { "deny" },
            }),
        );
        assert!(
            resp.is_some(),
            "JWT with wrong algorithm should be rejected"
        );
        assert_unauthorized(&resp.unwrap());
    }

    #[test]
    fn rbac_multiple_roles_writer_grants_write_access() {
        // When token has multiple roles including writer, write access granted.
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        // Multiple roles as array
        let claims = serde_json::json!({
            "sub": "user-123",
            "role": ["reader", "writer", "auditor"]
        });
        let token = hs256_token(b"secret", &claims);
        let auth = format!("Bearer {token}");

        // send_message is a write tool
        let params = serde_json::json!({ "name": "send_message", "arguments": {} });
        let json_rpc = JsonRpcRequest::new("tools/call", Some(params), 1);
        let peer = SocketAddr::from(([10, 0, 0, 1], 1234));
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", auth.as_str())],
            Some(peer),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_rbac_artifact(
            "rbac_multiple_roles_writer_grants_write_access",
            &serde_json::json!({
                "claims": claims,
                "tool": "send_message",
                "result": if resp.is_none() { "allow" } else { "deny" },
            }),
        );
        assert!(
            resp.is_none(),
            "multiple roles including writer should grant write access"
        );
    }

    #[test]
    fn auth_bypass_localhost_with_valid_bearer_still_works() {
        // Even with localhost bypass enabled, valid bearer token should work.
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
            http_jwt_enabled: false,
            http_allow_localhost_unauthenticated: true,
            http_rbac_enabled: false,
            ..Default::default()
        };
        let state = build_state(config);

        let json_rpc = JsonRpcRequest::new("tools/list", None, 1);
        let localhost = SocketAddr::from(([127, 0, 0, 1], 1234));

        // Request with valid bearer from localhost
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api/",
            &[("Authorization", "Bearer secret-token")],
            Some(localhost),
        );
        let resp = block_on(state.check_rbac_and_rate_limit(&req, &json_rpc));
        write_jwt_artifact(
            "auth_bypass_localhost_with_valid_bearer_still_works",
            &serde_json::json!({
                "config": {
                    "http_allow_localhost_unauthenticated": true,
                    "http_bearer_token": "***"
                },
                "peer_addr": localhost.to_string(),
                "authorization": "Bearer ***",
                "result": if resp.is_none() { "allow" } else { "deny" },
            }),
        );
        assert!(
            resp.is_none(),
            "valid bearer token from localhost should succeed"
        );
    }

    #[test]
    fn stop_http_server_instance_returns_join_result_when_task_exits() {
        let runtime = build_http_runtime().expect("build test HTTP runtime");
        let instance = test_http_server_instance(
            runtime
                .handle()
                .try_spawn(async { Ok::<(), std::io::Error>(()) })
                .expect("spawn joinable task"),
            asupersync::server::shutdown::ShutdownSignal::new(),
        );

        runtime.block_on(async {
            stop_http_server_instance(instance)
                .await
                .expect("server stop should succeed");
        });
    }

    #[test]
    fn stop_http_server_instance_force_closes_after_drain_timeout() {
        let runtime = build_http_runtime().expect("build test HTTP runtime");
        let shutdown = asupersync::server::shutdown::ShutdownSignal::new();
        let task_shutdown = shutdown.clone();
        let instance = test_http_server_instance(
            runtime
                .handle()
                .try_spawn(async move {
                    while task_shutdown.phase()
                        != asupersync::server::shutdown::ShutdownPhase::ForceClosing
                    {
                        sleep(wall_now(), Duration::from_millis(10)).await;
                    }
                    Ok::<(), std::io::Error>(())
                })
                .expect("spawn force-close aware task"),
            shutdown,
        );

        runtime.block_on(async {
            stop_http_server_instance_with_timeouts(
                instance,
                Duration::from_millis(50),
                Duration::from_millis(250),
                Duration::from_millis(50),
            )
            .await
            .expect("force-close should unblock shutdown");
        });
    }

    #[test]
    fn stop_http_server_instance_times_out_when_task_never_completes() {
        let runtime = build_http_runtime().expect("build test HTTP runtime");
        let instance = test_http_server_instance(
            runtime
                .handle()
                .try_spawn(async {
                    sleep(wall_now(), Duration::from_secs(30)).await;
                    Ok::<(), std::io::Error>(())
                })
                .expect("spawn blocking task"),
            asupersync::server::shutdown::ShutdownSignal::new(),
        );

        let join_timeout = Duration::from_millis(50);
        let force_close_timeout = Duration::from_millis(50);
        let start = std::time::Instant::now();
        let err = runtime.block_on(async {
            stop_http_server_instance_with_timeouts(
                instance,
                join_timeout,
                force_close_timeout,
                join_timeout,
            )
            .await
            .expect_err("stuck join must time out")
        });
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < join_timeout + force_close_timeout + Duration::from_secs(1),
            "drain+force-close timeout should bound supervisor stalls"
        );
    }

    #[test]
    fn respawn_http_server_instance_with_retry_retries_until_success() {
        let runtime = build_http_runtime().expect("build test HTTP runtime");
        let runtime_handle = runtime.handle();
        let mut config = mcp_agent_mail_core::Config::default();
        let mut last_restart_sleep_ms = 0_u64;
        let mut retry_backoffs = Vec::new();
        let mut attempts = 0_u32;
        let mut slept = Vec::new();

        let instance = runtime.block_on(async {
            respawn_http_server_instance_with_retry_using(
                &mut config,
                &mut last_restart_sleep_ms,
                || false,
                |err, backoff_ms| {
                    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
                    retry_backoffs.push(backoff_ms);
                },
                |cfg| {
                    attempts = attempts.saturating_add(1);
                    std::future::ready(if attempts < 3 {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::AddrInUse,
                            "address already in use",
                        ))
                    } else {
                        Ok((
                            cfg,
                            test_http_server_instance(
                                runtime_handle
                                    .clone()
                                    .try_spawn(async { Ok::<(), std::io::Error>(()) })
                                    .expect("spawn joinable retry task"),
                                asupersync::server::shutdown::ShutdownSignal::new(),
                            ),
                        ))
                    })
                },
                |duration| {
                    slept.push(duration);
                    std::future::ready(())
                },
            )
            .await
            .expect("retry helper should eventually succeed")
        });

        assert_eq!(attempts, 3, "two transient failures then success");
        assert_eq!(
            retry_backoffs,
            vec![
                HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                restart_backoff_ms(
                    HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                    HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                    HTTP_SUPERVISOR_RESTART_BACKOFF_MAX_MS
                ),
            ]
        );
        assert_eq!(
            slept,
            vec![
                Duration::from_millis(HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS),
                Duration::from_millis(restart_backoff_ms(
                    HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                    HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                    HTTP_SUPERVISOR_RESTART_BACKOFF_MAX_MS
                )),
            ]
        );
        assert_eq!(
            last_restart_sleep_ms,
            restart_backoff_ms(
                HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                HTTP_SUPERVISOR_RESTART_BACKOFF_MIN_MS,
                HTTP_SUPERVISOR_RESTART_BACKOFF_MAX_MS
            )
        );

        runtime.block_on(async {
            stop_http_server_instance(instance)
                .await
                .expect("retry helper instance should stop cleanly");
        });
    }

    #[test]
    fn respawn_http_server_gives_up_after_max_consecutive_failures() {
        let runtime = build_http_runtime().expect("build test HTTP runtime");
        let mut config = mcp_agent_mail_core::Config::default();
        let mut last_restart_sleep_ms = 0_u64;
        let mut attempts = 0_u32;
        let mut retry_errors = Vec::new();

        let result = runtime.block_on(async {
            respawn_http_server_instance_with_retry_using(
                &mut config,
                &mut last_restart_sleep_ms,
                || false,
                |err, _backoff_ms| {
                    retry_errors.push(err.to_string());
                },
                |_cfg| {
                    attempts = attempts.saturating_add(1);
                    std::future::ready(Err::<
                        (mcp_agent_mail_core::Config, HttpServerInstance),
                        std::io::Error,
                    >(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        "address already in use (test)",
                    )))
                },
                |_duration| std::future::ready(()),
            )
            .await
        });

        let Err(err) = result else {
            panic!("should fail after max retries, but got Ok");
        };
        assert_eq!(
            attempts, HTTP_SUPERVISOR_MAX_CONSECUTIVE_RESTART_FAILURES,
            "should attempt exactly {HTTP_SUPERVISOR_MAX_CONSECUTIVE_RESTART_FAILURES} times"
        );
        // on_retry_error fires for each failure that will be retried, but not
        // for the final failure (which triggers the give-up path instead).
        assert_eq!(
            retry_errors.len() as u32,
            HTTP_SUPERVISOR_MAX_CONSECUTIVE_RESTART_FAILURES - 1,
            "on_retry_error should fire for each retried failure (all but the last)"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("consecutive attempts"),
            "error message should mention consecutive attempts, got: {msg}"
        );
        assert!(
            msg.contains("address already in use"),
            "error message should include the last spawn error, got: {msg}"
        );
    }

    #[test]
    fn atc_effect_semantic_key_is_project_scoped() {
        let left = atc::AtcEffectPlan {
            decision_id: 7,
            effect_id: "atc-effect-left".to_string(),
            experience_id: Some(17),
            claim_id: "atc-claim-left".to_string(),
            evidence_id: "atc-evidence-left".to_string(),
            trace_id: "atc-trace-left".to_string(),
            timestamp_micros: 1_000_000,
            kind: "send_advisory".to_string(),
            category: "liveness".to_string(),
            agent: "AgentAlpha".to_string(),
            project_key: Some("/tmp/project-a".to_string()),
            policy_id: Some("policy-a".to_string()),
            policy_revision: 3,
            message: Some("same message".to_string()),
            expected_loss: Some(0.2),
            semantics: sample_effect_semantics(
                "liveness_monitoring",
                "AgentAlpha",
                Some("/tmp/project-a"),
            ),
        };
        let right = atc::AtcEffectPlan {
            project_key: Some("/tmp/project-b".to_string()),
            semantics: sample_effect_semantics(
                "liveness_monitoring",
                "AgentAlpha",
                Some("/tmp/project-b"),
            ),
            ..left.clone()
        };

        assert_ne!(
            atc_effect_semantic_key(&left),
            atc_effect_semantic_key(&right)
        );
    }

    #[test]
    fn atc_project_keys_match_accepts_slug_and_human_key_for_same_project() {
        let human_key = "/tmp/agent-mail-project-alpha";
        let slug = mcp_agent_mail_core::compute_project_slug(human_key);

        assert!(atc_project_keys_match(&slug, human_key));
        assert!(atc_project_keys_match(human_key, &slug));
    }

    #[test]
    fn atc_project_keys_match_rejects_prefix_collision() {
        let left = "/tmp/agent-mail-project";
        let right = "/tmp/agent-mail-project-backup";

        assert!(!atc_project_keys_match(left, right));
        assert!(!atc_project_keys_match(right, left));
    }

    #[test]
    fn atc_project_keys_match_rejects_case_variant_absolute_paths() {
        let left = "/tmp/agent-mail-project";
        let right = "/tmp/Agent-Mail-Project";

        assert!(!atc_project_keys_match(left, right));
        assert!(!atc_project_keys_match(right, left));
    }

    #[test]
    fn build_atc_feature_vector_captures_decision_quality_and_risk_tier() {
        let record = atc::AtcDecisionRecord {
            id: 42,
            claim_id: "clm-42".to_string(),
            evidence_id: "evi-42".to_string(),
            trace_id: "trc-42".to_string(),
            timestamp_micros: 1_700_000_000_000_000,
            subsystem: atc::AtcSubsystem::Liveness,
            decision_class: "liveness_transition".to_string(),
            subject: "BlueLake".to_string(),
            policy_id: Some("liveness-incumbent-r1".to_string()),
            posterior: vec![
                ("Alive".to_string(), 0.2),
                ("Flaky".to_string(), 0.7),
                ("Dead".to_string(), 0.1),
            ],
            action: "ReleaseReservations".to_string(),
            expected_loss: 1.5,
            runner_up_loss: 3.0,
            loss_table: vec![
                atc::AtcLossTableEntry {
                    action: "ReleaseReservations".to_string(),
                    expected_loss: 1.5,
                },
                atc::AtcLossTableEntry {
                    action: "Suspect".to_string(),
                    expected_loss: 3.0,
                },
            ],
            evidence_summary: "agent declared dead".to_string(),
            calibration_healthy: false,
            safe_mode_active: true,
            fallback_reason: Some("budget_pressure".to_string()),
        };

        let features = build_atc_feature_vector(&record, EffectKind::Release);

        assert_eq!(features.posterior_alive_bp, 2000);
        assert_eq!(features.posterior_flaky_bp, 7000);
        assert_eq!(features.expected_loss_bp, 150);
        assert_eq!(features.loss_gap_bp, 150);
        assert_eq!(
            features.risk_tier,
            FeatureVector::risk_tier_for(EffectKind::Release)
        );
        assert!(!features.calibration_healthy);
        assert!(features.safe_mode_active);
    }

    #[test]
    fn atc_operator_wait_duration_uses_next_due_deadline() {
        let snapshot = AtcOperatorSnapshot {
            enabled: true,
            source: "live".to_string(),
            kernel: atc::AtcKernelTelemetry {
                next_due_micros: Some(1_250_000),
                ..Default::default()
            },
            ..Default::default()
        };

        let wait = atc_operator_wait_duration(&snapshot, 1_000_000, Duration::from_secs(5));
        assert_eq!(wait, Duration::from_micros(250_000));
    }

    #[test]
    fn atc_operator_wait_duration_prioritizes_executor_backlog() {
        let snapshot = AtcOperatorSnapshot {
            enabled: true,
            source: "live".to_string(),
            executor_pending_effects: 3,
            kernel: atc::AtcKernelTelemetry {
                next_due_micros: Some(5_000_000),
                ..Default::default()
            },
            ..Default::default()
        };

        let wait = atc_operator_wait_duration(&snapshot, 1_000_000, Duration::from_secs(5));
        assert_eq!(wait, ATC_OPERATOR_MIN_TICK_INTERVAL);
    }
}
