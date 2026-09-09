//! Codex adapter: drive `codex exec`, which is natively non-interactive, so
//! there is none of the PTY/hook/DEC machinery the claude adapter needs. We
//! spawn a plain subprocess, pipe the prompt on stdin (no positional argument,
//! so a prompt beginning with `-` is never mistaken for a flag), and read the
//! `--json` event stream on stdout.
//!
//! The event stream exposes the answer (`item.completed` / `agent_message`),
//! token usage (`turn.completed`), and the session id (`thread.started`), but
//! *not* the model. For an honest `model_resolved` we read codex's own session
//! rollout file (`turn_context.payload.model`) keyed by that session id -- the
//! launcher's truth -- and fall back to `"unknown"` rather than echoing the
//! requested model.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::adapters::procgroup;
use crate::adapters::{Adapter, DriverError, RunOutcome};
use crate::args::Options;
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
use crate::signals;
use crate::transcript::{Summary, Usage};

const POLL: Duration = Duration::from_millis(50);
/// Cap on captured stderr surfaced when codex fails without a JSON error event.
const STDERR_TAIL_CAP: usize = 8192;
/// Bounded wait for the detached stderr reader to drain before snapshotting the
/// tail for a failure diagnostic -- long enough to catch a fast startup/auth
/// failure, short enough to never stall a real run.
const STDERR_DRAIN_WAIT: Duration = Duration::from_millis(200);

/// Drives the `codex` CLI via its non-interactive `exec` subcommand.
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
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
            // codex `--sandbox read-only|workspace-write` is an OS sandbox.
            Perms::ReadOnly | Perms::WorkspaceWrite => Enforcement::OsSandbox,
            // danger-full-access removes the sandbox.
            Perms::Full => Enforcement::Unenforced,
        }
    }

    fn network_plan(
        &self,
        perms: Option<Perms>,
        network: Network,
        bypass: bool,
    ) -> Result<NetworkPlan, String> {
        // codex has no domain-allowlist setting -- its sandbox network switch is
        // a boolean. There is no honest mapping for "some network", and silently
        // picking `none` or `full` is exactly the lie this crate exists to
        // avoid, so reject the tier instead.
        if network == Network::Restricted {
            return Err(
                "--network restricted is unsupported: codex's sandbox network switch is \
                 all-or-nothing (no domain allowlist). Use --network none or --network full"
                    .to_string(),
            );
        }
        // The bypass flag turns the sandbox off outright, so the network is open
        // whatever tier was asked for.
        if bypass {
            return Ok(NetworkPlan::open());
        }
        Ok(match perms {
            // The read-only sandbox blocks network unconditionally: codex
            // exposes `network_access` only for workspace-write, so a `full`
            // request here really runs with no network. Report the downgrade.
            Some(Perms::ReadOnly) => NetworkPlan::os_sandbox(Network::None),
            // The one tier with a real switch, and `build_argv` pins it rather
            // than inheriting whatever the user's config.toml happens to say.
            Some(Perms::WorkspaceWrite) => match network {
                Network::None => NetworkPlan::os_sandbox(Network::None),
                _ => NetworkPlan::open(),
            },
            // danger-full-access removes the sandbox, so nothing is held.
            Some(Perms::Full) => NetworkPlan::open(),
            // Without --perms, codex resolves the sandbox mode from its own
            // config, which dash-p does not control and cannot read reliably
            // (profiles, env, layered files). The override we pass applies only
            // if that mode is workspace-write, and `danger-full-access` would
            // leave the network open -- so claim nothing rather than report a
            // block we cannot prove.
            None => NetworkPlan::open(),
        })
    }
}

/// The `-c` override that pins codex's workspace-write network switch.
///
/// Without it the value falls back to the user's own `config.toml`, where a
/// `[sandbox_workspace_write] network_access = true` silently defeats
/// `--network none` -- the sandbox stays on and keeps blocking the filesystem,
/// so the run *looks* sandboxed while the network is wide open. Only
/// workspace-write reads this key: read-only blocks network unconditionally and
/// danger-full-access has no sandbox, so the override is inert (and harmless)
/// for both.
fn network_access_override(network: Network) -> String {
    let access = match network {
        // Restricted is rejected in `network_plan` before a run gets here; deny
        // is the safe reading if it ever does.
        Network::None | Network::Restricted => false,
        Network::Full => true,
    };
    format!("sandbox_workspace_write.network_access={access}")
}

/// Accumulated state folded from the codex `--json` event stream.
#[derive(Debug, Default, PartialEq)]
struct Folded {
    final_text: String,
    session_id: String,
    usage: Usage,
    num_turns: u32,
    is_error: bool,
    /// The harness's own error text, surfaced to the user on failure.
    error_message: String,
    /// The error looks like the harness rejecting the requested model.
    invalid_model: bool,
}

/// Heuristic: does a harness error message indicate the model was rejected?
/// Matched against codex's own error text (e.g. "The 'x' model is not
/// supported ..."), so exit 31 reflects the harness's live verdict.
fn looks_like_model_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("model")
        && (m.contains("not supported")
            || m.contains("not found")
            || m.contains("does not exist")
            || m.contains("unknown model")
            || m.contains("invalid model"))
}

/// Fold one event line into the running state. Unknown lines are ignored, so a
/// new codex event type never breaks the parse.
fn fold_event(state: &mut Folded, line: &str) {
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let Some(ty) = obj.get("type").and_then(Value::as_str) else {
        return;
    };
    match ty {
        "thread.started" => {
            if let Some(id) = obj.get("thread_id").and_then(Value::as_str) {
                state.session_id = id.to_string();
            }
        }
        "item.completed" => {
            let item = obj.get("item");
            let is_message = item
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                == Some("agent_message");
            if is_message
                && let Some(text) = item.and_then(|i| i.get("text")).and_then(Value::as_str)
            {
                // Last agent message wins, matching the claude adapter.
                state.final_text = text.to_string();
            }
        }
        "turn.completed" => {
            state.num_turns += 1;
            if let Some(u) = obj.get("usage") {
                let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                state.usage = Usage {
                    input_tokens: get("input_tokens"),
                    output_tokens: get("output_tokens"),
                    cache_read_input_tokens: get("cached_input_tokens"),
                    // Codex has no cache-creation counter.
                    cache_creation_input_tokens: 0,
                };
            }
        }
        // Any failure/error event marks the run as errored.
        t if t.contains("failed") || t == "error" => {
            state.is_error = true;
            let msg = obj
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| {
                    obj.get("error")
                        .and_then(|e| e.get("message"))
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

/// Extract the resolved model from a codex session rollout file: the last
/// `turn_context` event's `payload.model`. `None` if absent or unparseable.
fn model_from_rollout(contents: &str) -> Option<String> {
    let mut model = None;
    for line in contents.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) == Some("turn_context")
            && let Some(m) = obj
                .get("payload")
                .and_then(|p| p.get("model"))
                .and_then(Value::as_str)
        {
            model = Some(m.to_string());
        }
    }
    model
}

/// `$CODEX_HOME` or `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(h));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".codex"))
}

/// Best-effort: the configured default model from `$CODEX_HOME/config.toml`.
/// codex has no model-enumeration command, so this is the most we can probe
/// without making a paid call.
pub fn configured_model() -> Option<String> {
    let contents = std::fs::read_to_string(codex_home()?.join("config.toml")).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("model")
            && let Some(val) = rest.trim_start().strip_prefix('=')
        {
            return Some(val.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Best-effort lookup of the resolved model from the rollout file whose name
/// contains `session_id`, under `$CODEX_HOME/sessions`.
fn resolve_model(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    let sessions = codex_home()?.join("sessions");
    let path = find_rollout(&sessions, session_id)?;
    let contents = std::fs::read_to_string(path).ok()?;
    model_from_rollout(&contents)
}

/// Walk `$CODEX_HOME/sessions/**` for a `.jsonl` file whose name contains the
/// session id. Codex lays sessions out under year/month/day directories.
fn find_rollout(dir: &std::path::Path, session_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_rollout(&path, session_id) {
                return Some(found);
            }
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.contains(session_id)
            && name.ends_with(".jsonl")
        {
            return Some(path);
        }
    }
    None
}

fn build_argv(opts: &Options) -> Vec<String> {
    let mut v = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    if let Some(m) = &opts.model {
        v.push("--model".to_string());
        v.push(m.clone());
    }
    if let Some(cwd) = &opts.cwd {
        v.push("--cd".to_string());
        v.push(cwd.clone());
    }
    // Map the requested permission tier to codex's native OS sandbox.
    match opts.perms {
        Some(Perms::ReadOnly) => {
            v.push("--sandbox".to_string());
            v.push("read-only".to_string());
        }
        Some(Perms::WorkspaceWrite) => {
            v.push("--sandbox".to_string());
            v.push("workspace-write".to_string());
        }
        Some(Perms::Full) => {
            v.push("--sandbox".to_string());
            v.push("danger-full-access".to_string());
        }
        None => {}
    }
    // Pin the sandbox's network switch whenever a tier was requested, so the
    // caller's intent wins over the user's config.toml. Left untouched when no
    // `--network` was passed, so callers that never opt in keep codex's own
    // configured behavior.
    if let Some(network) = opts.network {
        v.push("-c".to_string());
        v.push(network_access_override(network));
    }
    if opts.skip_permissions {
        v.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    }
    v
}

fn run(opts: &Options, mut stream_out: Option<&mut dyn Write>) -> Result<RunOutcome, DriverError> {
    let start = Instant::now();
    let timeout = Duration::from_millis(opts.timeout_ms);

    // Codex floods stderr with its own diagnostics (skill-load errors, MCP
    // worker failures, "Reading prompt from stdin..."). That noise would drown
    // the run, so we don't pass it through by default -- but we do capture the
    // tail so a startup/auth failure that never reaches the JSON stdout stream
    // still yields a diagnostic instead of an empty answer. `--debug` also
    // mirrors it to our stderr live.
    let mut child = {
        let mut cmd = Command::new("codex");
        cmd.args(build_argv(opts))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // codex exec spawns its own tool subprocesses; lead a process group so
        // an interrupt/timeout tears the whole tree down, not just top-level.
        procgroup::lead_process_group(&mut cmd);
        cmd.spawn().map_err(|e| DriverError::Spawn(e.into()))?
    };

    // Feed the prompt on stdin from its own thread, then close it (dropping the
    // handle) so codex starts the turn. Writing on a dedicated thread avoids a
    // deadlock when the prompt exceeds the pipe buffer and codex has started
    // writing stdout before draining stdin.
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = opts.prompt.clone();
        thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }

    // Drain stderr into a bounded byte tail (and mirror to our stderr under
    // --debug) so it can't fill the pipe and block codex, while preserving the
    // last bytes for a failure diagnostic. Read in fixed-size chunks (not
    // read_until) so a newline-free flood can't buffer an unbounded line before
    // the cap applies. Kept as raw bytes -- a mid-UTF-8 truncation or stray
    // non-UTF-8 byte must not panic or abort the drain; we lossily decode only
    // when surfacing. The thread is detached (never joined): a tool descendant
    // that inherits stderr and outlives codex could otherwise hang a join
    // forever, so we snapshot the shared tail after a bounded readiness wait.
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let debug = opts.debug;
    let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_done = Arc::new(AtomicBool::new(false));
    let stderr_tail_writer = Arc::clone(&stderr_tail);
    let stderr_done_writer = Arc::clone(&stderr_done);
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr_pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if debug {
                        let _ = std::io::stderr().write_all(&chunk[..n]);
                    }
                    if let Ok(mut tail) = stderr_tail_writer.lock() {
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > STDERR_TAIL_CAP {
                            let cut = tail.len() - STDERR_TAIL_CAP;
                            tail.drain(..cut);
                        }
                    }
                }
                Err(_) => break,
            }
        }
        stderr_done_writer.store(true, Ordering::SeqCst);
    });

    // Read stdout lines on a thread so the main loop can honor the timeout and
    // interrupts even while a read would otherwise block.
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

    let streaming = opts.output_format == crate::args::OutputFormat::StreamJson
        && stream_out.is_some();
    let mut folded = Folded::default();
    let mut replay = String::new();

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
                replay.push_str(&line);
                fold_event(&mut folded, line.trim_end());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader.join();
    let status = child.wait().map_err(DriverError::Io)?;
    if !status.success() {
        folded.is_error = true;
    }

    let model = resolve_model(&folded.session_id).unwrap_or_default();
    let duration_ms = start.elapsed().as_millis() as u64;

    // On failure with no agent message, surface the harness's own error text so
    // the user sees why (e.g. an unsupported-model message) instead of nothing.
    // Prefer a JSON `error` event; if there is none (a failure before codex
    // emitted any structured event -- auth, sandbox, startup), fall back to the
    // captured stderr tail rather than returning an empty answer.
    let final_text = if !folded.final_text.is_empty() {
        folded.final_text
    } else if !folded.error_message.is_empty() {
        folded.error_message
    } else if folded.is_error {
        // A startup/auth failure emits no JSON event, so nothing else delays
        // this snapshot -- wait (bounded) for the detached reader to drain the
        // pipe, then decode. Bounded so a descendant holding stderr can't stall.
        let deadline = Instant::now() + STDERR_DRAIN_WAIT;
        while !stderr_done.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        stderr_tail
            .lock()
            .map(|t| String::from_utf8_lossy(&t).trim().to_string())
            .unwrap_or_default()
    } else {
        folded.final_text
    };

    let summary = Summary {
        final_text,
        session_id: folded.session_id,
        model,
        is_error: folded.is_error,
        num_turns: folded.num_turns.max(1),
        total_cost_usd: 0.0,
        duration_api_ms: 0,
        usage: folded.usage,
        jsonl_replay: replay,
    };

    let mut streamed = false;
    if streaming
        && let Some(w) = stream_out.as_mut()
    {
        crate::emit::emit_result_envelope(*w, &summary, duration_ms).map_err(DriverError::Io)?;
        let _ = w.flush();
        streamed = true;
    }

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
            r#"{"type":"thread.started","thread_id":"abc-123"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":12,"reasoning_output_tokens":5}}"#,
        ];
        let mut f = Folded::default();
        for l in lines {
            fold_event(&mut f, l);
        }
        assert_eq!(f.final_text, "hello");
        assert_eq!(f.session_id, "abc-123");
        assert_eq!(f.num_turns, 1);
        assert_eq!(f.usage.input_tokens, 100);
        assert_eq!(f.usage.output_tokens, 12);
        assert_eq!(f.usage.cache_read_input_tokens, 40);
        assert_eq!(f.usage.cache_creation_input_tokens, 0);
        assert!(!f.is_error);
    }

    #[test]
    fn last_agent_message_wins() {
        let mut f = Folded::default();
        fold_event(&mut f, r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}"#);
        fold_event(&mut f, r#"{"type":"item.completed","item":{"type":"agent_message","text":"second"}}"#);
        assert_eq!(f.final_text, "second");
    }

    #[test]
    fn non_message_items_ignored() {
        let mut f = Folded::default();
        fold_event(&mut f, r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#);
        assert_eq!(f.final_text, "");
    }

    #[test]
    fn failure_event_marks_error() {
        let mut f = Folded::default();
        fold_event(&mut f, r#"{"type":"turn.failed","error":{"message":"boom"}}"#);
        assert!(f.is_error);
        assert!(!f.invalid_model);
        assert_eq!(f.error_message, "boom");
    }

    #[test]
    fn model_rejection_flags_invalid_model() {
        let mut f = Folded::default();
        fold_event(
            &mut f,
            r#"{"type":"error","message":"The 'bogus' model is not supported when using Codex with a ChatGPT account."}"#,
        );
        assert!(f.is_error);
        assert!(f.invalid_model);
    }

    #[test]
    fn detects_model_error_phrasings() {
        assert!(looks_like_model_error("The 'x' model is not supported"));
        assert!(looks_like_model_error("unknown model: y"));
        assert!(looks_like_model_error("that model does not exist"));
        assert!(!looks_like_model_error("rate limit exceeded"));
        assert!(!looks_like_model_error("the network is unreachable"));
    }

    #[test]
    fn malformed_and_unknown_lines_ignored() {
        let mut f = Folded::default();
        fold_event(&mut f, "not json");
        fold_event(&mut f, r#"{"type":"some.future.event","x":1}"#);
        assert_eq!(f, Folded::default());
    }

    #[test]
    fn model_from_rollout_takes_last_turn_context() {
        let contents = concat!(
            r#"{"timestamp":"t","type":"session_meta","payload":{"id":"x","model_provider":"openai"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"model":"gpt-5.5","cwd":"/x"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"model":"gpt-5.5-codex"}}"#,
        );
        assert_eq!(model_from_rollout(contents).as_deref(), Some("gpt-5.5-codex"));
    }

    #[test]
    fn model_from_rollout_none_when_absent() {
        let contents = r#"{"type":"session_meta","payload":{"id":"x"}}"#;
        assert_eq!(model_from_rollout(contents), None);
    }

    #[test]
    fn build_argv_maps_flags() {
        let opts = Options {
            model: Some("gpt-5.5".into()),
            cwd: Some("/work".into()),
            skip_permissions: true,
            ..Options::default()
        };
        let v = build_argv(&opts);
        assert_eq!(v[0], "exec");
        assert!(v.contains(&"--json".to_string()));
        assert!(v.windows(2).any(|w| w == ["--model", "gpt-5.5"]));
        assert!(v.windows(2).any(|w| w == ["--cd", "/work"]));
        assert!(v.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    fn opts_for(perms: Option<Perms>, network: Option<Network>) -> Options {
        Options { perms, network, ..Options::default() }
    }

    /// The network `-c` override specifically -- `build_argv` emits more than
    /// one `-c`, so match on the key rather than taking the first.
    fn network_override(argv: &[String]) -> Option<&str> {
        argv.iter()
            .map(String::as_str)
            .find(|a| a.starts_with("sandbox_workspace_write.network_access="))
    }

    #[test]
    fn build_argv_pins_the_sandbox_network_switch() {
        // The bug this fixes: without an explicit override the value falls back
        // to the user's config.toml, where `network_access = true` silently
        // defeats `--network none` while the sandbox still looks active.
        let none = build_argv(&opts_for(Some(Perms::WorkspaceWrite), Some(Network::None)));
        assert_eq!(
            network_override(&none),
            Some("sandbox_workspace_write.network_access=false")
        );

        let full = build_argv(&opts_for(Some(Perms::WorkspaceWrite), Some(Network::Full)));
        assert_eq!(
            network_override(&full),
            Some("sandbox_workspace_write.network_access=true")
        );
    }

    #[test]
    fn build_argv_leaves_network_alone_when_unrequested() {
        // Callers that never opt in keep codex's own configured behavior.
        let v = build_argv(&opts_for(Some(Perms::WorkspaceWrite), None));
        assert_eq!(network_override(&v), None);
    }

    #[test]
    fn network_full_survives_both_live_call_shapes() {
        // `--network full` is passed with a bypass (PR review) and with
        // workspace-write. Both must reach codex with the network open.
        let bypass = Options {
            skip_permissions: true,
            network: Some(Network::Full),
            ..Options::default()
        };
        let v = build_argv(&bypass);
        assert_eq!(
            network_override(&v),
            Some("sandbox_workspace_write.network_access=true")
        );
        assert!(v.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));

        let ws = build_argv(&opts_for(Some(Perms::WorkspaceWrite), Some(Network::Full)));
        assert!(ws.windows(2).any(|w| w == ["--sandbox", "workspace-write"]));
        assert_eq!(
            network_override(&ws),
            Some("sandbox_workspace_write.network_access=true")
        );
    }

    #[test]
    fn network_plan_reports_each_tier_honestly() {
        let a = CodexAdapter;

        // workspace-write is the tier with a real switch.
        assert_eq!(
            a.network_plan(Some(Perms::WorkspaceWrite), Network::None, false).unwrap(),
            NetworkPlan::os_sandbox(Network::None)
        );
        assert_eq!(
            a.network_plan(Some(Perms::WorkspaceWrite), Network::Full, false).unwrap(),
            NetworkPlan::open()
        );

        // read-only blocks network unconditionally, so `full` is downgraded.
        assert_eq!(
            a.network_plan(Some(Perms::ReadOnly), Network::None, false).unwrap(),
            NetworkPlan::os_sandbox(Network::None)
        );
        assert_eq!(
            a.network_plan(Some(Perms::ReadOnly), Network::Full, false).unwrap(),
            NetworkPlan::os_sandbox(Network::None)
        );

        // A bypass removes the sandbox, so nothing is held whatever was asked.
        assert_eq!(
            a.network_plan(Some(Perms::WorkspaceWrite), Network::None, true).unwrap(),
            NetworkPlan::open()
        );
        assert_eq!(
            a.network_plan(Some(Perms::Full), Network::None, false).unwrap(),
            NetworkPlan::open()
        );

        // Without --perms the sandbox mode comes from codex's own config, which
        // dash-p cannot confirm -- so report an open network rather than a block
        // it cannot prove, even though the override is still passed.
        assert_eq!(
            a.network_plan(None, Network::None, false).unwrap(),
            NetworkPlan::open()
        );
    }

    #[test]
    fn an_unenforced_plan_never_claims_a_restricted_tier() {
        // The invariant that stops `--network none` reading like a real block on
        // a harness that enforces nothing.
        let plan = NetworkPlan::open();
        assert_eq!(plan.effective, Network::Full);
        assert_eq!(plan.enforcement, Enforcement::Unenforced);
    }

    #[test]
    fn network_plan_rejects_restricted() {
        let err = CodexAdapter
            .network_plan(Some(Perms::WorkspaceWrite), Network::Restricted, false)
            .unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
        assert!(err.contains("allowlist"), "explains why: {err}");
    }

    // ---- observed-outcome probes -------------------------------------------
    //
    // These assert what the sandbox *does*, not what `capabilities` advertises.
    // They drive `codex sandbox` -- the same config keys as `codex exec`, but a
    // plain command under seatbelt, so there is no model call and no cost.
    //
    // They skip where the environment cannot support them (no codex, no
    // outbound network), so a plain `cargo test` stays green anywhere. A skip
    // proves nothing, so CI should set `DASHP_REQUIRE_PROBES=1` to turn every
    // skip into a failure.

    const PROBE_URL: &str = "https://example.com";

    /// Skip a probe, or fail when the environment promised to support it.
    /// Returns `true`, so a caller can write `if <cannot run> && skip(..)
    /// { return }` and read it as "skipping, so return".
    fn skip(reason: &str) -> bool {
        assert!(
            std::env::var("DASHP_REQUIRE_PROBES").as_deref() != Ok("1"),
            "DASHP_REQUIRE_PROBES=1 but the probe could not run: {reason}"
        );
        eprintln!("skipping: {reason}");
        true
    }

    /// Translate the `codex exec` argv dash-p builds into the equivalent
    /// `codex sandbox` invocation: same `-c` overrides, but the sandbox mode
    /// arrives as a config key rather than codex exec's `--sandbox` flag.
    /// Driving the real `build_argv` output is the point -- a hand-written flag
    /// list here could drift from what dash-p actually sends.
    fn sandbox_argv_from(exec_argv: &[String]) -> Vec<String> {
        let mut v = vec!["sandbox".to_string()];
        let mut i = 0;
        while i < exec_argv.len() {
            match exec_argv[i].as_str() {
                "--sandbox" => {
                    v.push("-c".to_string());
                    // `{:?}` quotes it, which is what TOML wants for a string.
                    v.push(format!("sandbox_mode={:?}", exec_argv[i + 1]));
                    i += 2;
                }
                // Only `-c` carries over. `codex sandbox` demands a
                // `--permission-profile` as soon as `--cd` appears, so the probe
                // sets the workspace with the process working directory instead.
                "-c" => {
                    v.push("-c".to_string());
                    v.push(exec_argv[i + 1].clone());
                    i += 2;
                }
                _ => i += 1,
            }
        }
        v
    }

    /// A token unique to this call, for probe paths. The process id alone is
    /// shared by every test in the binary, and cargo runs them in parallel.
    fn probe_token() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// As `probe_token`, plus a clock reading, so a path is not guessable ahead
    /// of time.
    fn unique_token() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{nanos:x}", probe_token())
    }

    /// A throwaway workspace under `target/`, outside `$TMPDIR`. codex's
    /// workspace-write tier makes `$TMPDIR` writable by itself, so a probe
    /// workspace there cannot distinguish "the workspace root is right" from
    /// "this happens to be the temp dir".
    fn probe_workspace() -> Option<std::path::PathBuf> {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("dash-p-probes");
        std::fs::create_dir_all(&base).ok()?;
        let dir = base.join(format!("ws-{}", unique_token()));
        std::fs::create_dir(&dir).ok()?;
        Some(dir)
    }

    /// Create `dir` with mode 0700 in one step. Fails if it already exists, so a
    /// pre-created path cannot be reused.
    #[cfg(unix)]
    fn create_private_dir(dir: &std::path::Path) -> Option<()> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir).ok()
    }

    #[cfg(not(unix))]
    fn create_private_dir(dir: &std::path::Path) -> Option<()> {
        std::fs::create_dir(dir).ok()
    }

    /// A throwaway `CODEX_HOME` whose config contradicts the guarantee the
    /// probes assert: an open sandbox network.
    ///
    /// Without it the probes inherit the developer's config. On a machine using
    /// codex's defaults, `network_access` is already false, so deleting dash-p's
    /// pin would make the open-network control skip instead of fail and the
    /// probe would quietly lose its teeth. `codex sandbox` runs a plain command
    /// and needs no credentials, so this home holds only a `config.toml`.
    fn hostile_codex_home() -> Option<std::path::PathBuf> {
        // Exclusive create with 0700 in one step: `temp_dir()` is shared on
        // Linux, so a guessable pre-created path would let another local user
        // make `config.toml` a symlink and have the write below follow it.
        let home = std::env::temp_dir().join(format!("dash-p-probe-home-{}", unique_token()));
        create_private_dir(&home)?;
        std::fs::write(
            home.join("config.toml"),
            "[sandbox_workspace_write]\nnetwork_access = true\n",
        )
        .ok()?;
        Some(home)
    }

    /// Run `argv` under the sandbox dash-p's own flags describe. `None` when
    /// codex isn't installed. `cwd` becomes the sandbox's workspace root, and
    /// the run always uses a hostile `CODEX_HOME` so the pins are contradicted.
    fn under_sandbox_in(
        exec_argv: &[String],
        cwd: Option<&std::path::Path>,
        argv: &[&str],
    ) -> Option<std::process::Output> {
        let mut a = sandbox_argv_from(exec_argv);
        a.push("--".to_string());
        a.extend(argv.iter().map(|s| s.to_string()));
        let mut cmd = std::process::Command::new("codex");
        cmd.args(&a);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // No hostile home means no probe. Falling back to the developer's real
        // config would quietly disarm the probe: on a machine with codex's
        // defaults, a deleted pin would still pass.
        let home = hostile_codex_home()?;
        cmd.env("CODEX_HOME", &home);
        let out = cmd.output().ok();
        let _ = std::fs::remove_dir_all(&home);
        out
    }

    fn under_sandbox(exec_argv: &[String], argv: &[&str]) -> Option<std::process::Output> {
        under_sandbox_in(exec_argv, None, argv)
    }

    /// Whether a network call actually gets out of that sandbox. `None` when
    /// codex could not run the command at all -- a usage or config error would
    /// otherwise read as "the network was blocked" and pass a probe vacuously.
    fn network_reachable(exec_argv: &[String]) -> Option<bool> {
        let out = under_sandbox(
            exec_argv,
            // Short timeout: a machine with codex but no network otherwise
            // stalls every probe for the full duration.
            &["curl", "-sS", "-m", "5", "-o", "/dev/null", "-w", "%{http_code}", PROBE_URL],
        )?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("Usage:") || stderr.contains("error: ") {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).contains("200"))
    }

    /// The open-network control: proves the probe can reach the network at all
    /// under this codex, so a later block is the sandbox and not a broken
    /// environment. `false` means the caller should skip.
    fn network_control_reaches_out() -> bool {
        let full = build_argv(&opts_for(Some(Perms::WorkspaceWrite), Some(Network::Full)));
        match network_reachable(&full) {
            None => !skip("codex cannot run a sandboxed command"),
            Some(false) => !skip(&format!("no outbound network reached {PROBE_URL}")),
            Some(true) => true,
        }
    }

    #[test]
    fn network_none_actually_blocks_the_network() {
        if !network_control_reaches_out() {
            return;
        }
        let none_argv = build_argv(&opts_for(Some(Perms::WorkspaceWrite), Some(Network::None)));
        // The control proved the probe can reach the network, so a block here is
        // the sandbox and not a flaky link or a broken codex.
        assert_eq!(
            network_reachable(&none_argv),
            Some(false),
            "--network none did not block the network: dash-p emitted {none_argv:?}"
        );
    }

    #[test]
    fn read_only_blocks_the_network_whatever_tier_was_asked_for() {
        // `network_plan` downgrades read-only + `full` to `none` on the claim
        // that the read-only sandbox has no network switch. That claim is
        // load-bearing -- read-only + full even passes `network_access=true` in
        // argv -- so probe it rather than trust it.
        if !network_control_reaches_out() {
            return;
        }
        let argv = build_argv(&opts_for(Some(Perms::ReadOnly), Some(Network::Full)));
        assert_eq!(
            network_reachable(&argv),
            Some(false),
            "read-only reached the network, so the downgrade to none is wrong: {argv:?}"
        );
    }

    #[test]
    fn workspace_write_sandbox_blocks_writes_outside_the_workspace() {
        // The control from the original report: the OS sandbox really is on for
        // the same invocation, so a network leak is a --network wiring gap
        // rather than a missing sandbox.
        let Ok(home) = std::env::var("HOME") else {
            skip("no HOME");
            return;
        };
        // Under `target/`, deliberately NOT under $TMPDIR: codex's
        // workspace-write tier makes $TMPDIR writable on its own, so a workspace
        // there would let the inside-write control pass without proving the
        // workspace root is what dash-p asked for.
        let Some(tmp) = probe_workspace() else {
            skip("cannot create a workspace dir");
            return;
        };
        // The temp dir is the workspace root (set as the process working
        // directory), so "outside the workspace" is well defined.
        let argv = build_argv(&opts_for(Some(Perms::WorkspaceWrite), Some(Network::None)));

        // Can codex run a sandboxed command here at all? A no-op separates a
        // broken environment (missing binary, no `sandbox` subcommand, bad
        // config key) from a sandbox that ran and denied something.
        match under_sandbox_in(&argv, Some(&tmp), &["true"]) {
            Some(out) if out.status.success() => {}
            _ => {
                let _ = std::fs::remove_dir_all(&tmp);
                skip("codex cannot run a sandboxed command");
                return;
            }
        }

        // Positive control: the same invocation must ALLOW a write inside the
        // workspace. codex ran a moment ago, so a denial here is a real defect
        // (workspace-write delivered as read-only, say) and must fail rather
        // than skip -- otherwise the denial below passes for the wrong reason.
        let inside = tmp.join("inside.txt");
        let allowed = under_sandbox_in(&argv, Some(&tmp), &["touch", &inside.to_string_lossy()])
            .expect("codex ran a moment ago");
        let inside_ok = allowed.status.success() && inside.exists();
        if !inside_ok {
            let _ = std::fs::remove_dir_all(&tmp);
            panic!(
                "workspace-write denied a write inside its own workspace ({}); \
                 the sandbox mode mapping is wrong: {argv:?}",
                inside.display()
            );
        }

        // Now the real assertion: a write outside the workspace is denied.
        let outside = format!("{home}/.dash-p-sandbox-probe-{}", probe_token());
        let out = under_sandbox_in(&argv, Some(&tmp), &["touch", &outside])
            .expect("codex ran a moment ago");
        // Clean up in the failure case, where the sandbox let the write through.
        let existed = std::path::Path::new(&outside).exists();
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            !existed && !out.status.success(),
            "workspace-write sandbox allowed a write to {outside}"
        );
    }
}
