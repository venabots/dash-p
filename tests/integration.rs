//! End-to-end tests against the real `claude` binary. No mocks: a mock would
//! only mirror our assumptions about claude, not claude itself.
//!
//! Gated on `DASHP_E2E=1` so a plain `cargo test` stays hermetic. Point at
//! a specific binary with `DASHP_CLAUDE_BIN=/path/to/claude` (required on
//! machines where `claude` on PATH is a wrapper that injects its own
//! `--settings`, e.g. cmux).
//!
//! Run:
//!   DASHP_E2E=1 DASHP_CLAUDE_BIN=/path/to/claude \
//!     cargo test --test integration -- --test-threads=1 --nocapture

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_dash-p");

fn e2e_enabled() -> bool {
    std::env::var("DASHP_E2E").as_deref() == Ok("1")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("failed to spawn dash-p")
}

#[test]
fn text_mode_returns_answer() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    let out = run(&[
        "--dangerously-skip-permissions",
        "--timeout",
        "90",
        "Reply with the single word OK and nothing else.",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "non-zero exit. stderr: {stderr}");
    assert!(
        stdout.to_uppercase().contains("OK"),
        "expected OK in stdout, got: {stdout:?}"
    );
}

#[test]
fn json_mode_is_well_formed() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    let out = run(&[
        "--output-format",
        "json",
        "--dangerously-skip-permissions",
        "--timeout",
        "90",
        "Reply with the single word OK and nothing else.",
    ]);
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is not valid JSON");
    assert_eq!(v["type"], "result");
    assert_eq!(v["is_error"], false);
    assert!(v["result"].as_str().unwrap().to_uppercase().contains("OK"));
    assert!(!v["session_id"].as_str().unwrap().is_empty());
    // Usage should be populated from the transcript.
    assert!(v["usage"]["output_tokens"].as_u64().unwrap() > 0);
}

#[test]
fn stream_json_emits_lines_then_result() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    let out = run(&[
        "--output-format",
        "stream-json",
        "--dangerously-skip-permissions",
        "--timeout",
        "90",
        "Reply with the single word OK and nothing else.",
    ]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(lines.len() >= 2, "expected several JSONL lines, got: {stdout:?}");
    // Every line is valid JSON.
    for l in &lines {
        serde_json::from_str::<serde_json::Value>(l)
            .unwrap_or_else(|e| panic!("invalid JSONL line {l:?}: {e}"));
    }
    // The last line is the result envelope.
    let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!(last["type"], "result");
}
