#![forbid(unsafe_code)]

mod ack_ttl;
mod cleanup;
pub mod console;
mod mail_ui;
mod markdown;
mod retention;
mod static_files;
mod templates;
pub mod theme;
mod tool_metrics;

use asupersync::http::h1::HttpClient;
use asupersync::http::h1::listener::Http1Listener;
use asupersync::http::h1::types::{
    Method as Http1Method, Request as Http1Request, Response as Http1Response, default_reason,
};
use asupersync::messaging::RedisClient;
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{timeout, wall_now};
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
use mcp_agent_mail_db::{
    DbPoolConfig, QueryTracker, active_tracker, create_pool, set_active_tracker,
};
use mcp_agent_mail_tools::{
    AcknowledgeMessage, AcquireBuildSlot, AgentsListResource, ConfigEnvironmentQueryResource,
    ConfigEnvironmentResource, CreateAgentIdentity, EnsureProduct, EnsureProject, FetchInbox,
    FetchInboxProduct, FileReservationPaths, FileReservationsResource, ForceReleaseFileReservation,
    HealthCheck, IdentityProjectResource, InboxResource, InstallPrecommitGuard, ListContacts,
    MacroContactHandshake, MacroFileReservationCycle, MacroPrepareThread, MacroStartSession,
    MailboxResource, MailboxWithCommitsResource, MarkMessageRead, MessageDetailsResource,
    OutboxResource, ProductDetailsResource, ProductsLink, ProjectDetailsResource,
    ProjectsListQueryResource, ProjectsListResource, RegisterAgent, ReleaseBuildSlot,
    ReleaseFileReservations, RenewBuildSlot, RenewFileReservations, ReplyMessage, RequestContact,
    RespondContact, SearchMessages, SearchMessagesProduct, SendMessage, SetContactPolicy,
    SummarizeThread, SummarizeThreadProduct, ThreadDetailsResource, ToolingCapabilitiesResource,
    ToolingDirectoryQueryResource, ToolingDirectoryResource, ToolingLocksQueryResource,
    ToolingLocksResource, ToolingMetricsQueryResource, ToolingMetricsResource,
    ToolingRecentResource, ToolingSchemasQueryResource, ToolingSchemasResource,
    UninstallPrecommitGuard, ViewsAckOverdueResource, ViewsAckRequiredResource,
    ViewsAcksStaleResource, ViewsUrgentUnreadResource, Whois, clusters,
};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct InstrumentedTool<T> {
    tool_name: String,
    inner: T,
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
        mcp_agent_mail_tools::record_call(&self.tool_name);
        match self.inner.call(ctx, arguments) {
            Ok(v) => Ok(v),
            Err(e) => {
                mcp_agent_mail_tools::record_error(&self.tool_name);
                Err(e)
            }
        }
    }

    fn call_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
        mcp_agent_mail_tools::record_call(&self.tool_name);
        Box::pin(async move {
            let out = self.inner.call_async(ctx, arguments).await;
            if !matches!(out, fastmcp_core::Outcome::Ok(_)) {
                mcp_agent_mail_tools::record_error(&self.tool_name);
            }
            out
        })
    }
}

fn add_tool<T: fastmcp::ToolHandler + 'static>(
    server: fastmcp_server::ServerBuilder,
    config: &mcp_agent_mail_core::Config,
    tool_name: &str,
    cluster: &str,
    tool: T,
) -> fastmcp_server::ServerBuilder {
    if config.should_expose_tool(tool_name, cluster) {
        server.tool(InstrumentedTool {
            tool_name: tool_name.to_string(),
            inner: tool,
        })
    } else {
        server
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_server(config: &mcp_agent_mail_core::Config) -> Server {
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

pub fn run_stdio(config: &mcp_agent_mail_core::Config) {
    // Initialize console theme from parsed config (includes persisted envfile values).
    let _ = theme::init_console_theme_from_config(config.console_theme);
    // Enable global query tracker if instrumentation is on.
    if config.instrumentation_enabled {
        mcp_agent_mail_db::QUERY_TRACKER.enable(Some(config.instrumentation_slow_query_ms));
    }
    mcp_agent_mail_storage::wbq_start();
    build_server(config).run_stdio();
    // run_stdio() does not return; WBQ drain thread exits with the process.
}

pub fn run_http(config: &mcp_agent_mail_core::Config) -> std::io::Result<()> {
    // Initialize console theme from parsed config (includes persisted envfile values).
    let _ = theme::init_console_theme_from_config(config.console_theme);
    // Enable global query tracker if instrumentation is on.
    if config.instrumentation_enabled {
        mcp_agent_mail_db::QUERY_TRACKER.enable(Some(config.instrumentation_slow_query_ms));
    }
    mcp_agent_mail_storage::wbq_start();
    cleanup::start(config);
    ack_ttl::start(config);
    tool_metrics::start(config);
    retention::start(config);
    let dashboard = StartupDashboard::maybe_start(config);
    set_dashboard_handle(dashboard.clone());

    let server = build_server(config);
    let server_info = server.info().clone();
    let server_capabilities = server.capabilities().clone();
    let router = Arc::new(server.into_router());

    let state = Arc::new(HttpState::new(
        router,
        server_info,
        server_capabilities,
        config.clone(),
    ));

    let addr = format!("{}:{}", config.http_host, config.http_port);
    let runtime = RuntimeBuilder::new()
        .build()
        .map_err(|e| map_asupersync_err(&e))?;

    let handle = runtime.handle();
    let result = runtime.block_on(async move {
        let handler_state = Arc::clone(&state);
        let listener = Http1Listener::bind(addr, move |req| {
            let inner = Arc::clone(&handler_state);
            async move { inner.handle(req).await }
        })
        .await?;

        listener.run(&handle).await?;
        Ok::<(), std::io::Error>(())
    });

    retention::shutdown();
    tool_metrics::shutdown();
    ack_ttl::shutdown();
    cleanup::shutdown();
    mcp_agent_mail_storage::wbq_shutdown();
    if let Some(dashboard) = dashboard.as_ref() {
        dashboard.shutdown();
    }
    set_dashboard_handle(None);
    result
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
static LIVE_DASHBOARD: std::sync::LazyLock<Mutex<Option<Arc<StartupDashboard>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

const JWKS_CACHE_TTL: Duration = Duration::from_secs(60);
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

struct StartupDashboard {
    writer: Mutex<ftui::TerminalWriter<std::io::Stdout>>,
    stop: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    input_worker: Mutex<Option<JoinHandle<()>>>,
    started_at: Instant,
    endpoint: String,
    web_ui: String,
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
}

impl StartupDashboard {
    fn maybe_start(config: &mcp_agent_mail_core::Config) -> Option<Arc<Self>> {
        if !config.log_rich_enabled || !std::io::stdout().is_terminal() {
            return None;
        }

        let term_width = parse_env_u16("COLUMNS", 120).max(80);
        let term_height = parse_env_u16("LINES", 36).max(20);
        let console_layout = ConsoleLayoutState::from_config(config);
        let (screen_mode, ui_anchor) = console_layout.compute_writer_settings(term_height);
        let term_caps = ftui::TerminalCapabilities::detect();
        let console_caps = console::ConsoleCaps::from_capabilities(&term_caps);

        // Emit a grep-friendly console summary before engaging AltScreen so PTY capture and
        // terminal scrollback have stable, plain-text breadcrumbs (E2E + debugging).
        if console_layout.is_split_mode() {
            eprintln!("Console: {}", console_layout.summary_line());
            eprintln!("{}", console_caps.one_liner());
        }

        let mut writer =
            ftui::TerminalWriter::new(std::io::stdout(), screen_mode, ui_anchor, term_caps);
        writer.set_size(term_width, term_height);

        let endpoint = format!(
            "http://{}:{}{}",
            config.http_host, config.http_port, config.http_path
        );
        let web_ui = format!("http://{}:{}/mail", config.http_host, config.http_port);

        let dashboard = Arc::new(Self {
            writer: Mutex::new(writer),
            stop: AtomicBool::new(false),
            worker: Mutex::new(None),
            input_worker: Mutex::new(None),
            started_at: Instant::now(),
            endpoint,
            web_ui,
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
        });

        dashboard.refresh_db_stats();
        dashboard.render_now();
        dashboard.emit_startup_showcase(config);
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
                while !this.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1200));
                    if this.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    this.sparkline.sample();
                    this.refresh_db_stats();
                    this.render_now();
                }
            });

        if let Ok(join) = handle {
            *lock_mutex(&self.worker) = Some(join);
        }
    }

    fn spawn_console_input_worker(self: &Arc<Self>) {
        if !lock_mutex(&self.console_layout).interactive_enabled || !std::io::stdin().is_terminal()
        {
            return;
        }

        let this = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("mcp-agent-mail-dashboard-input".to_string())
            .spawn(move || {
                let Ok(session) = ftui::TerminalSession::minimal() else {
                    this.log_line("Console interactive mode: failed to enter raw mode");
                    return;
                };

                this.log_line(
                    "Console layout keys: +/- or Up/Down (height), t/b (anchor), a (auto-size), i/l (split request), [/ ] (split ratio), ? (help)",
                );

                while !this.stop.load(Ordering::Relaxed) {
                    if !session
                        .poll_event(std::time::Duration::from_millis(100))
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    while let Ok(Some(event)) = session.read_event() {
                        if this.handle_console_event(&session, &event) {
                            break;
                        }
                        if !session
                            .poll_event(std::time::Duration::from_millis(0))
                            .unwrap_or(false)
                        {
                            break;
                        }
                    }
                }
                drop(session);
            });

        if let Ok(join) = handle {
            *lock_mutex(&self.input_worker) = Some(join);
        }
    }

    fn handle_console_event(&self, session: &ftui::TerminalSession, event: &ftui::Event) -> bool {
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
            ftui::core::terminal_session::best_effort_cleanup_for_exit();
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
                            self.dispatch_palette_action(&id, session);
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

        // In split mode, route log-pane-specific keys to the LogViewer.
        if lock_mutex(&self.console_layout).is_split_mode() {
            let handled = self.handle_log_pane_key(key.code, event);
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

        let (term_width, term_height) = session.size().unwrap_or((80, 24));
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
    fn dispatch_palette_action(&self, id: &str, session: &ftui::TerminalSession) {
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
            }

            _ => {
                self.log_line(&format!("Console: unknown action '{id}'"));
            }
        }

        if layout_changed {
            let (term_width, term_height) = session.size().unwrap_or((80, 24));
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
        let mut writer = ftui::TerminalWriter::new(
            std::io::stdout(),
            screen_mode,
            ui_anchor,
            ftui::TerminalCapabilities::detect(),
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
                pane.push(l.to_string());
            }
        } else {
            let mut writer = lock_mutex(&self.writer);
            let _ = writer.write_log(&line);
        }
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
        self.render_now();
    }

    fn refresh_db_stats(&self) {
        *lock_mutex(&self.db_stats) = fetch_dashboard_db_stats(&self.database_url);
    }

    fn snapshot(&self) -> DashboardSnapshot {
        let requests_total = self.requests_total.load(Ordering::Relaxed);
        let latency_total_ms = self.latency_total_ms.load(Ordering::Relaxed);
        DashboardSnapshot {
            endpoint: self.endpoint.clone(),
            web_ui: self.web_ui.clone(),
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

    fn render_now(&self) {
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
        let mut writer = lock_mutex(&self.writer);
        let width = writer.width().max(80);
        let ui_height = writer.ui_height().max(8);
        let rendered = {
            let buffer = writer.take_render_buffer(width, ui_height);
            let (pool, links) = writer.pool_and_links_mut();
            let mut frame = ftui::Frame::from_buffer(buffer, pool);
            frame.links = Some(links);
            let area = Rect::new(0, 0, width, ui_height);
            if is_split {
                let mut pane = lock_mutex(&self.log_pane);
                console::render_split_frame(&mut frame, area, split_ratio, &mut pane, |f, a| {
                    render_dashboard_frame(f, a, &snapshot, phase, changed_rows);
                });
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
                    .border_type(BorderType::Ascii)
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
                .border_type(BorderType::Double)
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

    let left = format!(
        "Endpoint: {}\nWeb UI: {}\nAuth: {}\nStorage: {}\nDatabase: {}",
        compact_path(&snapshot.endpoint, 52),
        compact_path(&snapshot.web_ui, 52),
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

    Table::new(
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
    .style(card_style)
    .render(cols[1], frame);

    // Render agents panel (when agents exist)
    let traffic_col = if has_agents {
        let agent_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Agents ");

        // Narrow width: collapse to a single summary line
        if cols[2].width < 22 {
            let count = snapshot.db.agents_list.len();
            let summary = snapshot.db.agents_list.first().map_or_else(
                || format!("{count} agents"),
                |first| format!("{count} agents\n{}", first.name),
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
            Table::new(
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
            .style(dim_style)
            .render(cols[2], frame);
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
                .border_type(BorderType::Ascii)
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

fn pretty_num(value: u64) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + (s.len() / 3));
    for (idx, ch) in s.chars().enumerate() {
        if idx > 0 && (s.len() - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Format a relative time string like "12s ago", "5m ago", "2h ago".
fn relative_time_short(now_us: i64, ts_us: i64) -> String {
    let delta_s = (now_us.saturating_sub(ts_us)) / 1_000_000;
    if delta_s < 0 {
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

/// Strip ANSI escape sequences and return the visible character count.
#[allow(dead_code)]
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if ch == '\x1b' {
            in_esc = true;
        } else {
            len += unicode_char_width(ch);
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
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let mut out = String::from(indent);

    let mut tail = trimmed;
    // Try to match "key": value pattern
    if let Some(colon_pos) = tail.find(':') {
        let before_colon = &tail[..colon_pos];
        if before_colon.contains('"') {
            // It's a key
            out.push_str(key_color);
            out.push_str(before_colon);
            out.push_str(ansi_off);
            out.push(':');
            tail = &tail[colon_pos + 1..];

            // Check if value is numeric
            let val = tail.trim().trim_end_matches(',');
            if val.parse::<u64>().is_ok() || val.parse::<i64>().is_ok() {
                let before_val = &tail[..tail.len() - tail.trim_start().len()];
                let trimmed_tail = tail.trim_start();
                out.push_str(before_val);
                let has_comma = trimmed_tail.ends_with(',');
                let num_part = trimmed_tail.trim_end_matches(',');
                out.push_str(num_color);
                out.push_str(num_part);
                out.push_str(ansi_off);
                if has_comma {
                    out.push(',');
                }
            } else {
                out.push_str(tail);
            }
            return out;
        }
    }

    // Not a key-value line — just return as-is (braces, etc.)
    out.push_str(tail);
    out
}

fn fetch_dashboard_db_stats(database_url: &str) -> DashboardDbStats {
    let cfg = DbPoolConfig {
        database_url: database_url.to_string(),
        ..Default::default()
    };
    let Ok(path) = cfg.sqlite_path() else {
        return DashboardDbStats::default();
    };
    let Ok(conn) = mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection::open_file(&path) else {
        return DashboardDbStats::default();
    };
    let agents_list = conn
        .query_sync(
            "SELECT name, program, last_active_ts FROM agents \
             ORDER BY last_active_ts DESC LIMIT 10",
            &[],
        )
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    Some(AgentSummary {
                        name: row.get_named::<String>("name").ok()?,
                        program: row.get_named::<String>("program").ok()?,
                        last_active_ts: row.get_named::<i64>("last_active_ts").ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    DashboardDbStats {
        projects: dashboard_count(&conn, "SELECT COUNT(*) AS c FROM projects"),
        agents: dashboard_count(&conn, "SELECT COUNT(*) AS c FROM agents"),
        messages: dashboard_count(&conn, "SELECT COUNT(*) AS c FROM messages"),
        file_reservations: dashboard_count(
            &conn,
            "SELECT COUNT(*) AS c FROM file_reservations WHERE released_ts IS NULL",
        ),
        contact_links: dashboard_count(&conn, "SELECT COUNT(*) AS c FROM agent_links"),
        ack_pending: dashboard_count(
            &conn,
            "SELECT COUNT(*) AS c FROM message_recipients mr \
             JOIN messages m ON m.id = mr.message_id \
             WHERE m.ack_required = 1 AND mr.ack_ts IS NULL",
        ),
        agents_list,
    }
}

fn dashboard_count(conn: &mcp_agent_mail_db::sqlmodel_sqlite::SqliteConnection, sql: &str) -> u64 {
    conn.query_sync(sql, &[])
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
    /// Optional web root for SPA static file serving.
    web_root: Option<static_files::WebRoot>,
}

impl HttpState {
    fn new(
        router: Arc<fastmcp_server::Router>,
        server_info: fastmcp_protocol::ServerInfo,
        server_capabilities: fastmcp_protocol::ServerCapabilities,
        config: mcp_agent_mail_core::Config,
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
            web_root,
        }
    }

    #[allow(clippy::unused_async)] // Required for Http1Listener interface
    async fn handle(&self, req: Http1Request) -> Http1Response {
        let dashboard = dashboard_handle();
        if !self.config.http_request_log_enabled && dashboard.is_none() {
            return self.handle_inner(req).await;
        }

        let start = Instant::now();
        let method = req.method.clone();
        let (path, _query) = split_path_query(&req.uri);
        let client_ip = req
            .peer_addr
            .map_or_else(|| "-".to_string(), |addr| addr.ip().to_string());

        let resp = self.handle_inner(req).await;
        let dur_ms = u64::try_from(start.elapsed().as_millis().min(u128::from(u64::MAX)))
            .unwrap_or(u64::MAX);
        if let Some(dashboard) = dashboard.as_ref() {
            dashboard.record_request(method.as_str(), &path, resp.status, dur_ms, &client_ip);
        }
        if self.config.http_request_log_enabled || dashboard.is_some() {
            self.emit_http_request_log(method.as_str(), &path, resp.status, dur_ms, &client_ip);
        }
        resp
    }

    async fn handle_inner(&self, mut req: Http1Request) -> Http1Response {
        if let Some(resp) = self.handle_options(&req) {
            return resp;
        }

        let (path, _query) = split_path_query(&req.uri);
        // Legacy parity: `/health/*` bypasses bearer auth even when configured.
        // BearerAuthMiddleware in the legacy FastAPI stack checks only this prefix.
        if path.starts_with("/health/") {
            if let Some(resp) = self.handle_special_routes(&req, &path) {
                return resp;
            }
            return self.error_response(&req, 404, "Not Found");
        }

        // Legacy parity: bearer auth applies to all non-health routes (even unknown paths/methods),
        // so missing/invalid auth yields 401 instead of downstream 404/405/400.
        if let Some(resp) = self.check_bearer_auth(&req) {
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
        maybe_inject_localhost_authorization_for_base_passthrough(
            &self.config,
            &mut req,
            &path,
            &base_no_slash,
        );

        // Legacy parity: direct POST handler for `/base` forwards to the mounted `/base/` app.
        let effective_path = if base_no_slash == "/" || path != base_no_slash {
            path.clone()
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

        if let Some(resp) = self.check_rbac_and_rate_limit(&req, &json_rpc).await {
            return resp;
        }

        let response = self.dispatch(json_rpc).map_or_else(
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
            ftui_runtime::ftui_eprintln!("{line}");

            // Rich-ish panel output (stdout), fallback to legacy plain-text line on any error.
            // Gate: only render ANSI panel when rich output is enabled AND stdout is a TTY.
            let use_ansi = self.config.log_rich_enabled && std::io::stdout().is_terminal();
            if let Some(panel) = console::render_http_request_panel(
                100,
                method,
                path,
                status,
                duration_ms,
                client_ip,
                use_ansi,
            ) {
                if !dashboard_write_log(&panel) {
                    ftui_runtime::ftui_println!("{panel}");
                }
            } else {
                let fallback =
                    http_request_log_fallback_line(method, path, status, duration_ms, client_ip);
                if !dashboard_write_log(&fallback) {
                    ftui_runtime::ftui_println!("{fallback}");
                }
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

    fn handle_special_routes(&self, req: &Http1Request, path: &str) -> Option<Http1Response> {
        match path {
            "/health/liveness" => {
                if !matches!(req.method, Http1Method::Get) {
                    return Some(self.error_response(req, 405, "Method Not Allowed"));
                }
                return Some(self.json_response(req, 200, &serde_json::json!({"status":"alive"})));
            }
            "/health/readiness" => {
                if !matches!(req.method, Http1Method::Get) {
                    return Some(self.error_response(req, 405, "Method Not Allowed"));
                }
                if let Err(err) = readiness_check(&self.config) {
                    return Some(self.error_response(req, 503, &err));
                }
                return Some(self.json_response(req, 200, &serde_json::json!({"status":"ready"})));
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

        if path == "/mail/api/locks" {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let payload = match mcp_agent_mail_storage::collect_lock_status(&self.config) {
                Ok(v) => v,
                Err(err) => {
                    let msg = format!("lock status error: {err}");
                    return Some(self.error_response(req, 500, &msg));
                }
            };
            return Some(self.json_response(req, 200, &payload));
        }

        if path == "/mail" || path.starts_with("/mail/") {
            if !matches!(req.method, Http1Method::Get) {
                return Some(self.error_response(req, 405, "Method Not Allowed"));
            }
            let (_path_part, query_part) = split_path_query(&req.uri);
            let query_str = query_part.as_deref().unwrap_or("");
            match mail_ui::dispatch(path, query_str) {
                Ok(Some(body)) => {
                    let is_api = path.contains("/api/");
                    let content_type = if is_api {
                        "application/json"
                    } else {
                        "text/html; charset=utf-8"
                    };
                    return Some(self.raw_response(req, 200, content_type, body.into_bytes()));
                }
                Ok(None) => {
                    return Some(self.error_response(req, 404, "Not Found"));
                }
                Err((status, msg)) => {
                    if status == 404 {
                        let html = templates::render_template(
                            "error.html",
                            serde_json::json!({ "message": msg }),
                        )
                        .unwrap_or_else(|_| msg.clone());
                        return Some(self.raw_response(
                            req,
                            404,
                            "text/html; charset=utf-8",
                            html.into_bytes(),
                        ));
                    }
                    return Some(self.error_response(req, status, &msg));
                }
            }
        }

        // Static file serving from optional web/ SPA directory.
        // Only serve for GET requests on non-API paths (legacy Python: _is_api_path check).
        if let Some(ref web_root) = self.web_root {
            if matches!(req.method, Http1Method::Get) && !self.path_allowed(path) {
                if let Some((content_type, body)) = web_root.serve(path) {
                    let mut resp = self.raw_response(req, 200, content_type, body);
                    resp.headers.push((
                        "cache-control".to_string(),
                        "no-store, no-cache, must-revalidate".to_string(),
                    ));
                    return Some(resp);
                }
            }
        }

        None
    }

    /// Check if `path` is under the configured MCP base path.
    ///
    /// Legacy parity: `FastAPI` `mount(base_no_slash, app)` + `mount(base_with_slash, app)`
    /// routes the exact base **and** all sub-paths to the stateless MCP app.
    fn path_allowed(&self, path: &str) -> bool {
        let base = normalize_base_path(&self.config.http_path);
        if base == "/" {
            return true;
        }
        let base_no_slash = base.trim_end_matches('/');
        // Exact match: /api
        if path == base_no_slash {
            return true;
        }
        // With trailing slash and any sub-path: /api/ or /api/foo
        path.starts_with(&format!("{base_no_slash}/"))
    }

    fn check_bearer_auth(&self, req: &Http1Request) -> Option<Http1Response> {
        let Some(expected) = &self.config.http_bearer_token else {
            return None;
        };

        if self.allow_local_unauthenticated(req) {
            return None;
        }

        // Legacy parity: compare the full header value (no trimming/coercion).
        let auth = header_value(req, "authorization").unwrap_or("");
        let expected_header = format!("Bearer {expected}");
        if !constant_time_eq(auth, expected_header.as_str()) {
            return Some(self.error_response(req, 401, "Unauthorized"));
        }
        None
    }

    async fn fetch_jwks(&self, url: &str, force: bool) -> Result<Arc<JwkSet>, ()> {
        if !force {
            let cached = self
                .jwks_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(entry) = cached {
                if entry.fetched_at.elapsed() < JWKS_CACHE_TTL {
                    return Ok(entry.jwks);
                }
            }
        }

        let fut = Box::pin(self.jwks_http_client.get(url));
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

    fn parse_bearer_token(req: &Http1Request) -> Result<&str, ()> {
        let Some(auth) = header_value(req, "authorization") else {
            return Err(());
        };
        let auth = auth.trim();
        let Some(token) = auth.strip_prefix("Bearer ").map(str::trim) else {
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

    async fn jwt_decoding_key(&self, kid: Option<&str>) -> Result<DecodingKey, ()> {
        if let Some(jwks_url) = self
            .config
            .http_jwt_jwks_url
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            // Cache JWKS fetches; if kid is missing from the cached set, force refresh once.
            let jwks = self.fetch_jwks(jwks_url, false).await?;
            let jwk = if let Some(kid) = kid {
                if let Some(jwk) = jwks.find(kid).cloned() {
                    jwk
                } else {
                    let jwks = self.fetch_jwks(jwks_url, true).await?;
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

    async fn decode_jwt(&self, req: &Http1Request) -> Result<JwtContext, ()> {
        let token = Self::parse_bearer_token(req)?;
        let algorithms = self.jwt_algorithms();
        let header = jsonwebtoken::decode_header(token).map_err(|_| ())?;
        let key = self.jwt_decoding_key(header.kid.as_deref()).await?;
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
            Budget::with_deadline_secs(self.request_timeout_secs)
        };
        let cx = Cx::for_request_with_budget(budget);

        let redis = self.rate_limit_redis_client(&cx).await;
        let has_redis = redis.is_some();

        if let Some(redis) = redis {
            if let Ok(allowed) =
                consume_rate_limit_redis(&cx, &redis, key, per_minute, burst, now).await
            {
                return allowed;
            }
            // Legacy parity: if Redis is configured, periodic cleanup is disabled even when
            // a specific Redis call fails and we fall back to memory.
        }

        self.rate_limiter
            .allow_memory(key, per_minute, burst, now, !has_redis)
    }

    async fn check_rbac_and_rate_limit(
        &self,
        req: &Http1Request,
        json_rpc: &JsonRpcRequest,
    ) -> Option<Http1Response> {
        let (kind, tool_name) = classify_request(json_rpc);
        let is_local_ok = self.allow_local_unauthenticated(req);

        let (roles, jwt_sub) = if self.config.http_jwt_enabled {
            match self.decode_jwt(req).await {
                Ok(ctx) => (ctx.roles, ctx.sub),
                Err(()) => return Some(self.error_response(req, 401, "Unauthorized")),
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
                .any(|r| self.config.http_rbac_writer_roles.contains(r))
                || roles.is_empty();

            if kind == RequestKind::Resources {
                // Legacy python allows resources regardless of role membership.
            } else if kind == RequestKind::Tools {
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
        if self.config.http_rate_limit_enabled {
            let (rpm, burst) = rate_limits_for(&self.config, kind);
            let identity = rate_limit_identity(req, jwt_sub.as_deref());
            let endpoint = tool_name.as_deref().unwrap_or("*");
            let key = format!("{kind}:{endpoint}:{identity}");

            if !self.consume_rate_limit(&key, rpm, burst).await {
                return Some(self.error_response(req, 429, "Rate limit exceeded"));
            }
        }

        None
    }

    fn dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let id = request.id.clone();
        match self.dispatch_inner(request) {
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
            Budget::with_deadline_secs(self.request_timeout_secs)
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
                let params: fastmcp_protocol::CallToolParams = parse_params(request.params)?;
                let tool_name = params.name.clone();
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

                let tool_call_console_enabled = self.config.log_rich_enabled
                    && self.config.tools_log_enabled
                    && self.config.log_tool_calls_enabled
                    && std::io::stdout().is_terminal();

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
                    if !dashboard_write_log(&panel) {
                        ftui_runtime::ftui_println!("{panel}");
                    }
                    Some(Instant::now())
                } else {
                    None
                };

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
                            if !dashboard_write_log(&panel) {
                                ftui_runtime::ftui_println!("{panel}");
                            }
                        }
                        return Err(e);
                    }
                };
                let mut value = serde_json::to_value(out).map_err(McpError::from)?;

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
                    if !dashboard_write_log(&panel) {
                        ftui_runtime::ftui_println!("{panel}");
                    }
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
        if let Some((key, value)) = pair.split_once('=') {
            if key == "format" {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn extract_arg_str(arguments: Option<&serde_json::Value>, keys: &[&str]) -> Option<String> {
    let args = arguments?.as_object()?;
    for key in keys {
        if let Some(value) = args.get(*key) {
            if let Some(s) = value.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn normalize_project_value(value: String) -> String {
    if value.starts_with('/') {
        mcp_agent_mail_db::queries::generate_slug(&value)
    } else {
        value
    }
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
        {
            if let Ok(envelope_json) = serde_json::to_string(&envelope) {
                block["text"] = serde_json::Value::String(envelope_json);
            }
        }
    }
}

fn map_asupersync_err(err: &asupersync::Error) -> std::io::Error {
    std::io::Error::other(format!("asupersync error: {err}"))
}

fn readiness_check(config: &mcp_agent_mail_core::Config) -> Result<(), String> {
    let pool_size = config
        .database_pool_size
        .unwrap_or(mcp_agent_mail_db::pool::DEFAULT_POOL_SIZE);
    let max_overflow = config
        .database_max_overflow
        .unwrap_or(mcp_agent_mail_db::pool::DEFAULT_MAX_OVERFLOW);
    let pool_timeout_ms = config
        .database_pool_timeout
        .map_or(mcp_agent_mail_db::pool::DEFAULT_POOL_TIMEOUT_MS, |v| {
            v.saturating_mul(1000)
        });
    let db_config = DbPoolConfig {
        database_url: config.database_url.clone(),
        min_connections: pool_size,
        max_connections: pool_size + max_overflow,
        acquire_timeout_ms: pool_timeout_ms,
        max_lifetime_ms: mcp_agent_mail_db::pool::DEFAULT_POOL_RECYCLE_MS,
        run_migrations: true,
    };
    let pool = create_pool(&db_config).map_err(|e| e.to_string())?;
    let cx = Cx::for_testing();
    let conn = match block_on(pool.acquire(&cx)) {
        asupersync::Outcome::Ok(c) => c,
        asupersync::Outcome::Err(e) => return Err(e.to_string()),
        asupersync::Outcome::Cancelled(_) => return Err("readiness cancelled".to_string()),
        asupersync::Outcome::Panicked(p) => {
            return Err(format!("readiness panic: {}", p.message()));
        }
    };
    conn.query_sync("SELECT 1", &[])
        .map_err(|e| e.to_string())?;
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
        if let Some(params) = req.params.as_ref() {
            if let Some(name) = params.get("name").and_then(|v| v.as_str()) {
                return (RequestKind::Tools, Some(name.to_string()));
            }
        }
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
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4().is_some_and(|v4| v4.is_loopback()),
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
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
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
        HttpError::InvalidContentType(_) | HttpError::JsonError(_) | HttpError::CodecError(_) => {
            HttpStatus::BAD_REQUEST
        }
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
    static REDIS_RATE_LIMIT_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .to_path_buf()
    }

    fn safe_component(value: &str) -> String {
        let mut out = value.trim().to_string();
        for ch in ['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '] {
            out = out.replace(ch, "_");
        }
        if out.is_empty() {
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
        HttpState::new(router, server_info, server_capabilities, config)
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
            ..Default::default()
        };
        let state = build_state(config);

        let req = make_request(Http1Method::Get, "/api/", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 401);
        let body: serde_json::Value =
            serde_json::from_slice(&resp.body).expect("bearer auth response json");
        assert_eq!(body["detail"], "Unauthorized");

        // `/health/*` must bypass bearer auth.
        let req_health = make_request(Http1Method::Get, "/health/liveness", &[]);
        let resp_health = block_on(state.handle(req_health));
        assert_eq!(resp_health.status, 200);
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
        assert!(state.dispatch(notification).is_none());
    }

    #[test]
    fn dispatch_returns_error_for_unknown_method() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let request = JsonRpcRequest::new("nonexistent/method", None, 1_i64);
        let resp = state.dispatch(request);
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
    fn http_post_base_path_without_slash_matches_base_with_slash() {
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
        assert_eq!(
            resp_base.body, resp_slash.body,
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
    fn health_readiness_returns_ready_json() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Get, "/health/readiness", &[]);
        let resp = block_on(state.handle(req));
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body, serde_json::json!({"status": "ready"}));
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
    fn health_readiness_rejects_post_with_405() {
        let config = mcp_agent_mail_core::Config::default();
        let state = build_state(config);
        let req = make_request(Http1Method::Post, "/health/readiness", &[]);
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
    fn health_readiness_bypasses_bearer_auth() {
        let config = mcp_agent_mail_core::Config {
            http_bearer_token: Some("secret-token".to_string()),
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
        assert!(text.contains("RedFox"));
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
    fn dashboard_changed_rows_highlight_no_panic() {
        let snap = make_test_snapshot(make_agents(0));
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 20, &mut pool);
        // All rows changed
        render_dashboard_frame(&mut frame, Rect::new(0, 0, 120, 20), &snap, 0.5, 0b11_1111);
    }
}
