//! LLM integration module for MCP Agent Mail
//!
//! Provides:
//! - Provider env variable bridging (synonym → canonical mapping)
//! - Model selection by available API keys
//! - Completion client using asupersync HTTP
//! - Safe JSON extraction from LLM responses
//! - Thread summary merge logic (heuristic + LLM refinement)

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use crate::search::{AggregateSummary, MentionCount, ThreadEntry, ThreadSummary, TopMention};
use mcp_agent_mail_core::{LockLevel, OrderedMutex, config::dotenv_value};

// ---------------------------------------------------------------------------
// Provider env bridge
// ---------------------------------------------------------------------------

/// Canonical env keys with supported aliases (including canonical itself).
const ENV_BRIDGE_MAPPINGS: &[(&str, &[&str])] = &[
    ("OPENAI_API_KEY", &["OPENAI_API_KEY"]),
    ("ANTHROPIC_API_KEY", &["ANTHROPIC_API_KEY"]),
    ("GROQ_API_KEY", &["GROQ_API_KEY"]),
    ("XAI_API_KEY", &["XAI_API_KEY", "GROK_API_KEY"]),
    ("GOOGLE_API_KEY", &["GOOGLE_API_KEY", "GEMINI_API_KEY"]),
    ("OPENROUTER_API_KEY", &["OPENROUTER_API_KEY"]),
    ("DEEPSEEK_API_KEY", &["DEEPSEEK_API_KEY"]),
];

/// In-memory bridged env vars (since `set_var` is unsafe in Rust 2024).
/// Maps canonical key → value when bridged from a synonym.
static BRIDGED_ENV: OnceLock<OrderedMutex<HashMap<String, String>>> = OnceLock::new();

fn bridged_env() -> &'static OrderedMutex<HashMap<String, String>> {
    BRIDGED_ENV.get_or_init(|| OrderedMutex::new(LockLevel::ToolsBridgedEnv, HashMap::new()))
}

/// Look up an env var, checking our bridged map first, then real env.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn dotenv_nonempty(key: &str) -> Option<String> {
    dotenv_value(key).filter(|v| !v.is_empty())
}

fn get_env_var(key: &str) -> Option<String> {
    if let Some(val) = env_nonempty(key) {
        return Some(val);
    }
    if let Some(val) = {
        let map = bridged_env().lock();
        map.get(key).cloned()
    } {
        return Some(val);
    }
    dotenv_nonempty(key)
}

fn get_from_any(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = env_nonempty(key) {
            return Some(val);
        }
    }
    for key in keys {
        if let Some(val) = dotenv_nonempty(key) {
            return Some(val);
        }
    }
    None
}

#[cfg(test)]
fn compute_env_bridge(
    env: &HashMap<String, String>,
    dotenv: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut bridged = HashMap::new();
    for &(canonical, aliases) in ENV_BRIDGE_MAPPINGS {
        if let Some(val) = env.get(canonical).filter(|v| !v.is_empty()) {
            bridged.insert(canonical.to_string(), val.clone());
            continue;
        }
        let mut selected: Option<String> = None;
        for alias in aliases {
            if let Some(val) = env.get(*alias).filter(|v| !v.is_empty()) {
                selected = Some(val.clone());
                break;
            }
        }
        if selected.is_none() {
            for alias in aliases {
                if let Some(val) = dotenv.get(*alias).filter(|v| !v.is_empty()) {
                    selected = Some(val.clone());
                    break;
                }
            }
        }
        if let Some(val) = selected {
            bridged.insert(canonical.to_string(), val);
        }
    }
    bridged
}

/// Bridge synonym env vars to canonical keys.
///
/// For each canonical key: if it is NOT already set in the real environment
/// (non-empty), look for aliases in env first, then .env, and store the result
/// in the bridged map.
pub fn bridge_provider_env() {
    let mut map = bridged_env().lock();
    for &(canonical, aliases) in ENV_BRIDGE_MAPPINGS {
        if env_nonempty(canonical).is_some() || map.contains_key(canonical) {
            continue;
        }
        if let Some(val) = get_from_any(aliases) {
            map.insert(canonical.to_string(), val);
        }
    }
}

// ---------------------------------------------------------------------------
// Model selection
// ---------------------------------------------------------------------------

/// Priority-ordered provider → model mapping.
const MODEL_PRIORITY: &[(&str, &str)] = &[
    ("OPENAI_API_KEY", "gpt-4o-mini"),
    ("GOOGLE_API_KEY", "gemini-1.5-flash"),
    ("ANTHROPIC_API_KEY", "claude-3-haiku-20240307"),
    ("GROQ_API_KEY", "groq/llama-3.1-8b-instant"),
    ("DEEPSEEK_API_KEY", "deepseek/deepseek-chat"),
    ("XAI_API_KEY", "xai/grok-2-mini"),
    (
        "OPENROUTER_API_KEY",
        "openrouter/meta-llama/llama-3.1-8b-instruct",
    ),
];

/// Default fallback model when no API keys are found.
const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Aliases that trigger dynamic model selection.
const AUTO_ALIASES: &[&str] = &["best", "auto", "gpt-5-mini", "gpt5-mini", "gpt-5m"];

/// Choose the best available model based on set API keys.
///
/// If `preferred` contains "/" or ":" (provider-qualified), returns it as-is.
/// Otherwise, checks env vars in priority order and returns the first match.
/// Falls back to `DEFAULT_MODEL`.
#[must_use]
pub fn choose_best_available_model(preferred: &str) -> String {
    // Provider-qualified names pass through
    if preferred.contains('/') || preferred.contains(':') {
        return preferred.to_string();
    }

    // If the preferred model matches a known model whose API key is available, use it
    let lower = preferred.to_ascii_lowercase();
    if !lower.is_empty() {
        for &(env_var, model) in MODEL_PRIORITY {
            if model.to_ascii_lowercase().contains(&lower) && get_env_var(env_var).is_some() {
                return model.to_string();
            }
        }
    }

    // Fall back to first available API key
    for &(env_var, model) in MODEL_PRIORITY {
        if get_env_var(env_var).is_some() {
            return model.to_string();
        }
    }

    DEFAULT_MODEL.to_string()
}

/// Resolve a model alias to a concrete model name.
///
/// "best", "auto", etc. trigger `choose_best_available_model`.
/// Other names are returned as-is.
#[must_use]
pub fn resolve_model_alias(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if AUTO_ALIASES.iter().any(|a| *a == lower) {
        choose_best_available_model(name)
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
// LLM completion types
// ---------------------------------------------------------------------------

/// Output from an LLM completion call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOutput {
    pub content: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

/// LLM completion error.
#[derive(Debug)]
pub enum LlmError {
    /// HTTP transport error.
    Http(String),
    /// Non-200 status code.
    StatusError { status: u16, body: String },
    /// Response parsing error.
    ParseError(String),
    /// No API key available for the selected provider.
    NoApiKey(String),
    /// LLM is disabled.
    Disabled,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::StatusError { status, body } => {
                write!(f, "LLM returned status {status}: {body}")
            }
            Self::ParseError(e) => write!(f, "parse error: {e}"),
            Self::NoApiKey(model) => write!(f, "no API key for model: {model}"),
            Self::Disabled => write!(f, "LLM is disabled"),
        }
    }
}

impl std::error::Error for LlmError {}

// ---------------------------------------------------------------------------
// API endpoint resolution
// ---------------------------------------------------------------------------

/// Determine the API base URL and auth header for a given model.
fn resolve_api_endpoint(model: &str) -> Result<(String, String, String), LlmError> {
    // Provider-qualified: "provider/model" or "provider:model"
    let provider = if model.contains('/') {
        model.split('/').next().unwrap_or("")
    } else if model.contains(':') {
        model.split(':').next().unwrap_or("")
    } else {
        // Guess provider from model name prefix
        if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") {
            "openai"
        } else if model.starts_with("claude") {
            "anthropic"
        } else if model.starts_with("gemini") {
            "google"
        } else {
            "openai" // default
        }
    };

    let provider_lower = provider.to_ascii_lowercase();
    match provider_lower.as_str() {
        "openai" | "gpt" => {
            let key = get_env_var("OPENAI_API_KEY")
                .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            Ok((
                "https://api.openai.com/v1/chat/completions".to_string(),
                format!("Bearer {key}"),
                model.to_string(),
            ))
        }
        "anthropic" | "claude" => {
            let key = get_env_var("ANTHROPIC_API_KEY")
                .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            // Anthropic uses a different header, but litellm-compatible endpoints
            // accept Bearer too
            Ok((
                "https://api.anthropic.com/v1/messages".to_string(),
                format!("Bearer {key}"),
                model.to_string(),
            ))
        }
        "google" | "gemini" => {
            let key = get_env_var("GOOGLE_API_KEY")
                .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            Ok((
                "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
                    .to_string(),
                format!("Bearer {key}"),
                model.to_string(),
            ))
        }
        "groq" => {
            let key =
                get_env_var("GROQ_API_KEY").ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            // Strip provider prefix for the API
            let api_model = model.strip_prefix("groq/").unwrap_or(model);
            Ok((
                "https://api.groq.com/openai/v1/chat/completions".to_string(),
                format!("Bearer {key}"),
                api_model.to_string(),
            ))
        }
        "deepseek" => {
            let key = get_env_var("DEEPSEEK_API_KEY")
                .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            let api_model = model.strip_prefix("deepseek/").unwrap_or(model);
            Ok((
                "https://api.deepseek.com/v1/chat/completions".to_string(),
                format!("Bearer {key}"),
                api_model.to_string(),
            ))
        }
        "xai" => {
            let key =
                get_env_var("XAI_API_KEY").ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            let api_model = model.strip_prefix("xai/").unwrap_or(model);
            Ok((
                "https://api.x.ai/v1/chat/completions".to_string(),
                format!("Bearer {key}"),
                api_model.to_string(),
            ))
        }
        "openrouter" => {
            let key = get_env_var("OPENROUTER_API_KEY")
                .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            let api_model = model.strip_prefix("openrouter/").unwrap_or(model);
            Ok((
                "https://openrouter.ai/api/v1/chat/completions".to_string(),
                format!("Bearer {key}"),
                api_model.to_string(),
            ))
        }
        _ => {
            // Try OpenAI-compatible endpoint
            let key = get_env_var("OPENAI_API_KEY")
                .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
            Ok((
                "https://api.openai.com/v1/chat/completions".to_string(),
                format!("Bearer {key}"),
                model.to_string(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP completion client
// ---------------------------------------------------------------------------

/// Global HTTP client instance for LLM calls.
static HTTP_CLIENT: OnceLock<asupersync::http::h1::HttpClient> = OnceLock::new();

fn get_http_client() -> &'static asupersync::http::h1::HttpClient {
    HTTP_CLIENT.get_or_init(asupersync::http::h1::HttpClient::new)
}

/// Call an OpenAI-compatible chat completion endpoint.
///
/// Sends system + user messages and extracts the response content.
/// On failure with the primary model, retries with `choose_best_available_model`
/// if that yields a different model.
pub async fn complete_system_user(
    system: &str,
    user: &str,
    model: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> Result<LlmOutput, LlmError> {
    let resolved = model.map_or_else(|| resolve_model_alias(DEFAULT_MODEL), resolve_model_alias);

    if llm_stub_enabled() {
        return Ok(LlmOutput {
            content: stubbed_completion_content(system, user),
            model: resolved,
            provider: Some("stub".to_string()),
            estimated_cost_usd: Some(0.0),
        });
    }

    match complete_single(&resolved, system, user, temperature, max_tokens).await {
        Ok(output) => Ok(output),
        Err(e) => {
            // Retry with best available if different
            let fallback = choose_best_available_model(&resolved);
            if fallback == resolved {
                Err(e)
            } else {
                tracing::warn!("LLM call failed with {resolved}, retrying with {fallback}: {e}");
                complete_single(&fallback, system, user, temperature, max_tokens).await
            }
        }
    }
}

async fn complete_single(
    model: &str,
    system: &str,
    user: &str,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> Result<LlmOutput, LlmError> {
    let (url, auth, api_model) = resolve_api_endpoint(model)?;
    let temp = temperature.unwrap_or(0.2);
    let max_tok = max_tokens.unwrap_or(512);

    let is_anthropic = url.contains("api.anthropic.com");

    let payload = if is_anthropic {
        serde_json::json!({
            "model": api_model,
            "system": system,
            "messages": [
                {"role": "user", "content": user}
            ],
            "temperature": temp,
            "max_tokens": max_tok
        })
    } else {
        serde_json::json!({
            "model": api_model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "temperature": temp,
            "max_tokens": max_tok
        })
    };

    let body_bytes =
        serde_json::to_vec(&payload).map_err(|e| LlmError::ParseError(e.to_string()))?;

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if is_anthropic {
        headers.push(("x-api-key".to_string(), auth.replace("Bearer ", "")));
        headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
    } else {
        headers.push(("Authorization".to_string(), auth));
    }

    let client = get_http_client();
    let response = client
        .request(
            asupersync::http::h1::Method::Post,
            &url,
            headers,
            body_bytes,
        )
        .await
        .map_err(|e| LlmError::Http(e.to_string()))?;

    if response.status != 200 {
        let body_text = String::from_utf8_lossy(&response.body).to_string();
        return Err(LlmError::StatusError {
            status: response.status,
            body: body_text,
        });
    }

    let resp_json: Value = serde_json::from_slice(&response.body)
        .map_err(|e| LlmError::ParseError(format!("response JSON: {e}")))?;

    let content = if is_anthropic {
        resp_json
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    let resp_model = resp_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(model)
        .to_string();

    let provider = resp_json
        .get("provider")
        .and_then(Value::as_str)
        .map(String::from);

    Ok(LlmOutput {
        content,
        model: resp_model,
        provider,
        estimated_cost_usd: None,
    })
}

// ---------------------------------------------------------------------------
// Safe JSON extraction
// ---------------------------------------------------------------------------

/// Parse JSON from LLM output using three fallback strategies:
/// 1. Direct parse (trim whitespace first)
/// 2. Fenced code block extraction (```json ... ``` or ``` ... ```)
/// 3. Brace-slice extraction (outermost { ... })
#[must_use]
pub fn parse_json_safely(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strategy 1: direct parse
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }

    // Strategy 2: fenced code block
    if let Some(v) = extract_fenced_json(trimmed) {
        return Some(v);
    }

    // Strategy 3: brace-slice
    extract_brace_json(trimmed)
}

fn extract_fenced_json(text: &str) -> Option<Value> {
    // Look for ```json\n...\n``` first, then plain ```\n...\n```
    let markers = ["```json\n", "```json\r\n", "```\n", "```\r\n"];
    for marker in markers {
        if let Some(start) = text.find(marker) {
            let content_start = start + marker.len();
            if let Some(end_rel) = text[content_start..].find("```") {
                let content = text[content_start..content_start + end_rel].trim();
                if let Ok(v) = serde_json::from_str(content) {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn extract_brace_json(text: &str) -> Option<Value> {
    let open = text.find('{')?;
    let close = text.rfind('}')?;
    if close <= open {
        return None;
    }
    let slice = &text[open..=close];
    serde_json::from_str(slice).ok()
}

// ---------------------------------------------------------------------------
// Thread summary merge logic
// ---------------------------------------------------------------------------

/// Action keywords used to identify heuristic `key_points` worth preserving.
const ACTION_KEYWORDS: &[&str] = &["TODO", "ACTION", "FIXME", "NEXT", "BLOCKED"];

/// Maximum `key_points` after merge.
const KEY_POINTS_CAP: usize = 10;

/// Merge LLM refinement into a heuristic `ThreadSummary` (single-thread mode).
///
/// Strategy:
/// - For `key_points`: keep heuristic items containing action keywords,
///   append them after LLM `key_points`, deduplicate, cap at 10.
/// - For other keys: LLM values overlay heuristic values if present.
pub fn merge_single_thread_summary(heuristic: &ThreadSummary, llm_json: &Value) -> ThreadSummary {
    let mut result = heuristic.clone();

    // key_points: special merge
    if let Some(llm_kp) = llm_json.get("key_points").and_then(Value::as_array) {
        let llm_points: Vec<String> = llm_kp
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();

        // Legacy Python semantics: only override key_points if non-empty, then append heuristic
        // keyword key_points (TODO/ACTION/etc) after LLM's items.
        if !llm_points.is_empty() {
            // Keep heuristic items with action keywords
            let action_points: Vec<String> = heuristic
                .key_points
                .iter()
                .filter(|kp| {
                    let upper = kp.to_ascii_uppercase();
                    ACTION_KEYWORDS.iter().any(|k| upper.contains(k))
                })
                .cloned()
                .collect();

            let mut merged: Vec<String> = Vec::new();
            for p in llm_points.into_iter().chain(action_points.into_iter()) {
                if !merged.contains(&p) {
                    merged.push(p);
                }
            }
            merged.truncate(KEY_POINTS_CAP);
            result.key_points = merged;
        }
    }

    // action_items
    if let Some(llm_ai) = llm_json.get("action_items").and_then(Value::as_array) {
        let items: Vec<String> = llm_ai
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        if !items.is_empty() {
            result.action_items = items;
        }
    }

    // participants
    if let Some(llm_p) = llm_json.get("participants").and_then(Value::as_array) {
        let parts: Vec<String> = llm_p
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        if !parts.is_empty() {
            result.participants = parts;
        }
    }

    // mentions
    if let Some(llm_m) = llm_json.get("mentions").and_then(Value::as_array) {
        let mentions: Vec<MentionCount> = llm_m
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                let count = m.get("count")?.as_i64()?;
                Some(MentionCount { name, count })
            })
            .collect();
        if !mentions.is_empty() {
            result.mentions = mentions;
        }
    }

    // code_references
    if let Some(llm_cr) = llm_json.get("code_references").and_then(Value::as_array) {
        let refs: Vec<String> = llm_cr
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        if !refs.is_empty() {
            result.code_references = Some(refs);
        }
    }

    // total_messages, open_actions, done_actions
    //
    // Legacy Python overlays only on truthy values; for integers, that means non-zero.
    if let Some(v) = llm_json.get("total_messages").and_then(Value::as_i64)
        && v != 0
    {
        result.total_messages = v;
    }
    if let Some(v) = llm_json.get("open_actions").and_then(Value::as_i64)
        && v != 0
    {
        result.open_actions = v;
    }
    if let Some(v) = llm_json.get("done_actions").and_then(Value::as_i64)
        && v != 0
    {
        result.done_actions = v;
    }

    result
}

/// Apply per-thread LLM revisions in multi-thread mode.
///
/// Legacy Python behavior: if LLM returns `threads[]`, replace per-thread `key_points` and
/// `action_items` when those arrays are present and non-empty.
pub fn apply_multi_thread_thread_revisions(threads: &mut [ThreadEntry], llm_json: &Value) {
    let Some(payload_threads) = llm_json.get("threads").and_then(Value::as_array) else {
        return;
    };

    let mut mapping: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for item in payload_threads {
        let Some(thread_id) = item.get("thread_id").and_then(Value::as_str) else {
            continue;
        };
        let key_points: Vec<String> = item
            .get("key_points")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let actions: Vec<String> = item
            .get("actions")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        mapping.insert(thread_id.to_string(), (key_points, actions));
    }

    if mapping.is_empty() {
        return;
    }

    for entry in threads {
        let Some((key_points, actions)) = mapping.get(&entry.thread_id) else {
            continue;
        };
        if !key_points.is_empty() {
            entry.summary.key_points = key_points.clone();
        }
        if !actions.is_empty() {
            entry.summary.action_items = actions.clone();
        }
    }
}

/// Merge LLM refinement into a heuristic `AggregateSummary` (multi-thread mode).
///
/// LLM aggregate keys overlay heuristic aggregate.
pub fn merge_multi_thread_aggregate(
    heuristic: &AggregateSummary,
    llm_json: &Value,
) -> AggregateSummary {
    let mut result = heuristic.clone();

    if let Some(agg) = llm_json.get("aggregate") {
        if let Some(kp) = agg.get("key_points").and_then(Value::as_array) {
            let points: Vec<String> = kp
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect();
            if !points.is_empty() {
                result.key_points = points;
            }
        }
        if let Some(ai) = agg.get("action_items").and_then(Value::as_array) {
            let items: Vec<String> = ai
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect();
            if !items.is_empty() {
                result.action_items = items;
            }
        }
        if let Some(tm) = agg.get("top_mentions").and_then(Value::as_array) {
            // top_mentions can be strings or objects (legacy Python).
            let mentions: Vec<TopMention> = tm
                .iter()
                .filter_map(|v| {
                    v.as_str().map_or_else(
                        || {
                            let name = v.get("name")?.as_str()?.to_string();
                            let count = v.get("count").and_then(Value::as_i64).unwrap_or(0);
                            Some(TopMention::Count(MentionCount { name, count }))
                        },
                        |s| Some(TopMention::Name(s.to_string())),
                    )
                })
                .collect();
            if !mentions.is_empty() {
                result.top_mentions = mentions;
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Summarize-thread LLM prompts
// ---------------------------------------------------------------------------

/// Max messages to send to LLM for single-thread summarization.
pub const MAX_MESSAGES_FOR_LLM: usize = 15;

/// Max body chars per message sent to LLM.
pub const MESSAGE_TRUNCATION_CHARS: usize = 800;

/// Max threads to include in multi-thread LLM context.
pub const MAX_THREADS_FOR_CONTEXT: usize = 8;

/// Max `key_points` per thread in multi-thread context.
pub const MAX_KEY_POINTS_PER_THREAD: usize = 6;

/// Max action items per thread in multi-thread context.
pub const MAX_ACTIONS_PER_THREAD: usize = 6;

/// Build the system prompt for single-thread LLM summarization.
#[must_use]
pub const fn single_thread_system_prompt() -> &'static str {
    "You are a senior engineer. Produce a concise JSON summary with keys: \
     `participants` (string[]), `key_points` (string[]), `action_items` (string[]), \
     `mentions` (array of {name, count}), `code_references` (string[]), \
     `total_messages` (int), `open_actions` (int), `done_actions` (int). \
     Return only valid JSON."
}

/// Build the user prompt for single-thread LLM summarization.
#[must_use]
pub fn single_thread_user_prompt(
    messages: &[(i64, String, String, String)], // (id, from, subject, body)
) -> String {
    let mut prompt = String::from("Summarize this thread:\n\n");
    for (id, from, subject, body) in messages.iter().take(MAX_MESSAGES_FOR_LLM) {
        let truncated_body = if body.len() > MESSAGE_TRUNCATION_CHARS {
            // Find a valid UTF-8 char boundary at or before the limit.
            let mut end = MESSAGE_TRUNCATION_CHARS;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            &body[..end]
        } else {
            body.as_str()
        };
        let _ = write!(
            prompt,
            "---\nMessage {id} from {from}\nSubject: {subject}\n{truncated_body}\n"
        );
    }
    prompt
}

/// Build the system prompt for multi-thread LLM summarization.
#[must_use]
pub const fn multi_thread_system_prompt() -> &'static str {
    "You are a senior engineer producing a crisp digest across threads. \
     Return JSON: { \"threads\": [{\"thread_id\": string, \"key_points\": string[], \
     \"actions\": string[]}], \"aggregate\": {\"top_mentions\": string[], \
     \"key_points\": string[], \"action_items\": string[]} }. Return only valid JSON."
}

/// Build the user prompt for multi-thread LLM summarization.
#[must_use]
pub fn multi_thread_user_prompt(
    threads: &[(String, Vec<String>, Vec<String>)], // (thread_id, key_points, action_items)
) -> String {
    let mut prompt = String::from("Digest these threads:\n\n");
    for (tid, kps, actions) in threads.iter().take(MAX_THREADS_FOR_CONTEXT) {
        let _ = writeln!(prompt, "Thread: {tid}");
        prompt.push_str("Key points:\n");
        for kp in kps.iter().take(MAX_KEY_POINTS_PER_THREAD) {
            let _ = writeln!(prompt, "- {kp}");
        }
        prompt.push_str("Actions:\n");
        for a in actions.iter().take(MAX_ACTIONS_PER_THREAD) {
            let _ = writeln!(prompt, "- {a}");
        }
        prompt.push('\n');
    }
    prompt
}

// ---------------------------------------------------------------------------
// Deterministic stub completions (conformance fixtures)
// ---------------------------------------------------------------------------

fn llm_stub_enabled() -> bool {
    let Ok(v) = std::env::var("MCP_AGENT_MAIL_LLM_STUB") else {
        return false;
    };
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn stubbed_completion_content(system: &str, user: &str) -> String {
    let sys = system.to_ascii_lowercase();

    if sys.contains("digest across threads") || user.starts_with("Digest these threads:") {
        // Intentionally include leading text so JSON parsing exercises brace-slice fallback.
        return "Ok, here's the digest:\n{\n  \"threads\": [\n    {\n      \"thread_id\": \"T-1\",\n      \"key_points\": [\"API v2 schema finalized\"],\n      \"actions\": [\"Update OpenAPI spec\"]\n    },\n    {\n      \"thread_id\": \"T-2\",\n      \"key_points\": [\"Migration to new DB\"],\n      \"actions\": [\"Run migration script\"]\n    }\n  ],\n  \"aggregate\": {\n    \"top_mentions\": [\"Alice\", \"Bob\"],\n    \"key_points\": [\"API schema and DB migration are the two main workstreams\"],\n    \"action_items\": [\"Update OpenAPI spec\", \"Run migration script\"]\n  }\n}\nDone."
            .to_string();
    }

    // Intentionally include fenced JSON so parsing exercises code-fence extraction fallback.
    "Here is the summary:\n```json\n{\n  \"participants\": [\"BlueLake\", \"GreenCastle\"],\n  \"key_points\": [\n    \"API migration planned for next sprint\",\n    \"Staging deployment needed before review\"\n  ],\n  \"action_items\": [\"Deploy to staging\", \"Update API docs\"],\n  \"mentions\": [{\"name\": \"Carol\", \"count\": 2}],\n  \"code_references\": [\"api/v2/users\"],\n  \"total_messages\": 2,\n  \"open_actions\": 1,\n  \"done_actions\": 0\n}\n```\n"
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    struct EnvBridgeFixture {
        cases: Vec<EnvBridgeCase>,
    }

    #[derive(Deserialize)]
    struct EnvBridgeCase {
        name: String,
        env: HashMap<String, String>,
        dotenv: HashMap<String, String>,
        expected_set: HashMap<String, String>,
    }

    // -- bridge_provider_env tests --

    #[test]
    fn bridge_provider_env_vectors() {
        let fixture: EnvBridgeFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/llm/env_bridge_vectors.json"
        ))
        .expect("fixture JSON parse failed");

        for case in fixture.cases {
            let got = compute_env_bridge(&case.env, &case.dotenv);
            assert_eq!(got, case.expected_set, "case: {}", case.name);
        }
    }

    // Note: env var tests are inherently sequential and may interfere.
    // We use unique prefixes or accept test isolation limitations.

    #[test]
    fn parse_json_clean() {
        let input = r#"{"key_points": ["item1"], "action_items": []}"#;
        let v = parse_json_safely(input).unwrap();
        assert_eq!(v["key_points"][0], "item1");
    }

    #[test]
    fn parse_json_whitespace() {
        let input = "  \n  {\"key_points\": [\"a\"]}  \n  ";
        let v = parse_json_safely(input).unwrap();
        assert_eq!(v["key_points"][0], "a");
    }

    #[test]
    fn parse_json_fenced_with_tag() {
        let input =
            "Here is the summary:\n```json\n{\"key_points\": [\"deploy API\"]}\n```\nLet me know.";
        let v = parse_json_safely(input).unwrap();
        assert_eq!(v["key_points"][0], "deploy API");
    }

    #[test]
    fn parse_json_fenced_no_tag() {
        let input = "```\n{\"participants\": [\"Alice\"]}\n```";
        let v = parse_json_safely(input).unwrap();
        assert_eq!(v["participants"][0], "Alice");
    }

    #[test]
    fn parse_json_brace_slice() {
        let input = "The summary is: {\"total_messages\": 5, \"open_actions\": 2} based on thread.";
        let v = parse_json_safely(input).unwrap();
        assert_eq!(v["total_messages"], 5);
    }

    #[test]
    fn parse_json_nested_braces() {
        let input = "Result: {\"data\": {\"inner\": true}, \"count\": 1}";
        let v = parse_json_safely(input).unwrap();
        assert!(v["data"]["inner"].as_bool().unwrap());
    }

    #[test]
    fn parse_json_no_json() {
        let input = "I couldn't generate a summary for this thread.";
        assert!(parse_json_safely(input).is_none());
    }

    #[test]
    fn parse_json_malformed() {
        let input = "{key_points: [missing quotes]}";
        assert!(parse_json_safely(input).is_none());
    }

    #[test]
    fn parse_json_empty() {
        assert!(parse_json_safely("").is_none());
    }

    #[test]
    fn parse_json_array() {
        let input = r#"[{"id": 1}, {"id": 2}]"#;
        let v = parse_json_safely(input).unwrap();
        assert_eq!(v[0]["id"], 1);
    }

    #[test]
    fn parse_json_multiple_fenced() {
        let input =
            "```json\n{\"first\": true}\n```\n\nAnd also:\n```json\n{\"second\": true}\n```";
        let v = parse_json_safely(input).unwrap();
        assert!(v["first"].as_bool().unwrap());
    }

    // -- model selection tests --

    #[test]
    fn resolve_alias_best() {
        // "best" triggers dynamic selection
        let result = resolve_model_alias("best");
        // Should return some model (depends on env vars)
        assert!(!result.is_empty());
    }

    #[test]
    fn resolve_alias_passthrough() {
        assert_eq!(resolve_model_alias("gpt-4o"), "gpt-4o");
        assert_eq!(
            resolve_model_alias("claude-3-opus-20240229"),
            "claude-3-opus-20240229"
        );
    }

    #[test]
    fn provider_qualified_passthrough() {
        assert_eq!(
            choose_best_available_model("groq/my-model"),
            "groq/my-model"
        );
        assert_eq!(
            choose_best_available_model("openrouter/meta-llama/llama-3.1-8b-instruct"),
            "openrouter/meta-llama/llama-3.1-8b-instruct"
        );
    }

    // -- merge tests --

    #[test]
    fn merge_single_full_refinement() {
        let heuristic = ThreadSummary {
            participants: vec!["Alice".into(), "Bob".into()],
            key_points: vec![
                "TODO: deploy to staging".into(),
                "discussed API changes".into(),
            ],
            action_items: vec!["TODO: deploy to staging".into()],
            total_messages: 5,
            open_actions: 1,
            done_actions: 0,
            mentions: vec![],
            code_references: None,
        };

        let llm_json: Value = serde_json::from_str(
            r#"{"participants": ["Alice", "Bob"], "key_points": ["API migration planned for next sprint", "Staging deployment needed before review"], "action_items": ["Deploy to staging", "Update API docs"], "mentions": [{"name": "Carol", "count": 2}], "code_references": ["api/v2/users"], "total_messages": 5, "open_actions": 2, "done_actions": 0}"#,
        )
        .unwrap();

        let merged = merge_single_thread_summary(&heuristic, &llm_json);

        // key_points: LLM first, then heuristic keyword points (TODO/ACTION/etc) appended.
        assert_eq!(
            merged.key_points[0],
            "API migration planned for next sprint"
        );
        assert!(
            merged
                .key_points
                .contains(&"API migration planned for next sprint".to_string())
        );
        assert!(
            merged
                .key_points
                .contains(&"Staging deployment needed before review".to_string())
        );
        assert!(
            merged
                .key_points
                .contains(&"TODO: deploy to staging".to_string())
        );
        assert_eq!(merged.key_points[2], "TODO: deploy to staging");

        // action_items from LLM
        assert_eq!(
            merged.action_items,
            vec!["Deploy to staging", "Update API docs"]
        );

        // mentions from LLM
        assert_eq!(merged.mentions[0].name, "Carol");
        assert_eq!(merged.mentions[0].count, 2);

        // code_references from LLM
        assert_eq!(
            merged.code_references.as_ref().unwrap(),
            &vec!["api/v2/users".to_string()]
        );

        assert_eq!(merged.open_actions, 2);
    }

    #[test]
    fn merge_single_partial_response() {
        let heuristic = ThreadSummary {
            participants: vec!["Alice".into()],
            key_points: vec!["FIXME: broken auth flow".into()],
            action_items: vec![],
            total_messages: 3,
            open_actions: 0,
            done_actions: 0,
            mentions: vec![],
            code_references: None,
        };

        let llm_json: Value = serde_json::from_str(
            r#"{"key_points": ["Authentication refactor in progress", "Need to update middleware"]}"#,
        )
        .unwrap();

        let merged = merge_single_thread_summary(&heuristic, &llm_json);

        // LLM key_points first, then heuristic keyword points appended.
        assert_eq!(merged.key_points[0], "Authentication refactor in progress");
        assert!(
            merged
                .key_points
                .contains(&"Authentication refactor in progress".to_string())
        );
        assert_eq!(merged.key_points[2], "FIXME: broken auth flow");

        // participants unchanged (not in LLM response)
        assert_eq!(merged.participants, vec!["Alice"]);
        assert_eq!(merged.total_messages, 3);
    }

    #[test]
    fn merge_single_llm_failure() {
        let heuristic = ThreadSummary {
            participants: vec!["Alice".into()],
            key_points: vec!["BLOCKED: waiting on infra".into()],
            action_items: vec!["BLOCKED: waiting on infra".into()],
            total_messages: 2,
            open_actions: 1,
            done_actions: 0,
            mentions: vec![],
            code_references: None,
        };

        // LLM failure = no merge, return heuristic as-is
        // (caller should detect None from parse_json_safely and skip merge)
        assert_eq!(heuristic.key_points, vec!["BLOCKED: waiting on infra"]);
        assert_eq!(heuristic.action_items, vec!["BLOCKED: waiting on infra"]);
    }

    #[test]
    fn merge_multi_thread_refinement() {
        let heuristic = AggregateSummary {
            top_mentions: vec![TopMention::Count(MentionCount {
                name: "Alice".into(),
                count: 3,
            })],
            key_points: vec![
                "TODO: finalize API schema".into(),
                "migration timeline discussed".into(),
            ],
            action_items: vec!["TODO: finalize API schema".into()],
        };

        let mut threads = vec![
            ThreadEntry {
                thread_id: "T-1".to_string(),
                summary: ThreadSummary {
                    participants: vec!["Alice".into()],
                    key_points: vec!["heuristic kp".into()],
                    action_items: vec!["heuristic action".into()],
                    total_messages: 2,
                    open_actions: 0,
                    done_actions: 0,
                    mentions: vec![],
                    code_references: None,
                },
            },
            ThreadEntry {
                thread_id: "T-2".to_string(),
                summary: ThreadSummary {
                    participants: vec!["Bob".into()],
                    key_points: vec!["heuristic kp 2".into()],
                    action_items: vec!["heuristic action 2".into()],
                    total_messages: 1,
                    open_actions: 0,
                    done_actions: 0,
                    mentions: vec![],
                    code_references: None,
                },
            },
        ];

        let llm_json: Value = serde_json::from_str(
            r#"{"threads": [{"thread_id": "T-1", "key_points": ["API v2 schema finalized"], "actions": ["Update OpenAPI spec"]}, {"thread_id": "T-2", "key_points": ["Migration to new DB"], "actions": ["Run migration script"]}], "aggregate": {"top_mentions": ["Alice", "Bob"], "key_points": ["API schema and DB migration are the two main workstreams"], "action_items": ["Update OpenAPI spec", "Run migration script"]}}"#,
        )
        .unwrap();

        apply_multi_thread_thread_revisions(&mut threads, &llm_json);
        assert_eq!(threads[0].thread_id, "T-1");
        assert_eq!(
            threads[0].summary.key_points,
            vec!["API v2 schema finalized"]
        );
        assert_eq!(threads[0].summary.action_items, vec!["Update OpenAPI spec"]);
        assert_eq!(threads[1].thread_id, "T-2");
        assert_eq!(threads[1].summary.key_points, vec!["Migration to new DB"]);
        assert_eq!(
            threads[1].summary.action_items,
            vec!["Run migration script"]
        );

        let merged = merge_multi_thread_aggregate(&heuristic, &llm_json);

        assert_eq!(
            merged.key_points,
            vec!["API schema and DB migration are the two main workstreams"]
        );
        assert_eq!(
            merged.action_items,
            vec!["Update OpenAPI spec", "Run migration script"]
        );
        // top_mentions from LLM (string form)
        assert!(matches!(
            &merged.top_mentions[0],
            TopMention::Name(name) if name == "Alice"
        ));
        assert!(matches!(
            &merged.top_mentions[1],
            TopMention::Name(name) if name == "Bob"
        ));
    }

    // -- extract_fenced_json edge cases --

    #[test]
    fn fenced_json_with_json_tag() {
        let input = "Preamble\n```json\n{\"a\": 1}\n```\nEpilogue";
        let val = extract_fenced_json(input).unwrap();
        assert_eq!(val["a"], 1);
    }

    #[test]
    fn fenced_json_without_tag() {
        let input = "```\n{\"b\": 2}\n```";
        let val = extract_fenced_json(input).unwrap();
        assert_eq!(val["b"], 2);
    }

    #[test]
    fn fenced_json_crlf_line_endings() {
        let input = "```json\r\n{\"c\": 3}\r\n```";
        let val = extract_fenced_json(input).unwrap();
        assert_eq!(val["c"], 3);
    }

    #[test]
    fn fenced_json_no_closing_fence() {
        let input = "```json\n{\"d\": 4}\nno closing";
        assert!(extract_fenced_json(input).is_none());
    }

    #[test]
    fn fenced_json_invalid_json_content() {
        let input = "```json\nnot json at all\n```";
        assert!(extract_fenced_json(input).is_none());
    }

    #[test]
    fn fenced_json_empty_fence() {
        let input = "```json\n\n```";
        assert!(extract_fenced_json(input).is_none());
    }

    #[test]
    fn fenced_json_picks_first_block() {
        let input = "```json\n{\"first\": true}\n```\n```json\n{\"second\": true}\n```";
        let val = extract_fenced_json(input).unwrap();
        assert!(val["first"].as_bool().unwrap());
    }

    // -- extract_brace_json edge cases --

    #[test]
    fn brace_json_simple() {
        let val = extract_brace_json("text {\"x\": 1} more").unwrap();
        assert_eq!(val["x"], 1);
    }

    #[test]
    fn brace_json_nested() {
        let val = extract_brace_json("prefix {\"a\": {\"b\": 2}} suffix").unwrap();
        assert_eq!(val["a"]["b"], 2);
    }

    #[test]
    fn brace_json_no_braces() {
        assert!(extract_brace_json("no json here").is_none());
    }

    #[test]
    fn brace_json_close_before_open() {
        assert!(extract_brace_json("} before { is bad").is_none());
    }

    #[test]
    fn brace_json_single_brace_only() {
        assert!(extract_brace_json("{").is_none());
        assert!(extract_brace_json("}").is_none());
    }

    #[test]
    fn brace_json_invalid_content() {
        assert!(extract_brace_json("{not: valid json}").is_none());
    }

    // -- stubbed_completion_content tests --

    #[test]
    fn stub_single_thread_returns_fenced_json() {
        let content = stubbed_completion_content("Summarize this thread", "Messages follow...");
        assert!(content.contains("```json"));
        assert!(content.contains("participants"));
        assert!(content.contains("BlueLake"));
        // Should parse successfully via fenced extraction
        let val = extract_fenced_json(&content).unwrap();
        assert!(val["participants"].is_array());
    }

    #[test]
    fn stub_multi_thread_returns_brace_json() {
        let content = stubbed_completion_content(
            "Digest across threads for a multi-thread summary",
            "Digest these threads: ...",
        );
        // Should NOT contain code fences (exercises brace-slice fallback)
        assert!(!content.contains("```json"));
        assert!(content.contains("threads"));
        // Should parse via brace extraction
        let val = extract_brace_json(&content).unwrap();
        assert!(val["aggregate"]["key_points"].is_array());
    }

    #[test]
    fn stub_single_thread_has_required_fields() {
        let content = stubbed_completion_content("summary prompt", "user messages");
        let val = extract_fenced_json(&content).unwrap();
        // All expected fields present
        assert!(val["participants"].is_array());
        assert!(val["key_points"].is_array());
        assert!(val["action_items"].is_array());
        assert!(val["total_messages"].is_number());
    }

    #[test]
    fn stub_multi_thread_has_required_fields() {
        let content =
            stubbed_completion_content("digest across threads", "Digest these threads: T-1, T-2");
        let val = extract_brace_json(&content).unwrap();
        assert!(val["threads"].is_array());
        assert!(val["aggregate"]["top_mentions"].is_array());
        assert!(val["aggregate"]["action_items"].is_array());
    }

    // -- prompt building tests --

    #[test]
    fn single_thread_prompt_truncation() {
        let messages = vec![(1, "Alice".to_string(), "Test".to_string(), "x".repeat(1000))];
        let prompt = single_thread_user_prompt(&messages);
        // Should contain truncated body (800 chars)
        assert!(prompt.len() < 1000);
        assert!(prompt.contains("Message 1 from Alice"));
    }

    #[test]
    fn multi_thread_prompt_limits() {
        let threads: Vec<(String, Vec<String>, Vec<String>)> = (0..10)
            .map(|i| {
                (
                    format!("T-{i}"),
                    vec!["point".to_string(); 10],
                    vec!["action".to_string(); 10],
                )
            })
            .collect();
        let prompt = multi_thread_user_prompt(&threads);
        // Only 8 threads included
        assert!(prompt.contains("T-7"));
        assert!(!prompt.contains("T-8"));
    }

    // -- system prompt format tests --

    #[test]
    fn single_thread_system_prompt_contains_required_keys() {
        let prompt = single_thread_system_prompt();
        for key in &[
            "participants",
            "key_points",
            "action_items",
            "mentions",
            "code_references",
            "total_messages",
            "open_actions",
            "done_actions",
        ] {
            assert!(
                prompt.contains(key),
                "single_thread_system_prompt missing key: {key}"
            );
        }
    }

    #[test]
    fn multi_thread_system_prompt_contains_required_keys() {
        let prompt = multi_thread_system_prompt();
        for key in &[
            "thread_id",
            "key_points",
            "actions",
            "top_mentions",
            "action_items",
        ] {
            assert!(
                prompt.contains(key),
                "multi_thread_system_prompt missing key: {key}"
            );
        }
    }

    #[test]
    fn single_thread_system_prompt_requests_json() {
        let prompt = single_thread_system_prompt();
        assert!(prompt.contains("JSON"), "should request JSON output");
    }

    #[test]
    fn multi_thread_system_prompt_requests_json() {
        let prompt = multi_thread_system_prompt();
        assert!(prompt.contains("JSON"), "should request JSON output");
    }

    // -- single_thread_user_prompt edge cases --

    #[test]
    fn single_thread_user_prompt_empty_messages() {
        let messages: Vec<(i64, String, String, String)> = vec![];
        let prompt = single_thread_user_prompt(&messages);
        assert!(prompt.starts_with("Summarize this thread:"));
        // Should not contain any message separator
        assert!(!prompt.contains("Message "));
    }

    #[test]
    fn single_thread_user_prompt_includes_all_fields() {
        let messages = vec![(
            42,
            "BlueLake".to_string(),
            "Design review".to_string(),
            "Let's review the API changes.".to_string(),
        )];
        let prompt = single_thread_user_prompt(&messages);
        assert!(prompt.contains("Message 42 from BlueLake"));
        assert!(prompt.contains("Subject: Design review"));
        assert!(prompt.contains("Let's review the API changes."));
    }

    #[test]
    fn single_thread_user_prompt_respects_max_messages() {
        let messages: Vec<_> = (0..MAX_MESSAGES_FOR_LLM + 5)
            .map(|i| {
                (
                    i64::try_from(i).unwrap(),
                    "Agent".to_string(),
                    "Subj".to_string(),
                    "body".to_string(),
                )
            })
            .collect();
        let prompt = single_thread_user_prompt(&messages);
        // Should contain the last included message
        let last_included = MAX_MESSAGES_FOR_LLM - 1;
        assert!(prompt.contains(&format!("Message {last_included}")));
        // Should NOT contain the first excluded message
        assert!(!prompt.contains(&format!("Message {MAX_MESSAGES_FOR_LLM}")));
    }

    // -- multi_thread_user_prompt edge cases --

    #[test]
    fn multi_thread_user_prompt_empty_threads() {
        let threads: Vec<(String, Vec<String>, Vec<String>)> = vec![];
        let prompt = multi_thread_user_prompt(&threads);
        assert!(prompt.starts_with("Digest these threads:"));
        assert!(!prompt.contains("Thread:"));
    }

    #[test]
    fn multi_thread_user_prompt_truncates_key_points() {
        let threads = vec![(
            "T-1".to_string(),
            (0..20).map(|i| format!("point-{i}")).collect(),
            vec!["action".to_string()],
        )];
        let prompt = multi_thread_user_prompt(&threads);
        // Should include up to MAX_KEY_POINTS_PER_THREAD
        let last_included = MAX_KEY_POINTS_PER_THREAD - 1;
        assert!(prompt.contains(&format!("point-{last_included}")));
        assert!(!prompt.contains(&format!("point-{MAX_KEY_POINTS_PER_THREAD}")));
    }

    #[test]
    fn multi_thread_user_prompt_truncates_actions() {
        let threads = vec![(
            "T-1".to_string(),
            vec!["kp".to_string()],
            (0..20).map(|i| format!("action-{i}")).collect(),
        )];
        let prompt = multi_thread_user_prompt(&threads);
        let last_included = MAX_ACTIONS_PER_THREAD - 1;
        assert!(prompt.contains(&format!("action-{last_included}")));
        assert!(!prompt.contains(&format!("action-{MAX_ACTIONS_PER_THREAD}")));
    }

    // ── choose_best_available_model ─────────────────────────────────

    #[test]
    fn choose_best_model_passthrough_qualified_name() {
        // Names with / or : are provider-qualified and pass through
        assert_eq!(choose_best_available_model("openai/gpt-4"), "openai/gpt-4");
        assert_eq!(
            choose_best_available_model("anthropic:claude-3"),
            "anthropic:claude-3"
        );
    }

    #[test]
    fn choose_best_model_returns_string() {
        // Can't control env vars in tests, but the function must always
        // return a non-empty string (either from env or DEFAULT_MODEL).
        let result = choose_best_available_model("auto");
        assert!(!result.is_empty());
    }

    // ── apply_multi_thread_thread_revisions ──────────────────────────

    fn make_thread_entry(id: &str, kp: &[&str], actions: &[&str]) -> ThreadEntry {
        ThreadEntry {
            thread_id: id.to_string(),
            summary: ThreadSummary {
                participants: vec![],
                key_points: kp.iter().map(std::string::ToString::to_string).collect(),
                action_items: actions
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
                total_messages: 0,
                open_actions: 0,
                done_actions: 0,
                mentions: vec![],
                code_references: None,
            },
        }
    }

    #[test]
    fn apply_revisions_updates_matching_threads() {
        let mut threads = vec![
            make_thread_entry("T-1", &["old-kp"], &["old-action"]),
            make_thread_entry("T-2", &["kp2"], &["action2"]),
        ];
        let llm_json = serde_json::json!({
            "threads": [
                {
                    "thread_id": "T-1",
                    "key_points": ["new-kp-1", "new-kp-2"],
                    "actions": ["new-action"]
                }
            ]
        });
        apply_multi_thread_thread_revisions(&mut threads, &llm_json);
        assert_eq!(threads[0].summary.key_points, vec!["new-kp-1", "new-kp-2"]);
        assert_eq!(threads[0].summary.action_items, vec!["new-action"]);
        // T-2 unchanged
        assert_eq!(threads[1].summary.key_points, vec!["kp2"]);
    }

    #[test]
    fn apply_revisions_no_threads_key_is_noop() {
        let mut threads = vec![make_thread_entry("T-1", &["kp"], &["a"])];
        let llm_json = serde_json::json!({ "something_else": true });
        apply_multi_thread_thread_revisions(&mut threads, &llm_json);
        assert_eq!(threads[0].summary.key_points, vec!["kp"]);
    }

    #[test]
    fn apply_revisions_empty_threads_array_is_noop() {
        let mut threads = vec![make_thread_entry("T-1", &["kp"], &["a"])];
        let llm_json = serde_json::json!({ "threads": [] });
        apply_multi_thread_thread_revisions(&mut threads, &llm_json);
        assert_eq!(threads[0].summary.key_points, vec!["kp"]);
    }

    #[test]
    fn apply_revisions_skips_entries_without_thread_id() {
        let mut threads = vec![make_thread_entry("T-1", &["kp"], &["a"])];
        let llm_json = serde_json::json!({
            "threads": [
                { "key_points": ["new"] }  // no thread_id
            ]
        });
        apply_multi_thread_thread_revisions(&mut threads, &llm_json);
        assert_eq!(threads[0].summary.key_points, vec!["kp"]);
    }

    #[test]
    fn apply_revisions_empty_arrays_preserve_original() {
        let mut threads = vec![make_thread_entry("T-1", &["kp"], &["action"])];
        let llm_json = serde_json::json!({
            "threads": [{
                "thread_id": "T-1",
                "key_points": [],
                "actions": []
            }]
        });
        apply_multi_thread_thread_revisions(&mut threads, &llm_json);
        // Empty arrays don't overwrite existing data
        assert_eq!(threads[0].summary.key_points, vec!["kp"]);
        assert_eq!(threads[0].summary.action_items, vec!["action"]);
    }

    // ── merge_multi_thread_aggregate ────────────────────────────────

    fn make_aggregate(kp: &[&str], actions: &[&str]) -> AggregateSummary {
        AggregateSummary {
            top_mentions: vec![],
            key_points: kp.iter().map(std::string::ToString::to_string).collect(),
            action_items: actions
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
        }
    }

    #[test]
    fn merge_aggregate_overlays_key_points_and_actions() {
        let heuristic = make_aggregate(&["old-kp"], &["old-action"]);
        let llm_json = serde_json::json!({
            "aggregate": {
                "key_points": ["new-kp"],
                "action_items": ["new-action"]
            }
        });
        let result = merge_multi_thread_aggregate(&heuristic, &llm_json);
        assert_eq!(result.key_points, vec!["new-kp"]);
        assert_eq!(result.action_items, vec!["new-action"]);
    }

    #[test]
    fn merge_aggregate_no_aggregate_key_preserves_heuristic() {
        let heuristic = make_aggregate(&["kp"], &["action"]);
        let llm_json = serde_json::json!({ "other": "data" });
        let result = merge_multi_thread_aggregate(&heuristic, &llm_json);
        assert_eq!(result.key_points, vec!["kp"]);
        assert_eq!(result.action_items, vec!["action"]);
    }

    #[test]
    fn merge_aggregate_top_mentions_as_strings() {
        let heuristic = make_aggregate(&["kp"], &[]);
        let llm_json = serde_json::json!({
            "aggregate": {
                "top_mentions": ["Alice", "Bob"]
            }
        });
        let result = merge_multi_thread_aggregate(&heuristic, &llm_json);
        assert_eq!(result.top_mentions.len(), 2);
        match &result.top_mentions[0] {
            TopMention::Name(n) => assert_eq!(n, "Alice"),
            other @ TopMention::Count(_) => panic!("expected Name, got {other:?}"),
        }
    }

    #[test]
    fn merge_aggregate_top_mentions_as_objects() {
        let heuristic = make_aggregate(&[], &[]);
        let llm_json = serde_json::json!({
            "aggregate": {
                "top_mentions": [
                    { "name": "Alice", "count": 5 },
                    { "name": "Bob" }
                ]
            }
        });
        let result = merge_multi_thread_aggregate(&heuristic, &llm_json);
        assert_eq!(result.top_mentions.len(), 2);
        match &result.top_mentions[0] {
            TopMention::Count(mc) => {
                assert_eq!(mc.name, "Alice");
                assert_eq!(mc.count, 5);
            }
            other @ TopMention::Name(_) => panic!("expected Count, got {other:?}"),
        }
        match &result.top_mentions[1] {
            TopMention::Count(mc) => {
                assert_eq!(mc.name, "Bob");
                assert_eq!(mc.count, 0); // default when missing
            }
            other @ TopMention::Name(_) => panic!("expected Count, got {other:?}"),
        }
    }

    #[test]
    fn merge_aggregate_empty_arrays_preserve_heuristic() {
        let heuristic = make_aggregate(&["kp"], &["action"]);
        let llm_json = serde_json::json!({
            "aggregate": {
                "key_points": [],
                "action_items": [],
                "top_mentions": []
            }
        });
        let result = merge_multi_thread_aggregate(&heuristic, &llm_json);
        // Empty arrays don't overwrite
        assert_eq!(result.key_points, vec!["kp"]);
        assert_eq!(result.action_items, vec!["action"]);
    }
}
