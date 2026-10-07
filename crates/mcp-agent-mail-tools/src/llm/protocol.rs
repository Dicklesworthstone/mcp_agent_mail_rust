//! Provider routing and wire formats, without environment mutation or network I/O.

use serde_json::{Value, json};

use super::LlmError;

const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    OpenAi,
    Anthropic,
    Google,
    Groq,
    DeepSeek,
    Xai,
    OpenRouter,
}

impl Provider {
    fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "openai" | "gpt" => Some(Self::OpenAi),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "google" | "gemini" => Some(Self::Google),
            "groq" => Some(Self::Groq),
            "deepseek" => Some(Self::DeepSeek),
            "xai" => Some(Self::Xai),
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    fn inferred(model: &str) -> Self {
        let lower = model.to_ascii_lowercase();
        if lower.starts_with("claude") {
            Self::Anthropic
        } else if lower.starts_with("gemini") {
            Self::Google
        } else if lower.starts_with("deepseek") {
            Self::DeepSeek
        } else if lower.starts_with("grok") {
            Self::Xai
        } else {
            Self::OpenAi
        }
    }

    const fn endpoint(self) -> (&'static str, &'static str) {
        match self {
            Self::OpenAi => (OPENAI_URL, "OPENAI_API_KEY"),
            Self::Anthropic => (ANTHROPIC_URL, "ANTHROPIC_API_KEY"),
            Self::Google => (
                "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
                "GOOGLE_API_KEY",
            ),
            Self::Groq => ("https://api.groq.com/openai/v1/chat/completions", "GROQ_API_KEY"),
            Self::DeepSeek => ("https://api.deepseek.com/v1/chat/completions", "DEEPSEEK_API_KEY"),
            Self::Xai => ("https://api.x.ai/v1/chat/completions", "XAI_API_KEY"),
            Self::OpenRouter => ("https://openrouter.ai/api/v1/chat/completions", "OPENROUTER_API_KEY"),
        }
    }
}

pub(super) fn resolve_api_endpoint(
    model: &str,
    get_key: impl Fn(&str) -> Option<String>,
) -> Result<(String, String, String), LlmError> {
    let trimmed = model.trim();
    // Split at the FIRST separator: openrouter:vendor/model must retain vendor/model.
    // Only known prefixes are stripped, preserving custom and ft:... OpenAI IDs.
    let (provider, api_model) = trimmed
        .split_once(['/', ':'])
        .and_then(|(prefix, suffix)| Provider::named(prefix).map(|p| (p, suffix)))
        .unwrap_or_else(|| (Provider::inferred(trimmed), trimmed));
    if api_model.trim().is_empty() {
        return Err(LlmError::ParseError("LLM model name is empty".to_string()));
    }
    let (url, key_name) = provider.endpoint();
    let key = get_key(key_name)
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| LlmError::NoApiKey(model.to_string()))?;
    let auth = if provider == Provider::Anthropic {
        key
    } else {
        format!("Bearer {key}")
    };
    Ok((url.to_string(), auth, api_model.to_string()))
}

fn uses_fixed_sampling(model: &str) -> bool {
    // Dated variants share the base model's restrictions. GPT-5.1+ default to
    // non-reasoning mode and accept sampling settings; do not discard those.
    let lower = model.to_ascii_lowercase();
    let base = lower.strip_prefix("ft:").unwrap_or(&lower);
    let base = base.split(':').next().unwrap_or(base);
    ["o1", "o3", "o4", "gpt-5"].iter().any(|prefix| {
        base == *prefix
            || base
                .strip_prefix(*prefix)
                .is_some_and(|rest| rest.starts_with('-'))
    })
}

pub(super) fn request_payload(
    url: &str,
    model: &str,
    system: &str,
    user: &str,
    temperature: f64,
    max_tokens: u32,
) -> Value {
    if url == ANTHROPIC_URL {
        return json!({
            "model": model,
            "system": system,
            "messages": [{"role": "user", "content": user}],
            "temperature": temperature,
            "max_tokens": max_tokens
        });
    }
    let mut payload = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ]
    });
    if url == OPENAI_URL {
        // max_tokens is deprecated and rejected by o-series models. Do not
        // impose this OpenAI-specific spelling on other compatible providers.
        payload["max_completion_tokens"] = json!(max_tokens);
        if !uses_fixed_sampling(model) {
            payload["temperature"] = json!(temperature);
        }
    } else {
        payload["max_tokens"] = json!(max_tokens);
        payload["temperature"] = json!(temperature);
    }
    payload
}

pub(super) fn response_content(response: &Value, is_anthropic: bool) -> Result<String, LlmError> {
    let content = if is_anthropic {
        response.pointer("/content/0/text")
    } else {
        response.pointer("/choices/0/message/content")
    }
    .and_then(Value::as_str)
    .filter(|text| !text.trim().is_empty())
    .ok_or_else(|| LlmError::ParseError("LLM response has no usable text content".to_string()))?;
    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(model: &str, expected_key: &str) -> (String, String, String) {
        resolve_api_endpoint(model, |key| {
            assert_eq!(key, expected_key, "incorrect provider for {model}");
            Some("test-key".to_string())
        })
        .expect("endpoint")
    }

    #[test]
    fn every_auto_selected_model_uses_its_own_provider_key() {
        for &(key, model) in super::super::MODEL_PRIORITY {
            route(model, key);
        }
    }

    #[test]
    fn qualification_preserves_nested_models_and_colons() {
        for model in [
            "openrouter:meta-llama/llama-4-scout-17b",
            "OpenRouter/meta-llama/llama-4-scout-17b",
        ] {
            let (url, auth, name) = route(model, "OPENROUTER_API_KEY");
            assert!(url.starts_with("https://openrouter.ai/"));
            assert_eq!(auth, "Bearer test-key");
            assert_eq!(name, "meta-llama/llama-4-scout-17b");
        }
        assert_eq!(route("GROQ:openai/gpt-oss-120b", "GROQ_API_KEY").2, "openai/gpt-oss-120b");
        assert_eq!(route("openrouter:vendor/model:free", "OPENROUTER_API_KEY").2, "vendor/model:free");
    }

    #[test]
    fn provider_prefixes_are_case_insensitive() {
        for (model, key, expected) in [
            ("OpenAI/gpt-5.4", "OPENAI_API_KEY", "gpt-5.4"),
            ("ANTHROPIC:claude-custom", "ANTHROPIC_API_KEY", "claude-custom"),
            ("Gemini:gemini-custom", "GOOGLE_API_KEY", "gemini-custom"),
            ("DeepSeek/deepseek-chat", "DEEPSEEK_API_KEY", "deepseek-chat"),
            ("XAI:grok-3", "XAI_API_KEY", "grok-3"),
        ] {
            assert_eq!(route(model, key).2, expected);
        }
    }

    #[test]
    fn bare_deepseek_and_grok_names_do_not_require_openai_credentials() {
        assert!(route("deepseek-reasoner", "DEEPSEEK_API_KEY").0.contains("api.deepseek.com"));
        assert!(route("grok-3", "XAI_API_KEY").0.contains("api.x.ai"));
    }

    #[test]
    fn custom_and_fine_tuned_openai_ids_are_not_mistaken_for_providers() {
        for model in ["ft:gpt-4o:organization:custom:id", "organization/custom-model"] {
            let (url, _, name) = route(model, "OPENAI_API_KEY");
            assert_eq!(url, OPENAI_URL);
            assert_eq!(name, model);
        }
    }

    #[test]
    fn anthropic_auth_is_not_bearer_prefixed() {
        assert_eq!(route("claude-custom", "ANTHROPIC_API_KEY").1, "test-key");
    }

    #[test]
    fn absent_or_blank_provider_credentials_are_errors_without_fallback() {
        for key in [None, Some(String::new()), Some("  ".to_string())] {
            assert!(matches!(
                resolve_api_endpoint("deepseek/deepseek-chat", |name| {
                    assert_eq!(name, "DEEPSEEK_API_KEY");
                    key.clone()
                }),
                Err(LlmError::NoApiKey(model)) if model == "deepseek/deepseek-chat"
            ));
        }
    }

    #[test]
    fn empty_model_is_rejected_before_credentials_are_read() {
        for model in ["", "  ", "openai/", "openrouter: "] {
            assert!(matches!(
                resolve_api_endpoint(model, |_| panic!("should not read credentials")),
                Err(LlmError::ParseError(_))
            ));
        }
    }

    #[test]
    fn openai_reasoning_models_use_supported_token_and_sampling_parameters() {
        for model in ["o1", "o3-mini", "o4-mini-2025-04-16", "gpt-5", "gpt-5-mini", "gpt-5-nano"] {
            let body = request_payload(OPENAI_URL, model, "system", "user", 0.2, 4096);
            assert_eq!(body["max_completion_tokens"], 4096);
            assert!(body.get("max_tokens").is_none());
            assert!(body.get("temperature").is_none(), "{model}");
            assert_eq!(body["messages"][0]["content"], "system");
            assert_eq!(body["messages"][1]["content"], "user");
        }
    }

    #[test]
    fn openai_default_and_non_reasoning_models_preserve_configured_sampling() {
        for model in [super::super::DEFAULT_MODEL, "gpt-4o", "gpt-5.1", "gpt-5.2"] {
            let body = request_payload(OPENAI_URL, model, "system", "user", 0.7, 1234);
            assert_eq!(body["temperature"], json!(0.7));
            assert_eq!(body["max_completion_tokens"], 1234);
            assert!(body.get("max_tokens").is_none());
        }
    }

    #[test]
    fn compatible_providers_keep_their_own_request_contract() {
        let (url, _, model) = route("groq/openai/gpt-oss-120b", "GROQ_API_KEY");
        let body = request_payload(&url, &model, "system", "user", 0.4, 512);
        assert_eq!(body["model"], "openai/gpt-oss-120b");
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["temperature"], json!(0.4));
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn anthropic_payload_keeps_system_outside_messages() {
        let body = request_payload(ANTHROPIC_URL, "claude-custom", "system", "user", 0.3, 1000);
        assert_eq!(body["system"], "system");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["max_tokens"], 1000);
    }

    #[test]
    fn missing_or_empty_completion_text_is_not_success() {
        for response in [json!({}), json!({"choices": []}), json!({"choices": [{"message": {"content": "  "}}]})] {
            assert!(response_content(&response, false).is_err());
        }
        assert!(response_content(&json!({"content": []}), true).is_err());
    }
}
