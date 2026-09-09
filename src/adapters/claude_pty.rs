//! Claude adapter: drive the interactive `claude` TUI under a PTY with the
//! prompt passed as a positional argument (interactive mode auto-submits it),
//! answer Ink's startup terminal probes on a pump thread, and wait for the Stop
//! hook.
//!
//! This is deliberately free of the input-timing machinery a keystroke-driven
//! approach needs: because the prompt is a positional arg, there is no
//! "wait for Ink to be ready, then type, then debounce Enter" dance. The only
//! thing the pump thread writes back to the PTY is DEC-query responses and a
//! single Enter to dismiss the workspace-trust dialog if it appears before the
//! session starts.
//!
//! It also drives a [`Harness::Custom`] path: a fork or wrapper of `claude` is
//! assumed claude-compatible and spoken to with this same protocol.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use portable_pty::{Child, MasterPty};

use crate::adapters::claude_common;
use crate::adapters::{Adapter, DriverError, RunOutcome};
use crate::args::{Options, OutputFormat};
use crate::dec::DecResponder;
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
use crate::hook::{self, HookHarness, PayloadFields};
use crate::pty::{self, SpawnConfig};
use crate::signals;
use crate::stream::Tailer;
use crate::transcript::{self, Summary, Usage};

const RECENT_CAP: usize = 8192;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Upper bound on un-newline-terminated FIFO bytes we'll buffer. Hook lines are
/// short single-line JSON; anything past this is malformed and gets dropped.
const MAX_LINE_BUF: usize = 1 << 20;
/// Stop can fire a few ms before claude flushes the assistant line to the
/// transcript JSONL. Retry window for transcript-derived summaries.
const TRANSCRIPT_RETRIES: u32 = 40;
const TRANSCRIPT_RETRY_DELAY: Duration = Duration::from_millis(50);
/// After Stop, briefly keep draining the transcript tailer so the final
/// assistant line (flushed just after Stop) makes it into the stream.
const POST_STOP_DRAIN_ROUNDS: u32 = 20;
const POST_STOP_DRAIN_DELAY: Duration = Duration::from_millis(20);

/// Drives the `claude` TUI under a PTY (the undocumented `--pty` flag). The
/// print-mode [`super::claude::ClaudeAdapter`] is the default; this is the
/// fallback for environments where `claude -p` is unavailable.
pub struct ClaudePtyAdapter;

impl Adapter for ClaudePtyAdapter {
    fn run(
        &self,
        opts: &Options,
        stream_out: Option<&mut dyn Write>,
    ) -> Result<RunOutcome, DriverError> {
        run(opts, stream_out)
    }

    fn drive(&self) -> &'static str {
        "pty"
    }

    fn perms_enforcement(&self, perms: Perms) -> Enforcement {
        claude_common::perms_enforcement(perms)
    }

    fn network_plan(
        &self,
        perms: Option<Perms>,
        network: Network,
        bypass: bool,
    ) -> Result<NetworkPlan, String> {
        Ok(claude_common::network_plan(perms, network, bypass))
    }
}

struct Shared {
    exited: AtomicBool,
    /// Set once the SessionStart hook fires. The pump thread stops scanning
    /// for the workspace-trust dialog after this, so a later assistant message
    /// that happens to contain "trust"/"folder" can never trigger a stray Enter.
    session_started: AtomicBool,
    debug: bool,
    /// Rolling tail of recent PTY output, for diagnostics if the child dies.
    tail: Mutex<Vec<u8>>,
}

/// Run a single prompt to completion. When `stream_out` is `Some` and the
/// output format is stream-json, transcript lines are written to it live as
/// claude flushes them, followed by the trailing `result` envelope.
pub fn run(opts: &Options, mut stream_out: Option<&mut dyn Write>) -> Result<RunOutcome, DriverError> {
    let start = Instant::now();

    let harness = HookHarness::create().map_err(DriverError::Io)?;
    let argv = build_argv(opts, &harness.settings_json);
    if opts.debug {
        eprintln!("[dash-p] argv: {argv:?}");
    }

    // Open the FIFO read side (non-blocking) before spawning so the child's
    // hook never blocks opening the write side. Also hold a write side open
    // ourselves so the reader never observes a spurious EOF between hook
    // fires (a FIFO reader sees EOF whenever the last writer closes).
    let fifo = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(&harness.fifo_path)
        .map_err(DriverError::Io)?;
    let _fifo_keepalive = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(&harness.fifo_path)
        .map_err(DriverError::Io)?;

    let mut extra_env = BTreeMap::new();
    extra_env.insert(
        "DASHP_FIFO".to_string(),
        harness.fifo_path.to_string_lossy().into_owned(),
    );

    let spawned = pty::spawn(SpawnConfig {
        argv: &argv,
        cwd: opts.cwd.as_deref(),
        extra_env: &extra_env,
        rows: opts.rows,
        cols: opts.cols,
    })
    .map_err(DriverError::Spawn)?;

    let mut child = spawned.child;
    let master = spawned.master;
    let reader = spawned.reader;
    let writer = spawned.writer;

    let shared = Arc::new(Shared {
        exited: AtomicBool::new(false),
        session_started: AtomicBool::new(false),
        debug: opts.debug,
        tail: Mutex::new(Vec::new()),
    });

    let pump = {
        let shared = Arc::clone(&shared);
        let (rows, cols) = (opts.rows, opts.cols);
        thread::spawn(move || pump_loop(reader, writer, shared, rows, cols))
    };

    let streaming = opts.output_format == OutputFormat::StreamJson && stream_out.is_some();
    let mut tailer: Option<Tailer> = None;
    let mut transcript_path: Option<String> = None;

    let timeout = Duration::from_millis(opts.timeout_ms);
    let mut line_buf: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 4096];
    let mut saw_session_start = false;
    let mut stop_payload: Option<String> = None;

    let stop_payload = loop {
        if signals::interrupted() {
            teardown(&mut child, master, pump);
            return Err(DriverError::Interrupted);
        }
        if start.elapsed() > timeout {
            teardown(&mut child, master, pump);
            return Err(if saw_session_start {
                DriverError::StopTimeout
            } else {
                DriverError::SessionStartTimeout
            });
        }
        match (&fifo).read(&mut read_buf) {
            Ok(0) => {}
            Ok(n) => line_buf.extend_from_slice(&read_buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                teardown(&mut child, master, pump);
                return Err(DriverError::Io(e));
            }
        }

        while let Some(nl) = line_buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&line_buf[..nl]).into_owned();
            line_buf.drain(..=nl);
            if let Some(ev) = hook::parse_line(&line) {
                match ev.event {
                    hook::HookEvent::SessionStart => {
                        saw_session_start = true;
                        shared.session_started.store(true, Ordering::SeqCst);
                        if opts.debug {
                            eprintln!("[dash-p +{}ms] SessionStart", start.elapsed().as_millis());
                        }
                        if streaming && transcript_path.is_none() {
                            transcript_path = hook::extract_fields(&ev.payload).transcript_path;
                        }
                    }
                    hook::HookEvent::Stop => {
                        if opts.debug {
                            eprintln!("[dash-p +{}ms] Stop", start.elapsed().as_millis());
                        }
                        if transcript_path.is_none() {
                            transcript_path = hook::extract_fields(&ev.payload).transcript_path;
                        }
                        stop_payload = Some(ev.payload);
                    }
                    hook::HookEvent::Unknown => {}
                }
            }
            if stop_payload.is_some() {
                break;
            }
        }

        // Guard against unbounded growth if a relay ever writes data without a
        // newline terminator. Hook lines are short single-line JSON.
        if line_buf.len() > MAX_LINE_BUF {
            if opts.debug {
                eprintln!(
                    "[dash-p] dropping {} bytes of unterminated FIFO data",
                    line_buf.len()
                );
            }
            line_buf.clear();
        }

        // Live-tail the transcript while the turn is in progress.
        if streaming
            && let Err(e) = pump_tailer(&mut tailer, &transcript_path, reborrow(&mut stream_out)) {
                teardown(&mut child, master, pump);
                return Err(DriverError::Io(e));
            }

        if let Some(payload) = stop_payload.take() {
            break payload;
        }

        // Checked only after draining + processing the FIFO this iteration, so a
        // child that writes Stop and exits in the same poll window still has its
        // answer recovered instead of being reported as exited-early.
        if shared.exited.load(Ordering::SeqCst) {
            let tail = shared.tail.lock().map(|t| strip_csi(&t)).unwrap_or_default();
            teardown(&mut child, master, pump);
            return Err(DriverError::ChildExitedEarly(tail));
        }

        thread::sleep(POLL_INTERVAL);
    };

    // The final assistant line is often flushed just after Stop; keep draining.
    if streaming {
        for _ in 0..POST_STOP_DRAIN_ROUNDS {
            // Best-effort final flush; we already have the answer.
            let _ = pump_tailer(&mut tailer, &transcript_path, reborrow(&mut stream_out));
            thread::sleep(POST_STOP_DRAIN_DELAY);
        }
    }

    let fields = hook::extract_fields(&stop_payload);
    let summary = summarize(opts, &fields, transcript_path.as_deref());

    // We have the answer; tear the child down immediately.
    teardown(&mut child, master, pump);

    let summary = summary?;
    let duration_ms = start.elapsed().as_millis() as u64;

    let mut streamed = false;
    if streaming
        && let Some(w) = reborrow(&mut stream_out) {
            crate::emit::emit_result_envelope(w, &summary, duration_ms).map_err(DriverError::Io)?;
            let _ = w.flush();
            streamed = true;
        }

    Ok(RunOutcome {
        summary,
        duration_ms,
        streamed,
        // The claude PTY path doesn't yet classify a model rejection distinctly
        // (it would surface as a generic startup/agent error).
        invalid_model: false,
    })
}

/// Short-lived reborrow of an `Option<&mut dyn Write>` so it can be passed
/// repeatedly without the borrow outliving each call.
fn reborrow<'a>(o: &'a mut Option<&mut dyn Write>) -> Option<&'a mut dyn Write> {
    o.as_mut().map(|w| &mut **w as &mut dyn Write)
}

fn pump_tailer(
    tailer: &mut Option<Tailer>,
    transcript_path: &Option<String>,
    out: Option<&mut dyn Write>,
) -> std::io::Result<()> {
    let Some(w) = out else { return Ok(()) };
    if tailer.is_none()
        && let Some(tp) = transcript_path {
            match Tailer::open(Path::new(tp)) {
                Ok(t) => *tailer = Some(t),
                // The transcript file may not exist yet; expected, retry later.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    if let Some(t) = tailer.as_mut() {
        let n = t.pump(w)?;
        if n > 0 {
            w.flush()?;
        }
    }
    Ok(())
}

fn teardown(
    child: &mut Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    pump: thread::JoinHandle<()>,
) {
    kill_child_group(child);
    // Dropping the master closes our fds; the child is already gone, so the
    // pump thread's blocking read returns EOF and the thread exits.
    drop(master);
    let _ = pump.join();
}

/// Kill the child and any descendants it put in its process group. portable-pty
/// makes the child a session leader on the PTY, so its pgid equals its pid and
/// `killpg` reaches the whole group. SIGTERM first with a short grace window,
/// then SIGKILL.
fn kill_child_group(child: &mut Box<dyn Child + Send + Sync>) {
    let pgid = child.process_id().map(|p| Pid::from_raw(p as i32));
    if let Some(pgid) = pgid {
        let _ = signal::killpg(pgid, Signal::SIGTERM);
    }
    let mut reaped = false;
    for _ in 0..15 {
        if let Ok(Some(_)) = child.try_wait() {
            reaped = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Only escalate to SIGKILL while the child is still alive. Sending it after
    // the child has been reaped risks signalling an unrelated process group
    // that reused the pid/pgid.
    if !reaped {
        if let Some(pgid) = pgid {
            let _ = signal::killpg(pgid, Signal::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Derive a Summary. For text output we prefer the Stop payload's
/// `last_assistant_message` (present in recent claude versions) so we never
/// wait on the transcript flush. json/stream-json need the transcript for
/// usage and cost, so we retry briefly there, falling back to the payload.
fn summarize(
    opts: &Options,
    fields: &PayloadFields,
    fallback_transcript: Option<&str>,
) -> Result<Summary, DriverError> {
    // Text output normally uses the payload's message directly to avoid waiting
    // on the transcript flush. But the transcript is the only authoritative
    // source of the resolved model, so when a metadata file is requested we
    // fall through to reading it (then still fall back to the payload).
    if opts.output_format == OutputFormat::Text
        && opts.meta_file.is_none()
        && let Some(msg) = &fields.last_assistant_message {
            return Ok(payload_only_summary(msg, fields));
        }

    // Prefer the Stop payload's transcript_path, but fall back to the one we
    // captured at SessionStart so json/stream-json still get usage/cost even if
    // a given Stop payload omits the field.
    let transcript = fields.transcript_path.as_deref().or(fallback_transcript);
    if let Some(tp) = transcript {
        let path = Path::new(tp);
        for _ in 0..TRANSCRIPT_RETRIES {
            if let Ok(Ok(s)) = transcript::parse_file(path)
                && (!s.final_text.is_empty() || s.is_error) {
                    return Ok(s);
                }
            thread::sleep(TRANSCRIPT_RETRY_DELAY);
        }
    }

    if let Some(msg) = &fields.last_assistant_message {
        return Ok(payload_only_summary(msg, fields));
    }
    Err(DriverError::TranscriptUnavailable)
}

fn payload_only_summary(msg: &str, fields: &PayloadFields) -> Summary {
    Summary {
        final_text: msg.to_string(),
        session_id: fields.session_id.clone().unwrap_or_default(),
        // The payload-only fast path has no transcript to read the model from;
        // metadata reports it as "unknown" rather than guessing.
        model: String::new(),
        is_error: false,
        num_turns: 1,
        total_cost_usd: 0.0,
        duration_api_ms: 0,
        usage: Usage::default(),
        jsonl_replay: String::new(),
    }
}

fn build_argv(opts: &Options, settings_json: &str) -> Vec<String> {
    // Pin a relative, path-like bin to absolute so `--cwd` (the PTY child's cwd)
    // doesn't relocate where the binary resolves. Fall back to the raw bin only
    // if the launch dir can't be read (spawn then surfaces the real error).
    let bin = claude_common::resolve_bin(&opts.harness);
    let bin = claude_common::pin_program(bin.clone(), opts.cwd.as_deref()).unwrap_or(bin);
    let mut v = vec![bin, "--settings".to_string(), settings_json.to_string()];
    if let Some(m) = &opts.model {
        v.push("--model".to_string());
        v.push(m.clone());
    }
    v.extend(opts.extra_args.iter().cloned());
    // perms_args last: `--disallowedTools` is variadic, so keep it adjacent to
    // the `--` terminator where it can't swallow a forwarded flag's value.
    v.extend(claude_common::perms_args(opts));
    // `--` terminates option parsing so a prompt beginning with `-` is taken as
    // the positional prompt, not a flag. The prompt MUST come last.
    v.push("--".to_string());
    v.push(opts.prompt.clone());
    v
}

fn pump_loop(
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
    shared: Arc<Shared>,
    rows: u16,
    cols: u16,
) {
    let mut dec = DecResponder::new(rows, cols);
    let mut recent: Vec<u8> = Vec::new();
    let mut trust_dismissed = false;
    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                shared.exited.store(true, Ordering::SeqCst);
                break;
            }
            Ok(n) => {
                let chunk = &buf[..n];

                let resp = dec.feed(chunk);
                if !resp.is_empty() {
                    let _ = writer.write_all(&resp);
                    let _ = writer.flush();
                }

                recent.extend_from_slice(chunk);
                if recent.len() > RECENT_CAP {
                    let drop = recent.len() - RECENT_CAP;
                    recent.drain(..drop);
                }
                if let Ok(mut t) = shared.tail.lock() {
                    *t = recent.clone();
                }

                // Only look for the workspace-trust dialog before the session
                // starts -- it blocks startup, so it can only appear then.
                let pre_session = !shared.session_started.load(Ordering::SeqCst);
                if !trust_dismissed && pre_session {
                    let stripped = strip_csi(&recent);
                    if stripped.contains("trust") && stripped.contains("folder") {
                        let _ = writer.write_all(b"\r");
                        let _ = writer.flush();
                        trust_dismissed = true;
                        if shared.debug {
                            eprintln!("[dash-p] workspace-trust dialog dismissed");
                        }
                    }
                }

                if shared.debug {
                    eprintln!("[dash-p] pty {n} bytes");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                shared.exited.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
}

/// Strip CSI / OSC / DCS escape sequences, leaving literal payload. Used so
/// substring matching (trust-dialog detection, diagnostics) is robust against
/// the cursor-positioning escapes the TUI pads words with.
fn strip_csi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b != 0x1b {
            out.push(b);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        match bytes[i + 1] {
            b'[' => {
                i += 2;
                while i < bytes.len() && (0x30..=0x3f).contains(&bytes[i]) {
                    i += 1;
                }
                while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // final byte
                }
            }
            b']' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'P' | b'X' | b'^' | b'_' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 2,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;

    fn opts() -> Options {
        Options {
            prompt: "hi".to_string(),
            ..Options::default()
        }
    }

    #[test]
    fn build_argv_minimal_puts_prompt_last() {
        let v = build_argv(&opts(), "{}");
        assert_eq!(v[0], std::env::var("DASHP_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()));
        assert_eq!(v[1], "--settings");
        assert_eq!(v[2], "{}");
        assert_eq!(v.last().unwrap(), "hi");
    }

    #[test]
    fn build_argv_custom_harness_uses_its_binary() {
        let o = Options {
            harness: Harness::Custom("/opt/bin/claude-fork".into()),
            ..opts()
        };
        let v = build_argv(&o, "{}");
        assert_eq!(v[0], "/opt/bin/claude-fork");
        assert_eq!(v.last().unwrap(), "hi");
    }

    #[test]
    fn build_argv_model_and_skip() {
        let o = Options {
            model: Some("opus".into()),
            skip_permissions: true,
            ..opts()
        };
        let v = build_argv(&o, "{}");
        assert!(v.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(v.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(v.last().unwrap(), "hi");
    }

    #[test]
    fn build_argv_extra_args_before_prompt() {
        let o = Options {
            extra_args: vec!["--verbose".into()],
            ..opts()
        };
        let v = build_argv(&o, "{}");
        let verbose_idx = v.iter().position(|s| s == "--verbose").unwrap();
        let prompt_idx = v.iter().position(|s| s == "hi").unwrap();
        assert!(verbose_idx < prompt_idx);
    }

    #[test]
    fn strip_csi_removes_cursor_moves() {
        let raw = b"\x1b[1Ctrust\x1b[3Cthis\x1b[2Cfolder\x1b[0m";
        let s = strip_csi(raw);
        assert!(s.contains("trust"));
        assert!(s.contains("folder"));
        assert!(!s.contains('\x1b'));
    }

    #[test]
    fn exit_codes_map() {
        assert_eq!(DriverError::StopTimeout.status().code(), 20);
        assert_eq!(DriverError::TranscriptUnavailable.status().code(), 10);
        assert_eq!(DriverError::Interrupted.status().code(), 130);
        assert_eq!(DriverError::Io(std::io::Error::other("x")).status().code(), 2);
    }
}
