//! The drive loop shared by the adapters that run a natively non-interactive
//! harness as a plain subprocess and read a JSONL event stream on its stdout
//! (codex, opencode). There is none of the PTY/hook/DEC machinery here: spawn
//! the child, feed it the prompt, hand each stdout line to the adapter's fold,
//! and hold the timeout and interrupts.
//!
//! What stays in each adapter is what genuinely differs: the argv, where the
//! prompt goes, and how the event stream folds into a `Summary`.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::adapters::{DriverError, procgroup};
use crate::args::{Options, OutputFormat};
use crate::signals;
use crate::transcript::Summary;

const POLL: Duration = Duration::from_millis(50);
/// Cap on captured stderr surfaced when a harness fails without a JSON error.
const STDERR_TAIL_CAP: usize = 8192;
/// Bounded wait for the detached stderr reader to drain before snapshotting the
/// tail for a failure diagnostic -- long enough to catch a fast startup/auth
/// failure, short enough to never stall a real run.
const STDERR_DRAIN_WAIT: Duration = Duration::from_millis(200);

/// What a subprocess run left behind once its stdout closed.
pub struct Finished {
    /// The child exited with status 0.
    pub success: bool,
    /// Every stdout line, verbatim, for the stream-json replay.
    pub replay: String,
    stderr: StderrTail,
}

impl Finished {
    /// The trimmed tail of the harness's stderr, for a failure that never
    /// reached the JSON stream (auth, startup, a bad flag). Waits a bounded time
    /// for the reader to drain first, so call it only when it is needed.
    pub fn stderr_tail(&self) -> String {
        self.stderr.snapshot()
    }
}

/// Whether this run writes stream-json to the caller live.
fn streaming(opts: &Options, stream_out: &Option<&mut (dyn Write + '_)>) -> bool {
    opts.output_format == OutputFormat::StreamJson && stream_out.is_some()
}

/// Spawn `cmd`, feed it `stdin` (or nothing), and pass each stdout line to
/// `on_line` until stdout closes. Under stream-json each line is also written
/// to `stream_out` as it arrives.
///
/// The child leads its own process group, so an interrupt or timeout tears
/// down the tools it spawned too, not only the top-level process.
pub fn run_jsonl(
    mut cmd: Command,
    stdin: Option<String>,
    opts: &Options,
    mut stream_out: Option<&mut (dyn Write + '_)>,
    mut on_line: impl FnMut(&str),
) -> Result<Finished, DriverError> {
    let start = Instant::now();
    let timeout = Duration::from_millis(opts.timeout_ms);
    let streaming = streaming(opts, &stream_out);

    // Harness stderr is mostly noise (skill-load errors, "Shell cwd was reset
    // ..."), so it is not passed through by default. The tail is still kept so a
    // failure that never reaches the JSON stream yields a diagnostic, and
    // `--debug` mirrors it live.
    let stdin_cfg = if stdin.is_some() { Stdio::piped() } else { Stdio::null() };
    cmd.stdin(stdin_cfg).stdout(Stdio::piped()).stderr(Stdio::piped());
    procgroup::lead_process_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| DriverError::Spawn(e.into()))?;

    // Write the prompt from its own thread, then close stdin by dropping the
    // handle so the harness starts the turn. A dedicated thread avoids a
    // deadlock when the prompt exceeds the pipe buffer and the child has started
    // writing stdout before draining stdin.
    if let (Some(prompt), Some(mut pipe)) = (stdin, child.stdin.take()) {
        thread::spawn(move || {
            let _ = pipe.write_all(prompt.as_bytes());
        });
    }

    let stderr = StderrTail::drain(child.stderr.take().expect("piped stderr"), opts.debug);

    // Read stdout lines on a thread so the loop below can honor the timeout and
    // interrupts even while a read would otherwise block.
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });

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
                on_line(line.trim_end());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader.join();
    let status = child.wait().map_err(DriverError::Io)?;
    Ok(Finished { success: status.success(), replay, stderr })
}

/// Close a stream-json run with the trailing `result` envelope. Returns whether
/// the run was streamed, so the caller does not print it a second time.
pub fn finish_stream(
    opts: &Options,
    mut stream_out: Option<&mut (dyn Write + '_)>,
    summary: &Summary,
    duration_ms: u64,
) -> Result<bool, DriverError> {
    if !streaming(opts, &stream_out) {
        return Ok(false);
    }
    let Some(w) = stream_out.as_mut() else {
        return Ok(false);
    };
    crate::emit::emit_result_envelope(*w, summary, duration_ms).map_err(DriverError::Io)?;
    let _ = w.flush();
    Ok(true)
}

/// A bounded byte tail of the child's stderr, filled by a detached reader.
///
/// Read in fixed-size chunks (not `read_until`) so a newline-free flood cannot
/// buffer an unbounded line before the cap applies. Kept as raw bytes: a
/// mid-UTF-8 cut or a stray non-UTF-8 byte must not abort the drain, so decoding
/// is lossy and happens only when surfacing. The thread is never joined -- a
/// tool descendant that inherits stderr and outlives the harness could hang a
/// join forever -- so a snapshot waits a bounded time instead.
struct StderrTail {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl StderrTail {
    fn drain(mut pipe: impl Read + Send + 'static, debug: bool) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (bytes_writer, done_writer) = (Arc::clone(&bytes), Arc::clone(&done));
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if debug {
                    let _ = std::io::stderr().write_all(&chunk[..n]);
                }
                if let Ok(mut tail) = bytes_writer.lock() {
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > STDERR_TAIL_CAP {
                        let cut = tail.len() - STDERR_TAIL_CAP;
                        tail.drain(..cut);
                    }
                }
            }
            done_writer.store(true, Ordering::SeqCst);
        });
        Self { bytes, done }
    }

    fn snapshot(&self) -> String {
        let deadline = Instant::now() + STDERR_DRAIN_WAIT;
        while !self.done.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        self.bytes
            .lock()
            .map(|t| String::from_utf8_lossy(&t).trim().to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    fn opts() -> Options {
        Options { timeout_ms: 10_000, ..Options::default() }
    }

    #[test]
    fn every_stdout_line_reaches_the_fold_and_the_replay() {
        let mut lines = Vec::new();
        let done = run_jsonl(sh("printf '{\"a\":1}\\n{\"b\":2}\\n'"), None, &opts(), None, |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert!(done.success);
        assert_eq!(lines, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
        assert_eq!(done.replay, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn the_prompt_arrives_on_stdin_and_stdin_closes() {
        // `cat` exits only once stdin closes, so finishing proves the close too.
        let mut lines = Vec::new();
        let prompt = "first line\n--flag-like second line".to_string();
        let done =
            run_jsonl(sh("cat"), Some(prompt), &opts(), None, |l| lines.push(l.to_string())).unwrap();
        assert!(done.success);
        assert_eq!(lines, vec!["first line", "--flag-like second line"]);
    }

    #[test]
    fn a_failed_child_reports_its_status_and_keeps_its_stderr() {
        let done =
            run_jsonl(sh("echo 'No API key found' >&2; exit 1"), None, &opts(), None, |_| {}).unwrap();
        assert!(!done.success);
        assert_eq!(done.stderr_tail(), "No API key found");
    }

    #[test]
    fn a_child_past_the_timeout_is_torn_down() {
        let short = Options { timeout_ms: 100, ..Options::default() };
        let started = Instant::now();
        let err = run_jsonl(sh("sleep 5"), None, &short, None, |_| {}).err().unwrap();
        assert!(matches!(err, DriverError::StopTimeout), "got: {err}");
        assert!(started.elapsed() < Duration::from_secs(3), "the child was not killed");
    }

    #[test]
    fn stream_json_lines_are_written_live_and_closed_with_a_result() {
        let o = Options { output_format: OutputFormat::StreamJson, ..opts() };
        let mut out = Vec::new();
        run_jsonl(sh("echo '{\"type\":\"x\"}'"), None, &o, Some(&mut out), |_| {}).unwrap();
        let summary = Summary {
            final_text: "hi".into(),
            session_id: String::new(),
            model: String::new(),
            is_error: false,
            num_turns: 1,
            total_cost_usd: 0.0,
            duration_api_ms: 0,
            usage: Default::default(),
            jsonl_replay: String::new(),
        };
        assert!(finish_stream(&o, Some(&mut out), &summary, 5).unwrap());
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(r#"{"type":"x"}"#));
        assert!(lines.next().unwrap().contains(r#""type":"result""#));
    }

    #[test]
    fn nothing_is_streamed_outside_stream_json() {
        let mut out = Vec::new();
        run_jsonl(sh("echo '{}'"), None, &opts(), Some(&mut out), |_| {}).unwrap();
        assert!(out.is_empty());
    }
}
