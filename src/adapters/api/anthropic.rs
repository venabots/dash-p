//! The Anthropic Messages API wire format: `POST {base}/v1/messages`.
//!
//! Spoken by Anthropic itself and by providers that conform to it. The base URL
//! has no `/v1` suffix, the same as the `ANTHROPIC_BASE_URL` convention.

use serde_json::{Value, json};

use super::{Call, Credential, HttpRequest, Reply};
use crate::transcript::Usage;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

const API_VERSION: &str = "2023-06-01";
/// The Messages API requires `max_tokens`. 16000 leaves room for adaptive
/// thinking and a full answer on a non-streaming call. A provider with a lower
/// cap needs `--max-tokens`.
const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// The credential to send. A bearer token (an OAuth token or a gateway's
/// token) goes in `Authorization`; an API key goes in `x-api-key`. Never both:
/// the API rejects a request that carries two credentials.
///
/// The token wins when both are set, the same order as Claude Code. A gateway
/// setup sets `ANTHROPIC_AUTH_TOKEN` beside a global `ANTHROPIC_API_KEY`, and
/// preferring the key would send it to the gateway host.
///
/// A variable named with `--api-key-env` is a key, unless its name ends in
/// `AUTH_TOKEN`, the `ANTHROPIC_AUTH_TOKEN` convention for a bearer token.
pub fn credential(
    api_key_env: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Credential, String> {
    if let Some(var) = api_key_env {
        let as_credential = if var.ends_with("AUTH_TOKEN") { Credential::bearer } else { Credential::api_key };
        return env(var)
            .map(|value| as_credential(var, value))
            .ok_or_else(|| format!("--api-key-env {var} is not set"));
    }
    env(AUTH_TOKEN_ENV)
        .map(|token| Credential::bearer(AUTH_TOKEN_ENV, token))
        .or_else(|| env(API_KEY_ENV).map(|key| Credential::api_key(API_KEY_ENV, key)))
        .ok_or_else(|| {
            format!("no credential: set {API_KEY_ENV} (or {AUTH_TOKEN_ENV}), or name another variable with --api-key-env")
        })
}

fn auth_header(credential: &Credential) -> (&'static str, String) {
    if credential.bearer {
        ("authorization", format!("Bearer {}", credential.value))
    } else {
        ("x-api-key", credential.value.clone())
    }
}

pub fn message_request(call: &Call) -> HttpRequest {
    let mut body = json!({
        "model": call.model,
        "max_tokens": call.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": [{ "role": "user", "content": call.prompt }],
    });
    if let Some(system) = call.system {
        body["system"] = json!(system);
    }
    HttpRequest {
        url: format!("{}/v1/messages", call.base_url),
        headers: vec![
            auth_header(call.credential),
            ("anthropic-version", API_VERSION.to_string()),
            ("content-type", "application/json".to_string()),
        ],
        body: Some(body.to_string()),
    }
}

pub fn models_request(base_url: &str, credential: &Credential) -> HttpRequest {
    HttpRequest {
        // One page of up to 1000, the most the API allows. The default page is
        // 20, which would cut the list short with no sign of it.
        url: format!("{base_url}/v1/models?limit=1000"),
        headers: vec![auth_header(credential), ("anthropic-version", API_VERSION.to_string())],
        body: None,
    }
}

/// Read a Messages API response. Any status outside 2xx is an error; the error
/// text is the API's own `error.type` and `error.message`.
pub fn parse_reply(status: u16, body: &str) -> Reply {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Reply::failure(format!("{status}: {}", super::excerpt(body)));
    };
    // An error object is an error whatever the status says.
    if !(200..300).contains(&status) || v.get("type").and_then(Value::as_str) == Some("error") {
        return parse_error(status, &v, body);
    }
    let Some(blocks) = v.get("content").and_then(Value::as_array) else {
        return Reply::failure(format!("{status}: response is not a message: {}", super::excerpt(body)));
    };
    let str_of = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    let usage = v.get("usage");
    let get = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
    // A refusal is HTTP 200, so the stop reason is the only signal. So is a
    // cap reached before any text, which thinking can use up on its own.
    let stop = v.get("stop_reason").and_then(Value::as_str).unwrap_or_default();
    let refused = stop == "refusal";
    let capped_empty = stop == "max_tokens" && text.is_empty();
    Reply {
        text: match (refused, capped_empty, text.is_empty()) {
            (true, _, true) => "the model declined to answer (stop_reason: refusal)".to_string(),
            (_, true, _) => {
                "the answer reached the token cap before any text (stop_reason: max_tokens); raise --max-tokens"
                    .to_string()
            }
            _ => text,
        },
        model: str_of("model"),
        id: str_of("id"),
        usage: Usage {
            input_tokens: get("input_tokens"),
            output_tokens: get("output_tokens"),
            cache_read_input_tokens: get("cache_read_input_tokens"),
            cache_creation_input_tokens: get("cache_creation_input_tokens"),
        },
        is_error: refused || capped_empty,
        invalid_model: false,
    }
}

fn parse_error(status: u16, v: &Value, body: &str) -> Reply {
    let err = v.get("error");
    let kind = err.and_then(|e| e.get("type")).and_then(Value::as_str).unwrap_or_default();
    let message = err.and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or_default();
    if message.is_empty() {
        return Reply::failure(format!("{status}: {}", super::excerpt(body)));
    }
    // Anthropic says a model the caller cannot use with a 404
    // `not_found_error` whose message starts with `model:`, whether or not the
    // model exists. A compatible server says it in its own words.
    let anthropic_rejection = status == 404 && kind == "not_found_error" && message.starts_with("model:");
    Reply {
        invalid_model: anthropic_rejection || super::names_a_missing_model(status, message),
        ..Reply::failure(format!("{status} {kind}: {message}"))
    }
}

/// The model ids in a `GET /v1/models` response.
pub fn parse_models(body: &str) -> Result<Vec<String>, String> {
    super::model_ids(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(name, _)| *name == k).map(|(_, v)| v.to_string())
    }

    fn call<'a>(credential: &'a Credential, system: Option<&'a str>) -> Call<'a> {
        Call {
            base_url: "https://proxy.example.com",
            credential,
            model: "claude-opus-5",
            prompt: "say ok",
            system,
            max_tokens: None,
        }
    }

    #[test]
    fn a_bearer_token_wins_over_an_api_key_and_never_travels_with_it() {
        // A gateway setup sets ANTHROPIC_AUTH_TOKEN next to a global
        // ANTHROPIC_API_KEY. Sending the key would hand it to the gateway.
        let env = env_with(&[(API_KEY_ENV, "sk-ant-1"), (AUTH_TOKEN_ENV, "tok-1")]);
        let c = credential(None, &env).unwrap();
        let req = message_request(&call(&c, None));
        assert!(req.headers.contains(&("authorization", "Bearer tok-1".to_string())));
        assert!(!req.headers.iter().any(|(k, _)| *k == "x-api-key"));
    }

    #[test]
    fn an_api_key_is_used_when_there_is_no_bearer_token() {
        let env = env_with(&[(API_KEY_ENV, "sk-ant-1")]);
        let c = credential(None, &env).unwrap();
        let req = message_request(&call(&c, None));
        assert!(req.headers.contains(&("x-api-key", "sk-ant-1".to_string())));
        assert!(!req.headers.iter().any(|(k, _)| *k == "authorization"));
    }

    #[test]
    fn api_key_env_names_the_variable_and_fails_when_it_is_unset() {
        let env = env_with(&[("DEEPSEEK_API_KEY", "ds-1"), (API_KEY_ENV, "sk-ant-1")]);
        let c = credential(Some("DEEPSEEK_API_KEY"), &env).unwrap();
        assert_eq!(c.value, "ds-1");
        assert_eq!(c.source, "DEEPSEEK_API_KEY");
        let err = credential(Some("MISSING_KEY"), &env).unwrap_err();
        assert!(err.contains("MISSING_KEY"), "{err}");
    }

    #[test]
    fn no_credential_names_every_way_to_give_one() {
        let err = credential(None, &env_with(&[])).unwrap_err();
        for hint in [API_KEY_ENV, AUTH_TOKEN_ENV, "--api-key-env"] {
            assert!(err.contains(hint), "{err}");
        }
    }

    #[test]
    fn the_request_carries_the_prompt_version_and_required_max_tokens() {
        let c = Credential::api_key(API_KEY_ENV, "k".into());
        let req = message_request(&call(&c, Some("be brief")));
        assert_eq!(req.url, "https://proxy.example.com/v1/messages");
        assert!(req.headers.contains(&("anthropic-version", API_VERSION.to_string())));
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["messages"], json!([{ "role": "user", "content": "say ok" }]));
        assert_eq!(body["system"], "be brief");
        assert!(body.get("tools").is_none(), "the API harness never offers tools");

        let capped = Call { max_tokens: Some(512), ..call(&c, None) };
        let body: Value = serde_json::from_str(message_request(&capped).body.as_deref().unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 512);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn a_success_folds_text_model_id_and_usage() {
        let body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5","content":[{"type":"thinking","thinking":""},{"type":"text","text":"o"},{"type":"text","text":"k"}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":1}}"#;
        let r = parse_reply(200, body);
        assert_eq!(r.text, "ok");
        assert_eq!(r.model, "claude-opus-5");
        assert_eq!(r.id, "msg_1");
        assert_eq!(r.usage.input_tokens, 12);
        assert_eq!(r.usage.cache_read_input_tokens, 4);
        assert_eq!(r.usage.cache_creation_input_tokens, 1);
        assert!(!r.is_error);
    }

    #[test]
    fn a_200_that_is_not_a_message_is_an_error() {
        for body in ["{}", r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#] {
            let r = parse_reply(200, body);
            assert!(r.is_error, "{body}");
        }
        assert_eq!(
            parse_reply(200, r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#).text,
            "200 overloaded_error: busy"
        );
    }

    #[test]
    fn a_cap_reached_before_any_text_is_an_error() {
        let body = r#"{"id":"msg_3","model":"claude-opus-5","content":[{"type":"thinking","thinking":""}],"stop_reason":"max_tokens","usage":{"input_tokens":5,"output_tokens":16000}}"#;
        let r = parse_reply(200, body);
        assert!(r.is_error);
        assert!(r.text.contains("--max-tokens"), "{}", r.text);
        // With some text, a cut-off answer is still the answer.
        let partial = r#"{"id":"msg_4","model":"m","content":[{"type":"text","text":"half"}],"stop_reason":"max_tokens"}"#;
        assert!(!parse_reply(200, partial).is_error);
    }

    #[test]
    fn a_named_auth_token_variable_is_sent_as_a_bearer_token() {
        let env = env_with(&[(AUTH_TOKEN_ENV, "tok-1"), ("ZAI_AUTH_TOKEN", "zai-1")]);
        for var in [AUTH_TOKEN_ENV, "ZAI_AUTH_TOKEN"] {
            assert!(credential(Some(var), &env).unwrap().bearer, "{var}");
        }
        let env = env_with(&[("DEEPSEEK_API_KEY", "ds-1")]);
        assert!(!credential(Some("DEEPSEEK_API_KEY"), &env).unwrap().bearer);
    }

    #[test]
    fn the_model_listing_asks_for_one_full_page() {
        let c = Credential::api_key(API_KEY_ENV, "k".into());
        assert_eq!(models_request("https://api.anthropic.com", &c).url, "https://api.anthropic.com/v1/models?limit=1000");
    }

    #[test]
    fn a_refusal_is_an_error_even_on_http_200() {
        let body = r#"{"id":"msg_2","model":"claude-opus-5","content":[],"stop_reason":"refusal","usage":{"input_tokens":5,"output_tokens":0}}"#;
        let r = parse_reply(200, body);
        assert!(r.is_error);
        assert!(r.text.contains("refusal"), "{}", r.text);
        assert!(!r.invalid_model);
    }

    #[test]
    fn an_unusable_model_is_an_invalid_model() {
        let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-bogus"},"request_id":"req_1"}"#;
        let r = parse_reply(404, body);
        assert!(r.is_error);
        assert!(r.invalid_model);
        assert_eq!(r.text, "404 not_found_error: model: claude-bogus");
        assert_eq!(r.model, "", "a rejected model never ran");
    }

    #[test]
    fn a_compatible_server_s_model_rejection_is_recognised_by_its_words() {
        let vllm = r#"{"type":"error","error":{"type":"invalid_request_error","message":"The model `claude-x` does not exist."}}"#;
        assert!(parse_reply(404, vllm).invalid_model);
        let litellm = r#"{"error":{"message":"model claude-x not found","type":"invalid_request_error"}}"#;
        assert!(parse_reply(400, litellm).invalid_model);
        let overloaded = r#"{"type":"error","error":{"type":"overloaded_error","message":"the model is overloaded"}}"#;
        assert!(!parse_reply(529, overloaded).invalid_model);
    }

    #[test]
    fn other_api_errors_are_plain_errors() {
        for (status, kind) in [(401, "authentication_error"), (429, "rate_limit_error"), (529, "overloaded_error")] {
            let body = format!(r#"{{"type":"error","error":{{"type":"{kind}","message":"nope"}}}}"#);
            let r = parse_reply(status, &body);
            assert!(r.is_error && !r.invalid_model, "{status}");
            assert_eq!(r.text, format!("{status} {kind}: nope"));
        }
        // An unknown endpoint is also a 404, but its message does not name a model.
        let r = parse_reply(404, r#"{"type":"error","error":{"type":"not_found_error","message":"Not Found"}}"#);
        assert!(!r.invalid_model);
    }

    #[test]
    fn a_body_that_is_not_json_is_shown_as_it_came() {
        let r = parse_reply(502, "<html>Bad Gateway</html>");
        assert!(r.is_error);
        assert_eq!(r.text, "502: <html>Bad Gateway</html>");
    }

    #[test]
    fn models_come_from_the_data_array() {
        let body = r#"{"data":[{"id":"claude-opus-5","type":"model"},{"id":"claude-haiku-4-5","type":"model"}],"has_more":false}"#;
        assert_eq!(parse_models(body).unwrap(), vec!["claude-opus-5", "claude-haiku-4-5"]);
    }
}
