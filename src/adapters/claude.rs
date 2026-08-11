//! Claude adapter (default): drive `claude -p` (print mode), which is natively
//! non-interactive and authoritative -- no PTY, hook, or transcript scraping.
//!
//! `claude -p --output-format json` returns the answer (`result`), usage, cost,
//! session id, and -- crucially -- `modelUsage`, an object keyed by the model
//! that actually ran. That key is `model_resolved`: the launcher's truth, read
//! straight from claude's own output. (The PTY drive, [`super::claude_pty`], is
//! the fallback for environments where `claude -p` is unavailable; select it
//! with the undocumented `--pty` flag.)

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::adapters::claude_common;
use crate::adapters::procgroup;
use crate::adapters::{Adapter, DriverError, RunOutcome};
use crate::args::{Options, OutputFormat};
use crate::policy::{Enforcement, Network, Perms};
use crate::signals;
use crate::transcript::{Summary, Usage};

const POLL: Duration = Duration::from_millis(50);

/// Drives `claude -p` (and claude-compatible custom binaries) in print mode.
pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
    fn run(
        &self,
        opts: &Options,
        stream_out: Option<&mut dyn Write>,
    ) -> Result<RunOutcome, DriverError> {
        run(opts, stream_out)
    }

    fn drive(&self) -> &'static str {
        "print"
    }

    fn perms_enforcement(&self, perms: Perms) -> Enforcement {
        claude_common::perms_enforcement(perms)
    }

    fn network_enforcement(&self, perms: Option<Perms>, network: Network) -> Enforcement {
        claude_common::network_enforcement(perms, network)
    }
}

/// What `claude -p` reports for the turn.
#[derive(Debug, Default, PartialEq)]
struct Parsed {
    final_text: String,
    session_id: String,
    model: String,
    is_error: bool,
    invalid_model: bool,
    num_turns: u32,
    total_cost_usd: f64,
    duration_api_ms: u64,
    usage: Usage,
}

/// claude's print-mode output format for a given dash-p output format.
fn claude_format(output: OutputFormat) -> &'static str {
    match output {
        OutputFormat::StreamJson => "stream-json",
        // text and json are both derived from the structured result envelope.
        OutputFormat::Text | OutputFormat::Json => "json",
    }
}

fn build_argv(opts: &Options) -> Vec<String> {
    let mut v = vec![
        claude_common::resolve_bin(&opts.harness),
        "-p".to_string(),
        "--output-format".to_string(),
        claude_format(opts.output_format).to_string(),
    ];
    if opts.output_format == OutputFormat::StreamJson {
        // stream-json requires verbose, and partial messages give live tokens.
        v.push("--verbose".to_string());
        v.push("--include-partial-messages".to_string());
    }
    if let Some(m) = &opts.model {
        v.push("--model".to_string());
        v.push(m.clone());
    }
    v.extend(opts.extra_args.iter().cloned());
    // perms_args last: `--disallowedTools` is variadic, so keep it adjacent to
    // the `--` terminator where it can't swallow a forwarded flag's value.
    v.extend(claude_common::perms_args(opts));
    // `--` so a prompt beginning with `-` is the positional prompt, not a flag.
    v.push("--".to_string());
    v.push(opts.prompt.clone());
    v
}

/// Extract the turn's result from a claude `type:"result"` envelope.
fn parse_result(v: &Value) -> Parsed {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let u = v.get("usage");
    let usage = Usage {
        input_tokens: usage_field(u, "input_tokens"),
        output_tokens: usage_field(u, "output_tokens"),
        cache_read_input_tokens: usage_field(u, "cache_read_input_tokens"),
        cache_creation_input_tokens: usage_field(u, "cache_creation_input_tokens"),
    };
    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let result = s("result");
    Parsed {
        model: model_from_usage(v),
        is_error,
        invalid_model: is_error && looks_like_model_error(&result),
        num_turns: v.get("num_turns").and_then(Value::as_u64).unwrap_or(0) as u32,
        total_cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        duration_api_ms: v.get("duration_api_ms").and_then(Value::as_u64).unwrap_or(0),
        usage,
        session_id: s("session_id"),
        final_text: result,
    }
}

fn usage_field(usage: Option<&Value>, key: &str) -> u64 {
    usage.and_then(|u| u.get(key)).and_then(Value::as_u64).unwrap_or(0)
}

/// `model_resolved` is the `modelUsage` key with the most tokens (usually the
/// only one) -- claude's own record of what actually ran.
fn model_from_usage(v: &Value) -> String {
    let Some(map) = v.get("modelUsage").and_then(Value::as_object) else {
        return String::new();
    };
    map.iter()
        .max_by_key(|(_, mu)| {
            mu.get("inputTokens").and_then(Value::as_u64).unwrap_or(0)
                + mu.get("outputTokens").and_then(Value::as_u64).unwrap_or(0)
        })
        .map(|(name, _)| name.clone())
        .unwrap_or_default()
}

/// Does an errored result read like claude rejecting the requested model?
fn looks_like_model_error(result: &str) -> bool {
    let r = result.to_ascii_lowercase();
    r.contains("model")
        && (r.contains("issue with the selected model")
            || r.contains("may not exist")
            || r.contains("does not exist")
            || r.contains("pick a different model")
            || r.contains("unknown model"))
}

fn run(opts: &Options, mut stream_out: Option<&mut dyn Write>) -> Result<RunOutcome, DriverError> {
    let start = Instant::now();
    let timeout = Duration::from_millis(opts.timeout_ms);

    // claude prints diagnostics to stderr; silence it unless --debug (the
    // result we need is the structured stdout).
    let stderr = if opts.debug { Stdio::inherit() } else { Stdio::null() };
    let argv = build_argv(opts);
    // Pin a relative, path-like program to an absolute path so `--cwd` doesn't
    // relocate where the binary itself resolves (see `claude_common`).
    let program = claude_common::pin_program(argv[0].clone(), opts.cwd.as_deref())
        .map_err(|e| DriverError::Spawn(e.into()))?;
    let mut cmd = Command::new(&program);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr);
    if let Some(cwd) = &opts.cwd {
        cmd.current_dir(cwd);
    }
    // claude -p spawns its own tool subprocesses; lead a process group so an
    // interrupt/timeout can tear the whole tree down, not just the top process.
    procgroup::lead_process_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| DriverError::Spawn(e.into()))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line.clone()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let streaming = opts.output_format == OutputFormat::StreamJson && stream_out.is_some();
    let mut raw = String::new();

    loop {
        if signals::interrupted() {
            procgroup::terminate_group(child.id());
            let _ = child.wait();
            let _ = reader.join();
            return Err(DriverError::Interrupted);
        }
        if start.elapsed() > timeout {
            procgroup::terminate_group(child.id());
            let _ = child.wait();
            let _ = reader.join();
            return Err(DriverError::StopTimeout);
        }
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                if streaming
                    && let Some(w) = stream_out.as_mut()
                {
                    let _ = w.write_all(line.as_bytes());
                    let _ = w.flush();
                }
                raw.push_str(&line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader.join();
    let status = child.wait().map_err(DriverError::Io)?;

    // For json mode the whole stdout is one envelope; for stream-json the
    // envelope is the last `type:"result"` line.
    let parsed = find_result(&raw)
        .map(|v| parse_result(&v))
        .unwrap_or_else(|| Parsed {
            is_error: true,
            ..Default::default()
        });

    if parsed.final_text.is_empty() && !status.success() && !parsed.is_error {
        // claude produced no parseable result and exited non-zero.
        return Err(DriverError::ChildExitedEarly(String::new()));
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let summary = Summary {
        final_text: parsed.final_text,
        session_id: parsed.session_id,
        model: parsed.model,
        is_error: parsed.is_error,
        num_turns: parsed.num_turns.max(1),
        total_cost_usd: parsed.total_cost_usd,
        duration_api_ms: parsed.duration_api_ms,
        usage: parsed.usage,
        jsonl_replay: raw,
    };

    Ok(RunOutcome {
        summary,
        duration_ms,
        streamed: streaming,
        invalid_model: parsed.invalid_model,
    })
}

/// Find the result envelope in claude's stdout: parse the whole thing (json
/// mode), else the last `type:"result"` line (stream-json mode).
fn find_result(raw: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(raw.trim())
        && v.get("type").and_then(Value::as_str) == Some("result")
    {
        return Some(v);
    }
    raw.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v.get("type").and_then(Value::as_str) == Some("result"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","session_id":"sid-1","num_turns":1,"total_cost_usd":0.11,"duration_api_ms":1630,"usage":{"input_tokens":11580,"output_tokens":4,"cache_read_input_tokens":12737,"cache_creation_input_tokens":4875},"modelUsage":{"claude-opus-4-8[1m]":{"inputTokens":11580,"outputTokens":4}}}"#;

    #[test]
    fn parses_authoritative_model_and_usage() {
        let v: Value = serde_json::from_str(RESULT).unwrap();
        let p = parse_result(&v);
        assert_eq!(p.final_text, "pong");
        assert_eq!(p.session_id, "sid-1");
        assert_eq!(p.model, "claude-opus-4-8[1m]");
        assert_eq!(p.usage.input_tokens, 11580);
        assert_eq!(p.usage.output_tokens, 4);
        assert_eq!(p.usage.cache_read_input_tokens, 12737);
        assert!((p.total_cost_usd - 0.11).abs() < 1e-9);
        assert!(!p.is_error && !p.invalid_model);
    }

    #[test]
    fn model_picks_the_busiest_entry() {
        let v: Value = serde_json::from_str(
            r#"{"type":"result","modelUsage":{"claude-haiku-4-5":{"inputTokens":2,"outputTokens":1},"claude-opus-4-8":{"inputTokens":900,"outputTokens":80}}}"#,
        )
        .unwrap();
        assert_eq!(model_from_usage(&v), "claude-opus-4-8");
    }

    #[test]
    fn invalid_model_detected() {
        let v: Value = serde_json::from_str(
            r#"{"type":"result","is_error":true,"api_error_status":404,"result":"There's an issue with the selected model (bogus). It may not exist or you may not have access to it."}"#,
        )
        .unwrap();
        let p = parse_result(&v);
        assert!(p.is_error);
        assert!(p.invalid_model);
    }

    #[test]
    fn other_errors_are_not_invalid_model() {
        let v: Value = serde_json::from_str(
            r#"{"type":"result","is_error":true,"result":"rate limit exceeded"}"#,
        )
        .unwrap();
        let p = parse_result(&v);
        assert!(p.is_error && !p.invalid_model);
    }

    #[test]
    fn find_result_in_stream_lines() {
        let stream = format!(
            "{}\n{}\n{}",
            r#"{"type":"system","subtype":"init"}"#, r#"{"type":"assistant","message":{}}"#, RESULT
        );
        let v = find_result(&stream).unwrap();
        assert_eq!(v.get("type").unwrap(), "result");
        assert_eq!(parse_result(&v).model, "claude-opus-4-8[1m]");
    }

    #[test]
    fn build_argv_json_for_text_and_model() {
        let o = Options {
            prompt: "hi".into(),
            model: Some("opus".into()),
            ..Options::default()
        };
        let v = build_argv(&o);
        assert!(v.contains(&"-p".to_string()));
        assert!(v.windows(2).any(|w| w == ["--output-format", "json"]));
        assert!(v.windows(2).any(|w| w == ["--model", "opus"]));
        assert_eq!(v.last().unwrap(), "hi");
    }

    #[test]
    fn build_argv_stream_json_adds_verbose() {
        let o = Options {
            prompt: "hi".into(),
            output_format: OutputFormat::StreamJson,
            ..Options::default()
        };
        let v = build_argv(&o);
        assert!(v.windows(2).any(|w| w == ["--output-format", "stream-json"]));
        assert!(v.contains(&"--verbose".to_string()));
    }
}
