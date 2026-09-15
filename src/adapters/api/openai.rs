//! The OpenAI Chat Completions wire format: `POST {base}/chat/completions`.
//!
//! Chat Completions, not the Responses API, because it is the format other
//! providers copy (OpenRouter, Groq, vLLM, Ollama, LM Studio, and more). The
//! base URL includes the version, the same as the `OPENAI_BASE_URL` convention.

use serde_json::{Value, json};

use super::{Call, Credential, HttpRequest, Reply};
use crate::transcript::Usage;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const BASE_URL_ENV: &str = "OPENAI_BASE_URL";
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";

/// The key goes in `Authorization: Bearer`. A keyless local server still needs
/// a value; any value works.
pub fn credential(
    api_key_env: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Credential, String> {
    let var = api_key_env.unwrap_or(API_KEY_ENV);
    env(var).map(|key| Credential::bearer(var, key)).ok_or_else(|| match api_key_env {
        Some(_) => format!("--api-key-env {var} is not set"),
        None => format!(
            "no credential: set {API_KEY_ENV} (any value for a keyless local server), or name another variable with --api-key-env"
        ),
    })
}

fn auth_header(credential: &Credential) -> (&'static str, String) {
    ("authorization", format!("Bearer {}", credential.value))
}

pub fn message_request(call: &Call) -> HttpRequest {
    let messages: Vec<Value> = call
        .system
        .map(|system| json!({ "role": "system", "content": system }))
        .into_iter()
        .chain(std::iter::once(json!({ "role": "user", "content": call.prompt })))
        .collect();
    let mut body = json!({ "model": call.model, "messages": messages });
    // Optional here, unlike Anthropic. `max_completion_tokens` replaced the
    // deprecated `max_tokens`, which newer reasoning models reject.
    if let Some(cap) = call.max_tokens {
        body["max_completion_tokens"] = json!(cap);
    }
    HttpRequest {
        url: format!("{}/chat/completions", call.base_url),
        headers: vec![auth_header(call.credential), ("content-type", "application/json".to_string())],
        body: Some(body.to_string()),
    }
}

pub fn models_request(base_url: &str, credential: &Credential) -> HttpRequest {
    HttpRequest { url: format!("{base_url}/models"), headers: vec![auth_header(credential)], body: None }
}

/// Read a Chat Completions response. Any status outside 2xx is an error.
pub fn parse_reply(status: u16, body: &str) -> Reply {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Reply::failure(format!("{status}: {}", super::excerpt(body)));
    };
    if !(200..300).contains(&status) {
        return parse_error(status, &v, body);
    }
    let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
        return Reply::failure(format!("{status}: response has no choices: {}", super::excerpt(body)));
    };
    let message = choice.get("message");
    let text = message.and_then(|m| m.get("content")).map(content_text).unwrap_or_default();
    let refusal = message.and_then(|m| m.get("refusal")).and_then(Value::as_str).filter(|r| !r.is_empty());
    let finish = choice.get("finish_reason").and_then(Value::as_str).unwrap_or_default();
    let filtered = finish == "content_filter";
    let capped_empty = finish == "length" && text.is_empty() && refusal.is_none();

    let usage = v.get("usage");
    let get = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
    let cached = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let str_of = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();

    Reply {
        text: match (refusal, filtered, text.is_empty()) {
            (Some(refusal), _, _) => refusal.to_string(),
            (None, true, true) => "the provider filtered the answer (finish_reason: content_filter)".to_string(),
            _ if capped_empty => {
                "the answer reached the token cap before any text (finish_reason: length); raise --max-tokens"
                    .to_string()
            }
            _ => text,
        },
        model: str_of("model"),
        id: str_of("id"),
        usage: Usage {
            // `prompt_tokens` includes the cached part. Split it out so input
            // means the same as on the other harnesses: tokens not read from cache.
            input_tokens: get("prompt_tokens").saturating_sub(cached),
            output_tokens: get("completion_tokens"),
            cache_read_input_tokens: cached,
            cache_creation_input_tokens: 0,
        },
        is_error: refusal.is_some() || filtered || capped_empty,
        invalid_model: false,
    }
}

/// `content` is a string, or on some providers an array of text parts.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

fn parse_error(status: u16, v: &Value, body: &str) -> Reply {
    let err = v.get("error");
    // OpenAI nests `{message, type, code}` under `error`. Compatible servers
    // also send `error` as a plain string, or a top-level `message`/`detail`.
    let message = err
        .and_then(|e| e.get("message"))
        .or(err.filter(|e| e.is_string()))
        .or_else(|| v.get("message"))
        .or_else(|| v.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if message.is_empty() {
        return Reply::failure(format!("{status}: {}", super::excerpt(body)));
    }
    let code = err.and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or_default();
    let kind = Some(code)
        .filter(|c| !c.is_empty())
        .or_else(|| err.and_then(|e| e.get("type")).and_then(Value::as_str))
        .unwrap_or_default();
    let text = if kind.is_empty() { format!("{status}: {message}") } else { format!("{status} {kind}: {message}") };
    Reply { invalid_model: code == "model_not_found" || names_a_missing_model(status, message), ..Reply::failure(text) }
}

/// A compatible server that sends no `model_not_found` code says it in words:
/// vLLM's "The model `x` does not exist", Ollama's "model \"x\" not found",
/// OpenRouter's "x is not a valid model ID".
fn names_a_missing_model(status: u16, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    matches!(status, 400 | 404)
        && m.contains("model")
        && (m.contains("not found") || m.contains("does not exist") || m.contains("not a valid model"))
}

/// The model ids in a `GET {base}/models` response.
pub fn parse_models(body: &str) -> Result<Vec<String>, String> {
    super::model_ids(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(name, _)| *name == k).map(|(_, v)| v.to_string())
    }

    fn call<'a>(credential: &'a Credential) -> Call<'a> {
        Call {
            base_url: "http://127.0.0.1:11434/v1",
            credential,
            model: "gpt-5.5",
            prompt: "say ok",
            system: None,
            max_tokens: None,
        }
    }

    #[test]
    fn the_key_is_a_bearer_token_from_the_named_variable() {
        let env = env_with(&[(API_KEY_ENV, "sk-1"), ("OPENROUTER_API_KEY", "or-1")]);
        assert_eq!(credential(None, &env).unwrap(), Credential::bearer(API_KEY_ENV, "sk-1".into()));
        assert_eq!(
            credential(Some("OPENROUTER_API_KEY"), &env).unwrap(),
            Credential::bearer("OPENROUTER_API_KEY", "or-1".into())
        );
        assert!(credential(Some("NOPE"), &env).unwrap_err().contains("NOPE"));
        assert!(credential(None, &env_with(&[])).unwrap_err().contains("--api-key-env"));
    }

    #[test]
    fn the_request_is_a_chat_completion_with_no_tools() {
        let c = Credential::bearer(API_KEY_ENV, "sk-1".into());
        let req = message_request(&call(&c));
        assert_eq!(req.url, "http://127.0.0.1:11434/v1/chat/completions");
        assert!(req.headers.contains(&("authorization", "Bearer sk-1".to_string())));
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body, json!({ "model": "gpt-5.5", "messages": [{ "role": "user", "content": "say ok" }] }));

        let full = Call { system: Some("be brief"), max_tokens: Some(256), ..call(&c) };
        let body: Value = serde_json::from_str(message_request(&full).body.as_deref().unwrap()).unwrap();
        assert_eq!(body["messages"][0], json!({ "role": "system", "content": "be brief" }));
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["max_completion_tokens"], 256);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn a_success_folds_text_model_id_and_split_usage() {
        let body = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-5.5-2026-04-01","choices":[{"index":0,"message":{"role":"assistant","content":"ok","refusal":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":5}}}"#;
        let r = parse_reply(200, body);
        assert_eq!(r.text, "ok");
        assert_eq!(r.model, "gpt-5.5-2026-04-01");
        assert_eq!(r.id, "chatcmpl-1");
        assert_eq!(r.usage.input_tokens, 6);
        assert_eq!(r.usage.cache_read_input_tokens, 5);
        assert_eq!(r.usage.output_tokens, 3);
        assert!(!r.is_error);
    }

    #[test]
    fn array_content_and_missing_usage_are_accepted() {
        let body = r#"{"id":"x","model":"local","choices":[{"message":{"content":[{"type":"text","text":"o"},{"type":"text","text":"k"}]},"finish_reason":"stop"}]}"#;
        let r = parse_reply(200, body);
        assert_eq!(r.text, "ok");
        assert_eq!(r.usage, Usage::default());
    }

    #[test]
    fn a_refusal_or_a_filtered_answer_is_an_error() {
        let refused = r#"{"id":"x","model":"m","choices":[{"message":{"content":null,"refusal":"I can't help with that."},"finish_reason":"stop"}]}"#;
        let r = parse_reply(200, refused);
        assert!(r.is_error);
        assert_eq!(r.text, "I can't help with that.");

        let filtered = r#"{"id":"x","model":"m","choices":[{"message":{"content":null},"finish_reason":"content_filter"}]}"#;
        let r = parse_reply(200, filtered);
        assert!(r.is_error);
        assert!(r.text.contains("content_filter"), "{}", r.text);
    }

    #[test]
    fn a_cap_reached_before_any_text_is_an_error() {
        let body = r#"{"id":"x","model":"m","choices":[{"message":{"content":""},"finish_reason":"length"}]}"#;
        let r = parse_reply(200, body);
        assert!(r.is_error);
        assert!(r.text.contains("--max-tokens"), "{}", r.text);
        let partial = r#"{"id":"x","model":"m","choices":[{"message":{"content":"half"},"finish_reason":"length"}]}"#;
        assert!(!parse_reply(200, partial).is_error);
    }

    #[test]
    fn no_choices_is_an_error() {
        let r = parse_reply(200, r#"{"id":"x","choices":[]}"#);
        assert!(r.is_error);
        assert!(r.text.contains("no choices"), "{}", r.text);
    }

    #[test]
    fn model_rejections_are_recognised_across_compatible_servers() {
        let openai = r#"{"error":{"message":"The model `gpt-bogus` does not exist or you do not have access to it.","type":"invalid_request_error","param":null,"code":"model_not_found"}}"#;
        let r = parse_reply(404, openai);
        assert!(r.invalid_model);
        assert!(r.text.starts_with("404 model_not_found: The model"), "{}", r.text);

        let ollama = r#"{"error":{"message":"model \"llama9\" not found, try pulling it first","type":"api_error"}}"#;
        assert!(parse_reply(404, ollama).invalid_model);

        let openrouter = r#"{"error":{"message":"bogus/model is not a valid model ID","code":400}}"#;
        assert!(parse_reply(400, openrouter).invalid_model);

        let plain = r#"{"error":"model 'x' not found"}"#;
        let r = parse_reply(404, plain);
        assert!(r.invalid_model);
        assert_eq!(r.text, "404: model 'x' not found");
    }

    #[test]
    fn other_errors_are_plain_errors() {
        let auth = r#"{"error":{"message":"Incorrect API key provided.","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        let r = parse_reply(401, auth);
        assert!(r.is_error && !r.invalid_model);
        assert_eq!(r.text, "401 invalid_api_key: Incorrect API key provided.");

        // A parameter the model does not take mentions the model but is not a
        // rejected model.
        let param = r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model.","type":"invalid_request_error","code":"unsupported_parameter"}}"#;
        assert!(!parse_reply(400, param).invalid_model);

        let r = parse_reply(503, "upstream unavailable");
        assert_eq!(r.text, "503: upstream unavailable");
    }

    #[test]
    fn models_come_from_the_data_array() {
        let body = r#"{"object":"list","data":[{"id":"gpt-5.5","object":"model"},{"id":"gpt-5.5-mini","object":"model"}]}"#;
        assert_eq!(parse_models(body).unwrap(), vec!["gpt-5.5", "gpt-5.5-mini"]);
    }
}
