//! pi adapter: drive `pi -p --mode json`, pi's natively non-interactive print
//! mode, through the shared JSONL subprocess loop (see `exec`).
//!
//! The prompt goes on stdin, never as a positional argument: pi reads any
//! argument that starts with `@` as a file to attach -- even after `--` -- so a
//! prompt such as "@src/main.rs looks wrong" would silently become a file read.
//!
//! The event stream is complete enough for an honest envelope. Every assistant
//! `message_end` carries the provider, the model the provider reported
//! (`responseModel`), per-call usage and cost, and a `stopReason`. pi exits 0 in
//! JSON mode even when the model call failed, so the error state comes from the
//! last assistant message, not from the exit status alone.
//!
//! pi has no sandbox, by design. `--perms read-only` narrows pi's tool allowlist
//! to its read-only tools, which is agent-policy; the write tiers and every
//! network tier are unenforced.

use std::io::Write;
use std::process::Command;
use std::time::Instant;

use serde_json::Value;

use crate::adapters::exec;
use crate::adapters::{Adapter, DriverError, RunOutcome};
use crate::args::Options;
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
use crate::transcript::{Summary, Usage};

/// pi's read-only built-in tools. `--tools` is an allowlist that also filters
/// extension and custom tools, so nothing that writes or runs commands is left.
const READ_ONLY_TOOLS: &str = "read,grep,find,ls";

/// Drives the `pi` CLI via its non-interactive JSON print mode.
pub struct PiAdapter;

impl Adapter for PiAdapter {
    fn run(
        &self,
        opts: &Options,
        stream_out: Option<&mut dyn Write>,
    ) -> Result<RunOutcome, DriverError> {
        run(opts, stream_out)
    }

    fn drive(&self) -> &'static str {
        "exec"
    }

    fn perms_enforcement(&self, perms: Perms) -> Enforcement {
        match perms {
            // The model is only offered read-only tools. pi itself still runs
            // with the user's permissions, so this is policy, not a sandbox.
            Perms::ReadOnly => Enforcement::AgentPolicy,
            Perms::WorkspaceWrite | Perms::Full => Enforcement::Unenforced,
        }
    }

    fn network_plan(
        &self,
        _perms: Option<Perms>,
        _network: Network,
        _bypass: bool,
    ) -> Result<NetworkPlan, String> {
        // pi has no network control. Every tier is accepted -- callers pass one
        // tier across mixed harnesses -- and reported as the open network the
        // run really gets.
        Ok(NetworkPlan::open())
    }
}

/// Accumulated state folded from the `pi --mode json` event stream.
#[derive(Debug, Default, PartialEq)]
struct Folded {
    session_id: String,
    /// At least one assistant message ended. A run without one produced no
    /// answer at all, which is a failure even when pi exits 0.
    saw_assistant: bool,
    /// Text of the last assistant message. Last one wins: after a tool round
    /// or an auto-retry, only the final message is the answer.
    final_text: String,
    /// `provider/model` of the last assistant message.
    model: String,
    usage: Usage,
    total_cost_usd: f64,
    /// Number of `turn_end` events (model calls, retries included).
    num_turns: u32,
    is_error: bool,
    /// pi's own error text for the last assistant message.
    error_message: String,
    /// The error looks like the provider rejecting the requested model.
    invalid_model: bool,
}

/// Fold one event line into the running state. Unknown lines are ignored, so a
/// new pi event type never breaks the parse.
fn fold_event(state: &mut Folded, line: &str) {
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("session") => {
            if state.session_id.is_empty()
                && let Some(id) = obj.get("id").and_then(Value::as_str)
            {
                state.session_id = id.to_string();
            }
        }
        Some("turn_end") => state.num_turns += 1,
        Some("message_end") => {
            if let Some(msg) = obj.get("message")
                && msg.get("role").and_then(Value::as_str) == Some("assistant")
            {
                fold_assistant(state, msg);
            }
        }
        _ => {}
    }
}

fn fold_assistant(state: &mut Folded, msg: &Value) {
    let str_of = |k: &str| msg.get(k).and_then(Value::as_str).unwrap_or_default();
    state.saw_assistant = true;

    // Each assistant message reports the usage of its own model call, so the
    // run total is the sum.
    if let Some(u) = msg.get("usage") {
        let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let total = &mut state.usage;
        total.input_tokens = total.input_tokens.saturating_add(get("input"));
        total.output_tokens = total.output_tokens.saturating_add(get("output"));
        total.cache_read_input_tokens = total.cache_read_input_tokens.saturating_add(get("cacheRead"));
        total.cache_creation_input_tokens =
            total.cache_creation_input_tokens.saturating_add(get("cacheWrite"));
        if let Some(cost) = u.get("cost").and_then(|c| c.get("total")).and_then(Value::as_f64) {
            state.total_cost_usd += cost;
        }
    }

    // `responseModel` is what the provider says ran; `model` is only the id pi
    // asked for, so it is the fallback.
    let model = Some(str_of("responseModel"))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| str_of("model"));
    state.model = match (str_of("provider"), model) {
        (_, "") => String::new(),
        ("", m) => m.to_string(),
        (p, m) => format!("{p}/{m}"),
    };

    state.final_text = msg
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    let stop = str_of("stopReason");
    state.is_error = matches!(stop, "error" | "aborted");
    state.error_message = if state.is_error {
        Some(str_of("errorMessage"))
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("pi request {stop}"))
    } else {
        String::new()
    };
    state.invalid_model = state.is_error && looks_like_model_error(&state.error_message);
}

/// Heuristic: does an error message say the model was rejected? Matched against
/// the text pi relays: its own `Model "x" not found`, an OpenAI-style "does not
/// exist", or Anthropic's `not_found_error`, so exit 31 reflects a live verdict.
fn looks_like_model_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("model")
        && (m.contains("not found")
            || m.contains("not_found")
            || m.contains("does not exist")
            || m.contains("not supported")
            || m.contains("unknown model")
            || m.contains("invalid model"))
}

/// Whether pi refused the model before any call: it prints `Error: Model "x"
/// not found` to stderr and exits 1. Only `Error:` lines count -- pi also warns
/// `Warning: Model "x" not found for provider "p"` and then runs anyway.
fn stderr_rejects_model(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.trim_start().starts_with("Error:") && looks_like_model_error(l))
}

fn build_argv(opts: &Options) -> Vec<String> {
    let mut v = vec!["-p".to_string(), "--mode".to_string(), "json".to_string()];
    if let Some(m) = &opts.model {
        v.push("--model".to_string());
        v.push(m.clone());
    }
    // Kept under a bypass too, like claude's read-only denial list: the
    // envelope already reports the bypass as unenforced, and fewer tools is the
    // safe direction.
    if opts.perms == Some(Perms::ReadOnly) {
        v.push("--tools".to_string());
        v.push(READ_ONLY_TOOLS.to_string());
    }
    v
}

fn run(opts: &Options, mut stream_out: Option<&mut dyn Write>) -> Result<RunOutcome, DriverError> {
    let start = Instant::now();
    let mut cmd = Command::new("pi");
    cmd.args(build_argv(opts));
    if let Some(cwd) = &opts.cwd {
        cmd.current_dir(cwd);
    }

    let mut folded = Folded::default();
    let done = exec::run_jsonl(
        cmd,
        Some(opts.prompt.clone()),
        opts,
        stream_out.as_deref_mut(),
        |line| fold_event(&mut folded, line),
    )?;

    // A non-zero exit with no errored message is a failure before the first
    // model call (no credentials, unknown model); a clean exit with no
    // assistant message produced no answer. pi's stderr says why in both cases.
    if !done.success || !folded.saw_assistant {
        folded.is_error = true;
    }
    let final_text = if !folded.final_text.is_empty() {
        folded.final_text
    } else if !folded.error_message.is_empty() {
        folded.error_message
    } else if folded.is_error {
        let tail = done.stderr_tail();
        folded.invalid_model = stderr_rejects_model(&tail);
        if tail.is_empty() { "pi produced no assistant message".to_string() } else { tail }
    } else {
        String::new()
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let summary = Summary {
        final_text,
        session_id: folded.session_id,
        model: folded.model,
        is_error: folded.is_error,
        num_turns: folded.num_turns.max(1),
        total_cost_usd: folded.total_cost_usd,
        duration_api_ms: 0,
        usage: folded.usage,
        jsonl_replay: done.replay,
    };
    let streamed = exec::finish_stream(opts, stream_out, &summary, duration_ms)?;

    Ok(RunOutcome {
        summary,
        duration_ms,
        streamed,
        invalid_model: folded.invalid_model,
    })
}

/// `pi --list-models`: the models pi can use right now (only providers with
/// credentials are listed). `None` when pi is missing or the listing fails.
pub fn list_models() -> Option<String> {
    let out = Command::new("pi").args(["--list-models", "--offline"]).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Event lines captured from `pi 0.85.1 -p --mode json` against a local
     * OpenAI-compatible server, trimmed to the fields the fold reads. */
    const SESSION: &str = r#"{"type":"session","version":3,"id":"01a0a4dd-a124","timestamp":"2026-09-15T11:39:43.269Z","cwd":"/w"}"#;
    const USER_END: &str = r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"Reply ok"}]}}"#;
    const TOOL_CALL_END: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"hello.txt"}}],"api":"openai-completions","provider":"fake","model":"fake-tool","usage":{"input":6,"output":3,"cacheRead":5,"cacheWrite":0,"totalTokens":14,"cost":{"total":0.001}},"stopReason":"toolUse","responseModel":"fake-tool-2026-01-01"}}"#;
    const ANSWER_END: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"api":"openai-completions","provider":"fake","model":"fake-ok","usage":{"input":6,"output":3,"cacheRead":5,"cacheWrite":1,"totalTokens":14,"cost":{"input":0.000006,"output":0.000006,"cacheRead":0.0000025,"cacheWrite":0,"total":0.0000145}},"stopReason":"stop","responseModel":"fake-ok-2026-01-01"}}"#;
    const MISSING_MODEL_END: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[],"provider":"fake","model":"fake-missing","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}},"stopReason":"error","errorMessage":"404: {\"message\":\"The model `fake-missing` does not exist or you do not have access to it.\",\"type\":\"invalid_request_error\",\"code\":\"model_not_found\"}"}}"#;
    const SERVER_ERROR_END: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[],"provider":"fake","model":"fake-boom","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}},"stopReason":"error","errorMessage":"500: {\"message\":\"upstream exploded\",\"type\":\"server_error\"}"}}"#;
    const TURN_END: &str = r#"{"type":"turn_end","message":{"role":"assistant"},"toolResults":[]}"#;

    fn fold(lines: &[&str]) -> Folded {
        let mut f = Folded::default();
        for l in lines {
            fold_event(&mut f, l);
        }
        f
    }

    #[test]
    fn folds_a_plain_answer_into_the_envelope_fields() {
        let f = fold(&[SESSION, USER_END, ANSWER_END, TURN_END]);
        assert_eq!(f.final_text, "ok");
        assert_eq!(f.session_id, "01a0a4dd-a124");
        assert_eq!(f.model, "fake/fake-ok-2026-01-01");
        assert_eq!(f.usage.input_tokens, 6);
        assert_eq!(f.usage.output_tokens, 3);
        assert_eq!(f.usage.cache_read_input_tokens, 5);
        assert_eq!(f.usage.cache_creation_input_tokens, 1);
        assert!((f.total_cost_usd - 0.0000145).abs() < 1e-12);
        assert_eq!(f.num_turns, 1);
        assert!(f.saw_assistant);
        assert!(!f.is_error);
    }

    #[test]
    fn a_tool_round_sums_usage_and_the_last_message_is_the_answer() {
        let f = fold(&[SESSION, USER_END, TOOL_CALL_END, TURN_END, ANSWER_END, TURN_END]);
        assert_eq!(f.final_text, "ok");
        assert_eq!(f.num_turns, 2);
        assert_eq!(f.usage.input_tokens, 12);
        assert_eq!(f.usage.cache_read_input_tokens, 10);
        assert!((f.total_cost_usd - 0.0010145).abs() < 1e-12);
    }

    #[test]
    fn an_errored_last_message_is_an_error_with_pi_s_text() {
        let f = fold(&[SESSION, USER_END, SERVER_ERROR_END, TURN_END]);
        assert!(f.is_error);
        assert!(f.error_message.contains("upstream exploded"), "{}", f.error_message);
        assert!(!f.invalid_model);
        assert_eq!(f.final_text, "");
    }

    #[test]
    fn a_successful_retry_clears_the_earlier_error() {
        // pi retries a failed call itself; only the final attempt decides.
        let f = fold(&[SESSION, USER_END, SERVER_ERROR_END, TURN_END, ANSWER_END, TURN_END]);
        assert!(!f.is_error);
        assert!(f.error_message.is_empty());
        assert_eq!(f.final_text, "ok");
        assert_eq!(f.num_turns, 2);
    }

    #[test]
    fn a_provider_model_rejection_flags_invalid_model() {
        let f = fold(&[SESSION, USER_END, MISSING_MODEL_END, TURN_END]);
        assert!(f.is_error);
        assert!(f.invalid_model);
        // No responseModel on a failed call: fall back to the requested id.
        assert_eq!(f.model, "fake/fake-missing");
    }

    #[test]
    fn an_aborted_message_without_text_still_explains_itself() {
        let f = fold(&[r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"aborted"}}"#]);
        assert!(f.is_error);
        assert_eq!(f.error_message, "pi request aborted");
    }

    #[test]
    fn anthropic_style_model_errors_are_recognised() {
        assert!(looks_like_model_error(
            r#"404 {"type":"error","error":{"type":"not_found_error","message":"model: claude-bogus"}}"#
        ));
        assert!(!looks_like_model_error("500: upstream exploded"));
    }

    #[test]
    fn only_stderr_errors_count_as_a_model_rejection() {
        assert!(stderr_rejects_model(
            r#"Error: Model "bogus/xyz" not found. Use --list-models to see available models."#
        ));
        // pi warns and then runs the custom id, so the warning is not a verdict.
        assert!(!stderr_rejects_model(
            r#"Warning: Model "nope" not found for provider "fake". Using custom model id."#
        ));
        assert!(!stderr_rejects_model("No API key found for anthropic."));
    }

    #[test]
    fn malformed_unknown_and_non_assistant_lines_are_ignored() {
        let f = fold(&["not json", r#"{"type":"agent_settled"}"#, USER_END]);
        assert_eq!(f, Folded::default());
    }

    #[test]
    fn build_argv_uses_json_print_mode_and_never_carries_the_prompt() {
        let opts = Options {
            prompt: "@src/main.rs looks wrong".into(),
            model: Some("anthropic/claude-sonnet-4-5:high".into()),
            ..Options::default()
        };
        let v = build_argv(&opts);
        assert_eq!(&v[..3], ["-p", "--mode", "json"]);
        assert!(v.windows(2).any(|w| w == ["--model", "anthropic/claude-sonnet-4-5:high"]));
        assert!(!v.iter().any(|a| a.contains("looks wrong")), "prompt leaked into argv: {v:?}");
        assert!(!v.contains(&"--tools".to_string()));
    }

    #[test]
    fn read_only_narrows_the_tool_allowlist_even_under_a_bypass() {
        for skip_permissions in [false, true] {
            let opts = Options {
                perms: Some(Perms::ReadOnly),
                skip_permissions,
                ..Options::default()
            };
            assert!(build_argv(&opts).windows(2).any(|w| w == ["--tools", READ_ONLY_TOOLS]));
        }
        for tool in ["bash", "edit", "write"] {
            assert!(!READ_ONLY_TOOLS.contains(tool), "{tool} can change the machine");
        }
    }

    #[test]
    fn write_tiers_leave_pi_s_default_tools() {
        for perms in [Perms::WorkspaceWrite, Perms::Full] {
            let opts = Options { perms: Some(perms), ..Options::default() };
            assert!(!build_argv(&opts).contains(&"--tools".to_string()));
        }
    }

    #[test]
    fn enforcement_is_policy_for_read_only_and_nothing_else() {
        assert_eq!(PiAdapter.perms_enforcement(Perms::ReadOnly), Enforcement::AgentPolicy);
        assert_eq!(PiAdapter.perms_enforcement(Perms::WorkspaceWrite), Enforcement::Unenforced);
        assert_eq!(PiAdapter.perms_enforcement(Perms::Full), Enforcement::Unenforced);
    }

    #[test]
    fn every_network_tier_is_accepted_and_reported_as_an_open_network() {
        for network in [Network::None, Network::Restricted, Network::Full] {
            let plan = PiAdapter.network_plan(Some(Perms::ReadOnly), network, false).unwrap();
            assert_eq!(plan, NetworkPlan::open());
        }
    }
}
