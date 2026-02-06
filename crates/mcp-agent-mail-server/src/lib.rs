#![forbid(unsafe_code)]

use asupersync::http::h1::HttpClient;
use asupersync::http::h1::listener::Http1Listener;
use asupersync::http::h1::types::{
    Method as Http1Method, Request as Http1Request, Response as Http1Response, default_reason,
};
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{timeout, wall_now};
use asupersync::{Budget, Cx};
use fastmcp::prelude::*;
use fastmcp_core::{McpError, McpErrorCode, SessionState, block_on};
use fastmcp_protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use fastmcp_server::Session;
use fastmcp_transport::http::{
    HttpHandlerConfig, HttpMethod as McpHttpMethod, HttpRequest, HttpRequestHandler, HttpResponse,
};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation};
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn add_tool<T: fastmcp::ToolHandler + 'static>(
    server: fastmcp_server::ServerBuilder,
    config: &mcp_agent_mail_core::Config,
    tool_name: &str,
    cluster: &str,
    tool: T,
) -> fastmcp_server::ServerBuilder {
    if config.should_expose_tool(tool_name, cluster) {
        server.tool(tool)
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
    mcp_agent_mail_storage::wbq_start();
    build_server(config).run_stdio();
    // run_stdio() does not return; WBQ drain thread exits with the process.
}

pub fn run_http(config: &mcp_agent_mail_core::Config) -> std::io::Result<()> {
    mcp_agent_mail_storage::wbq_start();

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

    mcp_agent_mail_storage::wbq_shutdown();
    result
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

const JWKS_CACHE_TTL: Duration = Duration::from_secs(60);
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

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

struct HttpState {
    router: Arc<fastmcp_server::Router>,
    server_info: fastmcp_protocol::ServerInfo,
    server_capabilities: fastmcp_protocol::ServerCapabilities,
    config: mcp_agent_mail_core::Config,
    rate_limiter: Arc<RateLimiter>,
    request_timeout_secs: u64,
    handler: Arc<HttpRequestHandler>,
    jwks_http_client: HttpClient,
    jwks_cache: Mutex<Option<JwksCacheEntry>>,
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
        Self {
            router,
            server_info,
            server_capabilities,
            config,
            rate_limiter: Arc::new(RateLimiter::new()),
            request_timeout_secs: 30,
            handler,
            jwks_http_client: HttpClient::new(),
            jwks_cache: Mutex::new(None),
        }
    }

    #[allow(clippy::unused_async)] // Required for Http1Listener interface
    async fn handle(&self, req: Http1Request) -> Http1Response {
        if !self.config.http_request_log_enabled {
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
        self.emit_http_request_log(method.as_str(), &path, resp.status, dur_ms, &client_ip);
        resp
    }

    async fn handle_inner(&self, req: Http1Request) -> Http1Response {
        if let Some(resp) = self.handle_options(&req) {
            return resp;
        }

        let (path, _query) = split_path_query(&req.uri);
        if let Some(resp) = self.handle_special_routes(&req, &path) {
            return resp;
        }
        if !self.path_allowed(&path) {
            return self.error_response(&req, 404, "Not Found");
        }

        if !matches!(req.method, Http1Method::Post) {
            return self.error_response(&req, 405, "Method Not Allowed");
        }

        let http_req = to_mcp_http_request(&req, &path);
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

        if let Some(resp) = self.check_bearer_auth(&req) {
            return resp;
        }

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
            let use_ansi = std::io::stdout().is_terminal();
            if let Some(panel) = render_http_request_panel(
                100,
                method,
                path,
                status,
                duration_ms,
                client_ip,
                use_ansi,
            ) {
                ftui_runtime::ftui_println!("{panel}");
            } else {
                ftui_runtime::ftui_println!(
                    "{}",
                    http_request_log_fallback_line(method, path, status, duration_ms, client_ip)
                );
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
            return Some(self.error_response(req, 404, "Mail UI not implemented"));
        }

        None
    }

    /// Check if `path` is under the configured MCP base path.
    ///
    /// Legacy parity: FastAPI `mount(base_no_slash, app)` + `mount(base_with_slash, app)`
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

        let Some(auth) = header_value(req, "authorization") else {
            return Some(self.error_response(req, 401, "Unauthorized"));
        };

        let expected_header = format!("Bearer {expected}");
        if !constant_time_eq(auth.trim(), expected_header.as_str()) {
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

        // Rate limiting (memory backend only)
        if self.config.http_rate_limit_enabled {
            let (rpm, burst) = rate_limits_for(&self.config, kind);
            let identity = rate_limit_identity(req, jwt_sub.as_deref());
            let endpoint = tool_name.as_deref().unwrap_or("*");
            let key = format!("{kind}:{endpoint}:{identity}");

            if !self.rate_limiter.allow(&key, rpm, burst) {
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

                if let Some((tracker, _guard)) = tracker_state {
                    if self.config.tools_log_enabled {
                        log_tool_query_stats(
                            &tool_name,
                            project_hint.as_deref(),
                            agent_hint.as_deref(),
                            &tracker,
                        );
                    }
                }

                let out = result?;
                let mut value = serde_json::to_value(out).map_err(McpError::from)?;
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

struct RateLimiter {
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
    last_cleanup: Mutex<Instant>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            last_cleanup: Mutex::new(Instant::now()),
        }
    }

    fn allow(&self, key: &str, per_minute: u32, burst: u32) -> bool {
        if per_minute == 0 {
            return true;
        }
        let rate_per_sec = f64::from(per_minute) / 60.0;
        let burst = f64::from(burst.max(1));
        let now = Instant::now();

        self.cleanup(now);

        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = buckets.entry(key.to_string()).or_insert((burst, now));
        let elapsed = now.duration_since(entry.1).as_secs_f64();
        entry.0 = (entry.0 + elapsed * rate_per_sec).min(burst);
        entry.1 = now;

        let allowed = if entry.0 < 1.0 {
            false
        } else {
            entry.0 -= 1.0;
            true
        };

        // Release the lock before returning.
        drop(buckets);
        allowed
    }

    fn cleanup(&self, now: Instant) {
        {
            let mut last = self
                .last_cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if now.duration_since(*last) < Duration::from_secs(60) {
                return;
            }
            *last = now;
        }

        let Some(cutoff) = now.checked_sub(Duration::from_secs(3600)) else {
            // If we're running for < 1h, nothing can be older than the cutoff yet.
            return;
        };
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
        headers.insert(
            "content-type".to_string(),
            "application/json".to_string(),
        );
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
            .to_ipv4()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
    }
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
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
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

fn render_http_request_panel(
    width: usize,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
    client_ip: &str,
    use_ansi: bool,
) -> Option<String> {
    // `rich.console.Console(width=100)` - keep deterministic, but avoid panicking on tiny widths.
    if width < 20 {
        return None;
    }
    let inner_width = width.saturating_sub(2);

    let status_str = status.to_string();
    let dur_str = format!("{duration_ms}ms");

    // Title formatting mirrors legacy spacing: "METHOD␠␠PATH␠␠STATUS␠␠DUR".
    let reserved = method.len() + status_str.len() + dur_str.len() + 8; // 6 spaces between + 2 edge spaces
    let max_path = inner_width.saturating_sub(reserved).max(1);
    let path = if path.len() <= max_path {
        path.to_string()
    } else if max_path <= 3 {
        path[..max_path].to_string()
    } else {
        format!("{}...", &path[..(max_path - 3)])
    };

    let title_plain = format!("{method}  {path}  {status_str}  {dur_str}");

    let title_styled = if use_ansi {
        const RESET: &str = "\x1b[0m";
        const BOLD_BLUE: &str = "\x1b[1;34m";
        const BOLD_WHITE: &str = "\x1b[1;37m";
        const BOLD_GREEN: &str = "\x1b[1;32m";
        const BOLD_RED: &str = "\x1b[1;31m";
        const BOLD_YELLOW: &str = "\x1b[1;33m";

        let status_color = if (200..400).contains(&status) {
            BOLD_GREEN
        } else {
            BOLD_RED
        };

        format!(
            "{BOLD_BLUE}{method}{RESET}  {BOLD_WHITE}{path}{RESET}  {status_color}{status_str}{RESET}  {BOLD_YELLOW}{dur_str}{RESET}",
        )
    } else {
        title_plain.clone()
    };

    let mut top = format!(" {title_styled} ");
    let top_plain_len = title_plain.len().saturating_add(2);
    if top_plain_len > inner_width {
        return None;
    }
    top.push_str(&"-".repeat(inner_width.saturating_sub(top_plain_len)));

    // Body: "client: <ip>"
    let mut body_plain = format!(" client: {client_ip}");
    if body_plain.len() > inner_width {
        // Truncate client_ip to fit.
        let reserved = " client: ".len();
        let max_ip = inner_width.saturating_sub(reserved).max(1);
        let ip = if client_ip.len() <= max_ip {
            client_ip.to_string()
        } else if max_ip <= 3 {
            client_ip[..max_ip].to_string()
        } else {
            format!("{}...", &client_ip[..(max_ip - 3)])
        };
        body_plain = format!(" client: {ip}");
    }

    let body_plain_len = body_plain.len();

    let body_styled = if use_ansi {
        const RESET: &str = "\x1b[0m";
        const CYAN: &str = "\x1b[36m";
        const WHITE: &str = "\x1b[37m";

        let prefix = " client: ";
        let ip = body_plain.strip_prefix(prefix).unwrap_or(client_ip);
        format!(" {CYAN}client: {RESET}{WHITE}{ip}{RESET}")
    } else {
        body_plain
    };

    let mut body = body_styled;
    if body_plain_len > inner_width {
        return None;
    }
    body.push_str(&" ".repeat(inner_width.saturating_sub(body_plain_len)));

    let bottom = "-".repeat(inner_width);

    Some(format!("+{top}+\n|{body}|\n+{bottom}+"))
}

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
    use ftui_runtime::stdio_capture::StdioCapture;
    use std::sync::Mutex;

    static STDIO_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

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
        let req = make_request_with_peer_addr(
            Http1Method::Post,
            "/api",
            &[],
            Some(ipv6_loopback),
        );
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
        assert!(resp.is_some(), "unknown method should still return a response");
        let resp = resp.unwrap();
        assert!(
            resp.error.is_some(),
            "unknown method must return an error response"
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
        assert_eq!(resp.status, 401);
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
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"secret"),
        )
        .expect("encode token");
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
    fn jwt_roles_enforced_for_tools() {
        let config = mcp_agent_mail_core::Config {
            http_jwt_enabled: true,
            http_jwt_secret: Some("secret".to_string()),
            http_rbac_enabled: true,
            ..Default::default()
        };
        let state = build_state(config);
        let claims = serde_json::json!({ "sub": "user-123", "role": "reader" });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"secret"),
        )
        .expect("encode token");
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
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"secret"),
        )
        .expect("encode token");
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
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

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

        // Run a tiny blocking HTTP server on a dedicated OS thread. Using a separate
        // thread avoids in-process deadlocks when both client and server are driven by
        // a single-threaded async runtime.
        std::thread::scope(|s| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind jwks listener");
            listener.set_nonblocking(true).expect("set_nonblocking");
            let addr = listener.local_addr().expect("listener addr");
            let jwks_body = jwks_bytes.clone();
            let accepted = Arc::new(AtomicBool::new(false));
            let accepted2 = Arc::clone(&accepted);

            s.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match listener.accept() {
                        Ok((mut stream, _peer)) => {
                            accepted2.store(true, Ordering::SeqCst);
                            let status = "200 OK";
                            let body: &[u8] = jwks_body.as_slice();

                            let header = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(body);
                            let _ = stream.flush();
                            return;
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
                let jwks = state.fetch_jwks(&jwks_url, true).await;
                assert!(
                    jwks.is_ok(),
                    "fetch_jwks failed: {jwks:?}; accepted={}",
                    accepted.load(Ordering::SeqCst)
                );

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
            out.contains("+ GET  /health/liveness  200 "),
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
            out.contains("+ GET  /health/liveness  200 "),
            "missing panel title: {out:?}"
        );
        assert!(
            out.contains("| client: 127.0.0.1"),
            "missing panel body: {out:?}"
        );
    }

    #[test]
    fn http_request_panel_tiny_width_returns_none_and_fallback_is_exact() {
        assert!(render_http_request_panel(0, "GET", "/", 200, 1, "x", false).is_none());
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
}
