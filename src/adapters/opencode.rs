//! Opencode adapter: drive `opencode run --format json`, which is natively
//! non-interactive, so like the codex adapter there is no PTY/hook/DEC
//! machinery. We spawn a plain subprocess with the prompt as a trailing
//! positional argument (after `--`, so a prompt beginning with `-` is never
//! mistaken for a flag) and read the JSON event stream on stdout.
//!
//! The event stream exposes the answer (`text` parts), token usage and cost
//! (`step_finish`), and the session id (`sessionID`), but *not* the model, and
//! opencode has no OS-level sandbox. This adapter also passes nothing that
//! enforces a read-only / workspace-write policy on opencode -- so
//! `model_resolved` is reported as `"unknown"` and enforcement is always
//! `none` (never agent-policy or os-sandbox). Reporting that honestly is the
//! point; see `perms_enforcement`.

use std::io::Write;
use std::process::Command;
use std::time::Instant;

use serde_json::Value;

use crate::adapters::exec;
use crate::adapters::{Adapter, DriverError, RunOutcome};
use crate::args::Options;
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
use crate::transcript::{Summary, Usage};

/// Drives the `opencode` CLI via its non-interactive `run` subcommand.
pub struct OpencodeAdapter;

impl Adapter for OpencodeAdapter {
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

    fn perms_enforcement(&self, _perms: Perms) -> Enforcement {
        // opencode has no OS sandbox, and this adapter passes nothing that
        // enforces a read-only / workspace-write policy on opencode, so we
        // cannot honestly claim *any* enforcement. Reporting agent-policy here
        // would let `--require-enforcement any --perms read-only` pass preflight
        // while opencode runs with the user's normal write-capable agent.
        Enforcement::Unenforced
    }

    fn network_plan(
        &self,
        _perms: Option<Perms>,
        _network: Network,
        _bypass: bool,
    ) -> Result<NetworkPlan, String> {
        // opencode does not sandbox network at all. Every tier is still
        // accepted -- callers pass one tier across mixed harnesses -- but the
        // run gets an open network, so the envelope reports
        // `network_effective: "full"` with `network_enforcement: "none"` rather
        // than echoing a tier nothing held.
        Ok(NetworkPlan::open())
    }
}

/// Accumulated state folded from the opencode `--format json` event stream.
#[derive(Debug, Default, PartialEq)]
struct Folded {
    session_id: String,
    /// `(part id, latest text)` in first-seen order. A streamed part is
    /// re-emitted with a longer `text`, so we keep the latest value per id;
    /// parts with no id are distinct fragments and are always appended.
    parts: Vec<(String, String)>,
    usage: Usage,
    total_cost_usd: f64,
    /// Number of `step_finish` events (tool-call rounds), reported as num_turns.
    num_turns: u32,
    is_error: bool,
    /// The harness's own error text, surfaced to the user on failure.
    error_message: String,
    /// The error looks like opencode rejecting the requested model.
    invalid_model: bool,
}

impl Folded {
    fn final_text(&self) -> String {
        self.parts.iter().map(|(_, t)| t.as_str()).collect()
    }
}

/// Fold one event line into the running state. Unknown lines are ignored, so a
/// new opencode event type never breaks the parse.
fn fold_event(state: &mut Folded, line: &str) {
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if state.session_id.is_empty()
        && let Some(id) = obj.get("sessionID").and_then(Value::as_str)
    {
        state.session_id = id.to_string();
    }
    let Some(ty) = obj.get("type").and_then(Value::as_str) else {
        return;
    };
    match ty {
        "text" => {
            if let Some(part) = obj.get("part") {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match part.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    Some(id) => match state.parts.iter_mut().find(|(pid, _)| pid == id) {
                        Some(slot) => slot.1 = text,
                        None => state.parts.push((id.to_string(), text)),
                    },
                    None => state.parts.push((String::new(), text)),
                }
            }
        }
        "step_finish" => {
            // Each step_finish reports that step's usage (per-call deltas, not
            // running totals -- verified: a later step's `input` is far smaller
            // than the first), so accumulate across steps to get the run total,
            // matching how cost is summed below.
            state.num_turns += 1;
            if let Some(part) = obj.get("part") {
                if let Some(tok) = part.get("tokens") {
                    let get = |k: &str| tok.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let cache = tok.get("cache");
                    let cache_get =
                        |k: &str| cache.and_then(|c| c.get(k)).and_then(Value::as_u64).unwrap_or(0);
                    let u = &mut state.usage;
                    u.input_tokens = u.input_tokens.saturating_add(get("input"));
                    u.output_tokens = u.output_tokens.saturating_add(get("output"));
                    u.cache_read_input_tokens =
                        u.cache_read_input_tokens.saturating_add(cache_get("read"));
                    u.cache_creation_input_tokens =
                        u.cache_creation_input_tokens.saturating_add(cache_get("write"));
                }
                if let Some(c) = part.get("cost").and_then(Value::as_f64) {
                    state.total_cost_usd += c;
                }
            }
        }
        // A terminal error event (opencode emits `{"type":"error", ...}`) marks
        // the run as errored. Match exactly rather than by substring so a
        // benign event whose name merely contains "error" can't latch failure.
        "error" => {
            state.is_error = true;
            let msg = obj
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| {
                    obj.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    obj.get("error")
                        .and_then(|e| e.get("data"))
                        .and_then(|d| d.get("message"))
                        .and_then(Value::as_str)
                })
                .unwrap_or_default();
            if !msg.is_empty() {
                state.error_message = msg.to_string();
            }
            if looks_like_model_error(msg) {
                state.invalid_model = true;
            }
        }
        _ => {}
    }
}

/// Heuristic: does an opencode error message indicate the model was rejected?
/// Matched against opencode's own error text so exit 31 reflects its live
/// verdict. Mirrors the codex/claude adapters' detection.
fn looks_like_model_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("model")
        && (m.contains("not supported")
            || m.contains("not found")
            || m.contains("does not exist")
            || m.contains("unknown model")
            || m.contains("invalid model")
            || m.contains("no such model"))
}

fn build_argv(opts: &Options) -> Vec<String> {
    let mut v = vec!["run".to_string(), "--format".to_string(), "json".to_string()];
    if let Some(m) = &opts.model {
        v.push("--model".to_string());
        v.push(m.clone());
    }
    if let Some(cwd) = &opts.cwd {
        v.push("--dir".to_string());
        v.push(cwd.clone());
    }
    // opencode has no OS sandbox tiers; `--auto` auto-approves every tool.
    // Map it from an explicit bypass or the `full` perms tier only -- the
    // read-only / workspace-write tiers keep opencode's default gating.
    if opts.skip_permissions || opts.perms == Some(Perms::Full) {
        v.push("--auto".to_string());
    }
    // `--` terminates option parsing; the prompt is the trailing positional.
    v.push("--".to_string());
    v.push(opts.prompt.clone());
    v
}

fn run(opts: &Options, mut stream_out: Option<&mut dyn Write>) -> Result<RunOutcome, DriverError> {
    let start = Instant::now();
    let mut cmd = Command::new("opencode");
    cmd.args(build_argv(opts));

    let mut folded = Folded::default();
    let done = exec::run_jsonl(cmd, None, opts, stream_out.as_deref_mut(), |line| {
        fold_event(&mut folded, line)
    })?;
    if !done.success {
        folded.is_error = true;
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    // On failure with no answer, surface opencode's own error text -- prefer a
    // structured `error` event's message, then fall back to the stderr tail --
    // so the user sees why instead of an empty answer.
    let final_text = if !folded.parts.is_empty() {
        folded.final_text()
    } else if !folded.error_message.is_empty() {
        folded.error_message.clone()
    } else if folded.is_error {
        done.stderr_tail()
    } else {
        String::new()
    };

    let summary = Summary {
        final_text,
        session_id: folded.session_id,
        // opencode does not expose the resolved model in its event stream, so
        // report "unknown" rather than echoing the requested model.
        model: String::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_stream_into_answer_usage_session() {
        let lines = [
            r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
            r#"{"type":"text","sessionID":"ses_1","part":{"id":"prt_a","type":"text","text":"hello"}}"#,
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"type":"step-finish","tokens":{"input":9044,"output":4,"cache":{"write":0,"read":7168}},"cost":0.0012}}"#,
        ];
        let mut f = Folded::default();
        for l in lines {
            fold_event(&mut f, l);
        }
        assert_eq!(f.final_text(), "hello");
        assert_eq!(f.session_id, "ses_1");
        assert_eq!(f.usage.input_tokens, 9044);
        assert_eq!(f.usage.output_tokens, 4);
        assert_eq!(f.usage.cache_read_input_tokens, 7168);
        assert_eq!(f.usage.cache_creation_input_tokens, 0);
        assert!((f.total_cost_usd - 0.0012).abs() < 1e-9);
        assert!(!f.is_error);
    }

    #[test]
    fn streamed_part_updates_in_place_by_id() {
        let mut f = Folded::default();
        fold_event(&mut f, r#"{"type":"text","part":{"id":"p1","text":"hel"}}"#);
        fold_event(&mut f, r#"{"type":"text","part":{"id":"p1","text":"hello"}}"#);
        fold_event(&mut f, r#"{"type":"text","part":{"id":"p2","text":" world"}}"#);
        assert_eq!(f.final_text(), "hello world");
    }

    #[test]
    fn idless_parts_append_not_collapse() {
        let mut f = Folded::default();
        fold_event(&mut f, r#"{"type":"text","part":{"text":"foo"}}"#);
        fold_event(&mut f, r#"{"type":"text","part":{"text":"bar"}}"#);
        assert_eq!(f.final_text(), "foobar");
    }

    #[test]
    fn error_event_marks_error_and_captures_message() {
        let mut f = Folded::default();
        fold_event(
            &mut f,
            r#"{"type":"error","sessionID":"x","error":{"data":{"message":"Unexpected server error"}}}"#,
        );
        assert!(f.is_error);
        assert_eq!(f.error_message, "Unexpected server error");
        assert!(!f.invalid_model);
    }

    #[test]
    fn model_rejection_flags_invalid_model() {
        let mut f = Folded::default();
        fold_event(
            &mut f,
            r#"{"type":"error","error":{"message":"provider error: model bogus/notreal not found"}}"#,
        );
        assert!(f.is_error);
        assert!(f.invalid_model);
    }

    #[test]
    fn benign_error_named_event_does_not_latch_failure() {
        let mut f = Folded::default();
        // A non-terminal event whose type merely contains "error" must not flip
        // is_error (exact-match on "error" only).
        fold_event(&mut f, r#"{"type":"error_metrics","part":{}}"#);
        assert!(!f.is_error);
    }

    #[test]
    fn usage_accumulates_across_steps() {
        // opencode reports per-step usage; the run total is the sum. num_turns
        // counts the step_finish events.
        let mut f = Folded::default();
        fold_event(
            &mut f,
            r#"{"type":"step_finish","part":{"tokens":{"input":16833,"output":13,"cache":{"read":128,"write":0}},"cost":0.001}}"#,
        );
        fold_event(
            &mut f,
            r#"{"type":"step_finish","part":{"tokens":{"input":72,"output":38,"cache":{"read":16960,"write":0}},"cost":0.002}}"#,
        );
        assert_eq!(f.usage.input_tokens, 16833 + 72);
        assert_eq!(f.usage.output_tokens, 13 + 38);
        assert_eq!(f.usage.cache_read_input_tokens, 128 + 16960);
        assert!((f.total_cost_usd - 0.003).abs() < 1e-9);
        assert_eq!(f.num_turns, 2);
    }

    #[test]
    fn malformed_and_unknown_lines_ignored() {
        let mut f = Folded::default();
        fold_event(&mut f, "not json");
        fold_event(&mut f, r#"{"type":"some.future.event","x":1}"#);
        assert_eq!(f, Folded::default());
    }

    #[test]
    fn build_argv_maps_flags() {
        let opts = Options {
            prompt: "hi".into(),
            model: Some("anthropic/claude-sonnet-4".into()),
            cwd: Some("/work".into()),
            skip_permissions: true,
            ..Options::default()
        };
        let v = build_argv(&opts);
        assert_eq!(v[0], "run");
        assert!(v.windows(2).any(|w| w == ["--format", "json"]));
        assert!(v.windows(2).any(|w| w == ["--model", "anthropic/claude-sonnet-4"]));
        assert!(v.windows(2).any(|w| w == ["--dir", "/work"]));
        assert!(v.contains(&"--auto".to_string()));
        assert_eq!(v[v.len() - 2], "--");
        assert_eq!(v.last().unwrap(), "hi");
    }

    #[test]
    fn every_network_tier_is_accepted_and_reported_as_an_open_network() {
        // opencode has no sandbox at all, so every request leaves the network
        // open. Reporting the requested tier back would hide that.
        for network in [Network::None, Network::Restricted, Network::Full] {
            let plan = OpencodeAdapter
                .network_plan(Some(Perms::ReadOnly), network, false)
                .unwrap();
            assert_eq!(plan.effective, Network::Full);
            assert_eq!(plan.enforcement, Enforcement::Unenforced);
        }
    }

    #[test]
    fn build_argv_no_auto_without_bypass_or_full() {
        let opts = Options {
            prompt: "hi".into(),
            perms: Some(Perms::ReadOnly),
            ..Options::default()
        };
        let v = build_argv(&opts);
        assert!(!v.contains(&"--auto".to_string()));
    }
}
