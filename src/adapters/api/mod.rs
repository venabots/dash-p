//! API adapters: call a model provider's HTTP API directly. One request, one
//! answer -- no agent loop, no tools, nothing to install but a key.
//!
//! A harness here is named for the wire format it speaks, not for a vendor:
//! `anthropic-api` is the Anthropic Messages API and `openai-api` is OpenAI
//! Chat Completions. Many providers conform to one of the two, so `--base-url`
//! (or the format's own base-URL variable) points either one at them.
//!
//! The model gets the prompt and nothing else. It cannot read files, run
//! commands, or reach the network, so every permission and network tier is held
//! by construction: `no-tools`. The flip side is that a prompt which assumes a
//! workspace ("audit src/") gets an answer from a model that cannot see one.

use std::io::Write;
use std::time::Instant;

use serde_json::Value;

use crate::adapters::{Adapter, DriverError, RunOutcome};
use crate::args::{Options, OutputFormat};
use crate::harness::Harness;
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
use crate::transcript::{Summary, Usage};

pub mod anthropic;
mod http;
pub mod openai;

/// The wire formats dash-p speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    Openai,
}

impl Protocol {
    pub fn for_harness(harness: &Harness) -> Option<Self> {
        match harness {
            Harness::AnthropicApi => Some(Self::Anthropic),
            Harness::OpenaiApi => Some(Self::Openai),
            _ => None,
        }
    }

    fn harness_name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic-api",
            Self::Openai => "openai-api",
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => anthropic::DEFAULT_BASE_URL,
            Self::Openai => openai::DEFAULT_BASE_URL,
        }
    }

    fn base_url_env(self) -> &'static str {
        match self {
            Self::Anthropic => anthropic::BASE_URL_ENV,
            Self::Openai => openai::BASE_URL_ENV,
        }
    }

    fn credential(
        self,
        api_key_env: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Credential, String> {
        match self {
            Self::Anthropic => anthropic::credential(api_key_env, env),
            Self::Openai => openai::credential(api_key_env, env),
        }
    }

    fn message_request(self, call: &Call) -> HttpRequest {
        match self {
            Self::Anthropic => anthropic::message_request(call),
            Self::Openai => openai::message_request(call),
        }
    }

    fn parse_reply(self, status: u16, body: &str) -> Reply {
        match self {
            Self::Anthropic => anthropic::parse_reply(status, body),
            Self::Openai => openai::parse_reply(status, body),
        }
    }

    fn models_request(self, base_url: &str, credential: &Credential) -> HttpRequest {
        match self {
            Self::Anthropic => anthropic::models_request(base_url, credential),
            Self::Openai => openai::models_request(base_url, credential),
        }
    }

    fn parse_models(self, body: &str) -> Result<Vec<String>, String> {
        match self {
            Self::Anthropic => anthropic::parse_models(body),
            Self::Openai => openai::parse_models(body),
        }
    }
}

/// A credential and the environment variable it came from. The value is never
/// printed; the source name is what diagnostics show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub source: String,
    pub value: String,
    /// Sent as `Authorization: Bearer`, rather than the format's key header.
    pub bearer: bool,
}

impl Credential {
    fn api_key(source: &str, value: String) -> Self {
        Self { source: source.to_string(), value, bearer: false }
    }

    fn bearer(source: &str, value: String) -> Self {
        Self { source: source.to_string(), value, bearer: true }
    }
}

/// Everything one message request needs, already resolved.
pub struct Call<'a> {
    base_url: &'a str,
    credential: &'a Credential,
    model: &'a str,
    prompt: &'a str,
    system: Option<&'a str>,
    max_tokens: Option<u32>,
}

/// One HTTP request, built by a protocol and sent by `http`.
#[derive(Debug, PartialEq, Eq)]
pub struct HttpRequest {
    url: String,
    headers: Vec<(&'static str, String)>,
    /// `Some` for a POST, `None` for a GET.
    body: Option<String>,
}

/// A response, read into the fields the envelope needs.
#[derive(Debug, Default, PartialEq)]
pub struct Reply {
    text: String,
    model: String,
    id: String,
    usage: Usage,
    is_error: bool,
    invalid_model: bool,
}

impl Reply {
    fn failure(text: String) -> Self {
        Self { text, is_error: true, ..Self::default() }
    }
}

/// Drives one API wire format.
pub struct ApiAdapter(pub Protocol);

impl Adapter for ApiAdapter {
    fn run(
        &self,
        opts: &Options,
        stream_out: Option<&mut dyn Write>,
    ) -> Result<RunOutcome, DriverError> {
        run(self.0, opts, &env_var, stream_out)
    }

    fn drive(&self) -> &'static str {
        "api"
    }

    fn perms_enforcement(&self, _perms: Perms) -> Enforcement {
        Enforcement::NoTools
    }

    fn network_plan(
        &self,
        _perms: Option<Perms>,
        _network: Network,
        _bypass: bool,
    ) -> Result<NetworkPlan, String> {
        // The model has no tools, so it has no network, whatever tier was asked
        // for. A bypass flag changes nothing: there is no sandbox to remove.
        Ok(NetworkPlan::no_tools())
    }
}

/// A non-empty environment variable.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `--base-url`, else the format's base-URL variable, else the vendor's own.
fn base_url(protocol: Protocol, opts: &Options, env: &dyn Fn(&str) -> Option<String>) -> String {
    opts.base_url
        .clone()
        .or_else(|| env(protocol.base_url_env()))
        .unwrap_or_else(|| protocol.default_base_url().to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The `--system-prompt` value, if the caller passed one. It sits in
/// `extra_args` because `args.rs` forwards it for claude.
fn system_prompt(opts: &Options) -> Option<&str> {
    opts.extra_args.iter().enumerate().find_map(|(i, arg)| {
        arg.strip_prefix("--system-prompt=").or_else(|| {
            (arg == "--system-prompt").then(|| opts.extra_args.get(i + 1).map(String::as_str)).flatten()
        })
    })
}

fn run(
    protocol: Protocol,
    opts: &Options,
    env: &dyn Fn(&str) -> Option<String>,
    mut stream_out: Option<&mut dyn Write>,
) -> Result<RunOutcome, DriverError> {
    let start = Instant::now();
    let harness = protocol.harness_name();

    // A raw API has no default model to fall back on, and guessing one would
    // be a silent default.
    let Some(model) = opts.model.as_deref() else {
        let reply = Reply {
            invalid_model: true,
            ..Reply::failure(format!("{harness} has no default model: pass --model <id>"))
        };
        return Ok(outcome(reply, String::new(), 0, start));
    };
    let credential = protocol
        .credential(opts.api_key_env.as_deref(), env)
        .map_err(|why| DriverError::Setup(format!("{harness}: {why}")))?;
    let base_url = base_url(protocol, opts, env);

    let request = protocol.message_request(&Call {
        base_url: &base_url,
        credential: &credential,
        model,
        prompt: &opts.prompt,
        system: system_prompt(opts),
        max_tokens: opts.max_tokens,
    });
    let url = request.url.clone();
    let sent = Instant::now();
    let (reply, replay) = match http::send(request, opts)? {
        http::Outcome::Response { status, body } => (protocol.parse_reply(status, &body), one_line(&body)),
        http::Outcome::Failed(why) => {
            (Reply::failure(format!("{harness}: request to {url} failed: {why}")), String::new())
        }
    };
    let api_ms = sent.elapsed().as_millis() as u64;

    if opts.output_format == OutputFormat::StreamJson
        && let Some(w) = stream_out.as_mut()
    {
        let _ = w.write_all(replay.as_bytes());
        let _ = w.flush();
    }
    finish(opts, stream_out, outcome(reply, replay, api_ms, start))
}

/// Write the trailing `result` envelope under stream-json.
fn finish(
    opts: &Options,
    stream_out: Option<&mut dyn Write>,
    mut out: RunOutcome,
) -> Result<RunOutcome, DriverError> {
    if opts.output_format == OutputFormat::StreamJson
        && let Some(w) = stream_out
    {
        crate::emit::emit_result_envelope(w, &out.summary, out.duration_ms).map_err(DriverError::Io)?;
        let _ = w.flush();
        out.streamed = true;
    }
    Ok(out)
}

fn outcome(reply: Reply, replay: String, api_ms: u64, start: Instant) -> RunOutcome {
    RunOutcome {
        summary: Summary {
            final_text: reply.text,
            session_id: reply.id,
            model: reply.model,
            is_error: reply.is_error,
            num_turns: 1,
            total_cost_usd: 0.0,
            duration_api_ms: api_ms,
            usage: reply.usage,
            jsonl_replay: replay,
        },
        duration_ms: start.elapsed().as_millis() as u64,
        streamed: false,
        invalid_model: reply.invalid_model,
    }
}

/// The response body as one JSONL line: compact JSON when it parses, else the
/// text as a JSON string, so stream-json stays one object per line.
fn one_line(body: &str) -> String {
    let value = serde_json::from_str::<Value>(body).unwrap_or_else(|_| Value::String(body.to_string()));
    format!("{value}\n")
}

/// The start of a body that is not the JSON an API promised, for an error.
fn excerpt(body: &str) -> String {
    const MAX: usize = 500;
    let trimmed = body.trim();
    match trimmed.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}...", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// The ids in a models listing. Both formats use `{"data":[{"id":...}]}`.
fn model_ids(body: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(body).map_err(|_| format!("unexpected response: {}", excerpt(body)))?;
    v.get("data")
        .and_then(Value::as_array)
        .map(|models| models.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).map(str::to_string).collect())
        .ok_or_else(|| format!("unexpected response: {}", excerpt(body)))
}

/// Which credential a run would use, for `list harnesses`: `Ok(source)` or
/// `Err(hint)`. Never the value itself.
pub fn credential_status(protocol: Protocol) -> Result<String, String> {
    protocol.credential(None, &env_var).map(|c| c.source)
}

/// `list models`: the models the provider says this credential can use.
pub fn list_models(protocol: Protocol) -> Result<Vec<String>, String> {
    let credential = protocol.credential(None, &env_var)?;
    let base_url = base_url(protocol, &Options::default(), &env_var);
    let body = http::get(protocol.models_request(&base_url, &credential))?;
    protocol.parse_models(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::api::http::tests::serve_once;

    fn env_with(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(name, _)| *name == k).map(|(_, v)| v.clone())
    }

    fn opts(base_url: &str) -> Options {
        Options {
            prompt: "say ok".into(),
            model: Some("claude-opus-5".into()),
            base_url: Some(base_url.into()),
            timeout_ms: 5_000,
            ..Options::default()
        }
    }

    const OK_BODY: &str = r#"{"id":"msg_1","model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":2}}"#;

    #[test]
    fn base_url_prefers_the_flag_then_the_variable_then_the_vendor() {
        let env = env_with(vec![(anthropic::BASE_URL_ENV, "https://env.example.com/".into())]);
        let flag = Options { base_url: Some("http://127.0.0.1:9/".into()), ..Options::default() };
        assert_eq!(base_url(Protocol::Anthropic, &flag, &env), "http://127.0.0.1:9");
        assert_eq!(base_url(Protocol::Anthropic, &Options::default(), &env), "https://env.example.com");
        assert_eq!(base_url(Protocol::Anthropic, &Options::default(), &env_with(vec![])), anthropic::DEFAULT_BASE_URL);
        assert_eq!(base_url(Protocol::Openai, &Options::default(), &env_with(vec![])), openai::DEFAULT_BASE_URL);
    }

    #[test]
    fn system_prompt_reads_both_flag_forms() {
        let spaced = Options { extra_args: vec!["--system-prompt".into(), "be brief".into()], ..Options::default() };
        assert_eq!(system_prompt(&spaced), Some("be brief"));
        let inline = Options { extra_args: vec!["--system-prompt=be brief".into()], ..Options::default() };
        assert_eq!(system_prompt(&inline), Some("be brief"));
        assert_eq!(system_prompt(&Options::default()), None);
    }

    #[test]
    fn a_run_sends_one_request_and_folds_the_answer() {
        let (base, request) = serve_once(200, OK_BODY);
        let env = env_with(vec![(anthropic::API_KEY_ENV, "sk-test".into())]);
        let out = run(Protocol::Anthropic, &opts(&base), &env, None).unwrap();
        assert_eq!(out.summary.final_text, "ok");
        assert_eq!(out.summary.model, "claude-opus-5");
        assert_eq!(out.summary.session_id, "msg_1");
        assert_eq!(out.summary.usage.input_tokens, 12);
        assert!(!out.summary.is_error && !out.invalid_model);

        let raw = request.recv().unwrap();
        assert!(raw.starts_with("POST /v1/messages "), "{raw}");
        assert!(raw.to_ascii_lowercase().contains("x-api-key: sk-test"), "{raw}");
        assert!(raw.contains(r#""content":"say ok""#), "{raw}");
    }

    #[test]
    fn an_openai_run_posts_a_chat_completion_with_a_bearer_key() {
        let body = r#"{"id":"chatcmpl-1","model":"gpt-5.5-2026-04-01","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":5}}}"#;
        let (base, request) = serve_once(200, body);
        let env = env_with(vec![(openai::API_KEY_ENV, "sk-test".into())]);
        let o = Options { base_url: Some(format!("{base}/v1")), model: Some("gpt-5.5".into()), ..opts(&base) };
        let out = run(Protocol::Openai, &o, &env, None).unwrap();
        assert_eq!(out.summary.final_text, "ok");
        assert_eq!(out.summary.model, "gpt-5.5-2026-04-01");
        assert_eq!(out.summary.usage.input_tokens, 6);

        let raw = request.recv().unwrap();
        assert!(raw.starts_with("POST /v1/chat/completions "), "{raw}");
        assert!(raw.to_ascii_lowercase().contains("authorization: bearer sk-test"), "{raw}");
    }

    #[test]
    fn a_rejected_model_maps_to_invalid_model() {
        let (base, _) = serve_once(404, r#"{"type":"error","error":{"type":"not_found_error","message":"model: nope"}}"#);
        let env = env_with(vec![(anthropic::API_KEY_ENV, "sk-test".into())]);
        let out = run(Protocol::Anthropic, &opts(&base), &env, None).unwrap();
        assert!(out.summary.is_error && out.invalid_model);
        assert_eq!(out.summary.final_text, "404 not_found_error: model: nope");
    }

    #[test]
    fn no_model_is_an_invalid_model_before_any_request() {
        let o = Options { model: None, base_url: Some("http://127.0.0.1:9".into()), ..Options::default() };
        let out = run(Protocol::Anthropic, &o, &env_with(vec![]), None).unwrap();
        assert!(out.invalid_model);
        assert!(out.summary.final_text.contains("--model"), "{}", out.summary.final_text);
    }

    #[test]
    fn no_credential_is_a_setup_error_before_any_request() {
        let err = run(Protocol::Anthropic, &opts("http://127.0.0.1:9"), &env_with(vec![]), None).err().unwrap();
        assert!(matches!(err, DriverError::Setup(_)), "got: {err}");
        assert_eq!(err.status().code(), 2);
    }

    #[test]
    fn a_connection_failure_is_an_agent_error_that_names_the_url() {
        // Port 9 (discard) is closed on a normal machine, so connect is refused.
        let env = env_with(vec![(anthropic::API_KEY_ENV, "sk-test".into())]);
        let out = run(Protocol::Anthropic, &opts("http://127.0.0.1:9"), &env, None).unwrap();
        assert!(out.summary.is_error && !out.invalid_model);
        assert!(out.summary.final_text.contains("http://127.0.0.1:9/v1/messages"), "{}", out.summary.final_text);
        assert!(!out.summary.final_text.contains("sk-test"), "the key leaked into the error");
    }

    #[test]
    fn stream_json_writes_the_response_line_then_the_result() {
        let (base, _) = serve_once(200, OK_BODY);
        let env = env_with(vec![(anthropic::API_KEY_ENV, "sk-test".into())]);
        let o = Options { output_format: OutputFormat::StreamJson, ..opts(&base) };
        let mut buf = Vec::new();
        let out = run(Protocol::Anthropic, &o, &env, Some(&mut buf)).unwrap();
        assert!(out.streamed);
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0]["id"], "msg_1");
        assert_eq!(lines[1]["type"], "result");
    }

    #[test]
    fn every_tier_is_held_by_having_no_tools() {
        let adapter = ApiAdapter(Protocol::Openai);
        for perms in [Perms::ReadOnly, Perms::WorkspaceWrite, Perms::Full] {
            assert_eq!(adapter.perms_enforcement(perms), Enforcement::NoTools);
        }
        for network in [Network::None, Network::Restricted, Network::Full] {
            for bypass in [false, true] {
                assert_eq!(adapter.network_plan(None, network, bypass).unwrap(), NetworkPlan::no_tools());
            }
        }
    }

    #[test]
    fn excerpt_caps_long_bodies_on_a_char_boundary() {
        let long = "é".repeat(600);
        let cut = excerpt(&long);
        assert!(cut.ends_with("..."));
        assert_eq!(cut.chars().count(), 503);
    }
}
