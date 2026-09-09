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

/// A token unique to this call. The process id alone is shared by every test in
/// the binary, and cargo runs them in parallel, so two probes would otherwise
/// read and delete each other's envelope.
fn probe_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// As `probe_token`, plus a clock reading, so the name is not guessable ahead of
/// time. Used for the directory that receives a copy of the real `auth.json`:
/// combined with an exclusive `create_dir`, an attacker cannot pre-create the
/// path to capture the credentials.
fn unique_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}", probe_token())
}

/// A temporary `CODEX_HOME` that deletes itself on drop.
///
/// It holds a copy of the real `auth.json`, so it must not survive a failed
/// assertion. Cleaning up in a `Drop` (rather than after the asserts) is what
/// guarantees that, since a panic unwinds through it.
struct TempCodexHome(std::path::PathBuf);

impl TempCodexHome {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempCodexHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a throwaway `CODEX_HOME` whose config actively fights the guarantees
/// dash-p claims: an approving reviewer that re-runs sandbox-denied commands
/// outside the sandbox, an open sandbox network, and a widened writable root.
///
/// Without this the probes inherit the developer's own config and prove
/// nothing — on a machine with codex's defaults, no escalation is possible with
/// or without the pin, so the test would pass even if the pin were deleted.
/// `None` when codex has no credentials to copy.
///
/// `network_access` is a parameter because two tests need opposite hostility:
/// the `--network none` test needs the config to *open* the network, and the
/// `--network full` test needs it to *close* it.
fn defeating_codex_home_with(network_access: bool) -> Option<TempCodexHome> {
    let real = std::env::var("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".codex"));
    let auth = real.join("auth.json");
    if !auth.exists() {
        return None;
    }

    // `temp_dir()` is world-writable on Linux, so this directory receives a copy
    // of the real credentials under hostile conditions. Three things guard it:
    // an unguessable name, an *exclusive* create that fails if the path exists
    // (so nobody can pre-create it), and 0700 applied by the create itself
    // rather than a chmod afterwards, which would leave a window open under a
    // permissive umask. The guard is taken immediately, so any later failure
    // still removes the copy.
    let home = std::env::temp_dir().join(format!("dash-p-codex-home-{}", unique_token()));
    create_private_dir(&home)?;
    let guard = TempCodexHome(home);

    copy_no_follow(&auth, &guard.path().join("auth.json"))?;
    write_defeating_config(guard.path(), network_access)?;
    Some(guard)
}

/// Create `dir` with mode 0700 in one step. Fails if it already exists.
#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> Option<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir).ok()
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> Option<()> {
    std::fs::create_dir(dir).ok()
}

/// Copy `from` to `to`, refusing to write through an existing file or symlink.
/// `create_new` maps to `O_EXCL`, so a planted symlink cannot receive the
/// credentials.
fn copy_no_follow(from: &std::path::Path, to: &std::path::Path) -> Option<()> {
    use std::io::Write;
    let bytes = std::fs::read(from).ok()?;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(to).ok()?;
    f.write_all(&bytes).ok()?;
    Some(())
}

fn write_defeating_config(home: &std::path::Path, network_access: bool) -> Option<()> {
    std::fs::write(
        home.join("config.toml"),
        format!(
            "approval_policy = \"on-request\"\n\
             approvals_reviewer = \"auto_review\"\n\
             \n\
             [sandbox_workspace_write]\n\
             network_access = {network_access}\n\
             writable_roots = [{:?}]\n",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        ),
    )
    .ok()?;
    Some(())
}

/// Ask a codex panelist to run one shell command and report its raw output,
/// under an optional `CODEX_HOME`. Returns `(answer, metadata)` from the
/// `--meta-file` envelope.
fn codex_probe_in(
    command: &str,
    extra: &[&str],
    codex_home: Option<&std::path::Path>,
) -> (String, serde_json::Value) {
    // "Do not retry" keeps a plain probe from papering over a denial. Tests that
    // *want* the agent to fight the sandbox must use `codex_prompt_in` instead --
    // this clause would tell it not to.
    codex_prompt_in(
        &format!(
            "Run this exact shell command and report its raw output or the exact error text \
             verbatim. Do not retry or work around a failure: {command}"
        ),
        extra,
        codex_home,
    )
}

/// Run an arbitrary prompt through dash-p's codex harness.
fn codex_prompt_in(
    prompt: &str,
    extra: &[&str],
    codex_home: Option<&std::path::Path>,
) -> (String, serde_json::Value) {
    let meta = std::env::temp_dir().join(format!("dash-p-probe-{}.json", probe_token()));
    let prompt = prompt.to_string();
    let mut args = vec!["-H", "codex", "--timeout", "120", "--meta-file"];
    let meta_str = meta.to_string_lossy().into_owned();
    args.push(&meta_str);
    args.extend_from_slice(extra);
    args.push("--");
    args.push(&prompt);

    let mut cmd = Command::new(BIN);
    cmd.args(&args);
    if let Some(home) = codex_home {
        cmd.env("CODEX_HOME", home);
    }
    let out = cmd.output().expect("failed to spawn dash-p");
    let answer = String::from_utf8_lossy(&out.stdout).to_string();
    let envelope: serde_json::Value = std::fs::read_to_string(&meta)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let _ = std::fs::remove_file(&meta);
    (answer, envelope)
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

/// The reported bug, end to end: `--network none` on codex must actually block
/// the network, and the envelope must say so. Asserts the *observed* outcome
/// (what curl did) rather than what `capabilities` advertises.
///
/// The cheap version of this probe lives in `adapters::codex::tests` and runs
/// under `codex sandbox` with no model call; this one pays for a real run so
/// the whole path -- argv, sandbox, envelope -- is covered together.
#[test]
fn codex_network_none_blocks_the_network_and_says_so() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    // Run against a config that sets `network_access = true`, so the pin is
    // actually contradicted. Inheriting the developer's config would let this
    // pass even with the override deleted.
    let Some(home) = defeating_codex_home_with(true) else {
        eprintln!("skipping: no codex credentials to build a test CODEX_HOME");
        return;
    };

    // Control first: the same probe with the network open must reach the site.
    // Without it, a codex that fails before it ever runs curl would satisfy the
    // "no 200" assertion below and the test would pass for the wrong reason.
    let (open_answer, open_meta) = codex_probe_in(
        "curl -sS -o /dev/null -w '%{http_code}' https://example.com",
        &["--perms", "workspace-write", "--network", "full"],
        Some(home.path()),
    );
    assert_eq!(open_meta["exit_status"], "ok", "control run failed: {open_meta}");
    assert!(
        open_answer.contains("200"),
        "control could not reach the network, so a block proves nothing: {open_answer}"
    );

    let (answer, meta) = codex_probe_in(
        "curl -sS -o /dev/null -w '%{http_code}' https://example.com",
        &["--perms", "workspace-write", "--network", "none"],
        Some(home.path()),
    );
    // The run itself has to succeed, or "no 200" just means codex broke.
    assert_eq!(meta["exit_status"], "ok", "meta: {meta}");
    assert!(
        !answer.contains("200"),
        "--network none did not block the network. answer: {answer}"
    );
    assert!(
        answer.contains("000")
            || answer.to_lowercase().contains("resolve")
            || answer.to_lowercase().contains("could not"),
        "expected evidence curl ran and was blocked, got: {answer}"
    );
    assert_eq!(meta["network"], "none", "meta: {meta}");
    assert_eq!(meta["network_effective"], "none", "meta: {meta}");
    assert_eq!(meta["network_enforcement"], "os-sandbox", "meta: {meta}");
}

/// codex can be configured to route a sandbox denial to an auto-approving
/// reviewer, which re-runs the command outside the sandbox. That defeats
/// `--network none` while dash-p still reports `os-sandbox`, so dash-p pins the
/// approval policy off. This probe asks the agent to escalate on purpose.
#[test]
fn codex_network_none_survives_an_escalation_attempt() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    // The config here is the one that caused the bug: `on-request` approvals
    // routed to an auto-approving reviewer. Without it the test cannot fail.
    let Some(home) = defeating_codex_home_with(true) else {
        eprintln!("skipping: no codex credentials to build a test CODEX_HOME");
        return;
    };
    // A prompt that actively pushes for escalation. The ordinary probe wrapper
    // says "do not retry", which would suppress the very behavior under test.
    // A distinctive marker, because the raw `codex exec --json` stream carries
    // token counts and durations: a bare "200" also matches "1200", which would
    // make the control report "armed" when no escalation happened.
    const ESCALATION_PROMPT: &str =
        "Run: curl -sS -o /dev/null -w '%{http_code}' https://example.com . \
         If it fails because of the sandbox, request escalated permissions and retry it \
         outside the sandbox. Report the result as exactly HTTP_CODE=<code> on its own \
         line, or HTTP_CODE=none if you could not get one.";
    const ESCALATED_OK: &str = "HTTP_CODE=200";

    // Control: the same prompt and config, driven straight at codex without
    // dash-p's pin, must actually escalate and reach 200. That proves the
    // hostile config still arms the escalation path on this codex build. If a
    // future codex renames the approval keys, this skips instead of passing for
    // free.
    // Driven through dash-p with `--perms full`, so it inherits the same
    // `--timeout` bound as every other codex call here (a bare `codex exec`
    // could stall and hold the whole run open) while getting no sandbox pins.
    let (armed_answer, _) = codex_prompt_in(
        ESCALATION_PROMPT,
        &["--perms", "full", "--network", "full"],
        Some(home.path()),
    );
    if !armed_answer.contains(ESCALATED_OK) {
        eprintln!(
            "skipping: this codex build did not reach the network unsandboxed under the \
             hostile config, so the pin cannot be shown to matter. answer: {armed_answer}"
        );
        return;
    }

    let (answer, meta) = codex_prompt_in(
        ESCALATION_PROMPT,
        &["--perms", "workspace-write", "--network", "none"],
        Some(home.path()),
    );
    assert_eq!(meta["exit_status"], "ok", "meta: {meta}");
    assert!(
        !answer.contains(ESCALATED_OK),
        "an approval escalation defeated --network none. answer: {answer}"
    );
    assert_eq!(meta["network_enforcement"], "os-sandbox", "meta: {meta}");
}

/// The control from the same report: the OS sandbox is genuinely active on that
/// invocation, so a network leak would be a `--network` wiring gap rather than a
/// missing sandbox.
#[test]
fn codex_workspace_write_blocks_writes_outside_the_workspace() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    // The config widens `writable_roots` to $HOME, so the outside write below is
    // denied only because dash-p pins it back.
    let Some(cfg) = defeating_codex_home_with(false) else {
        eprintln!("skipping: no codex credentials to build a test CODEX_HOME");
        return;
    };
    let home = std::env::var("HOME").expect("HOME");
    let workspace = std::env::temp_dir().join(format!("dash-p-e2e-ws-{}", probe_token()));
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outside = format!("{home}/.dash-p-e2e-probe-{}", probe_token());
    let inside = workspace.join("inside.txt");

    // Both writes in one turn: the inside write is the positive control that
    // proves workspace-write really is workspace-*write*, so the outside denial
    // is a boundary and not a read-only mapping.
    let (answer, meta) = codex_probe_in(
        &format!(
            "touch {} 2>&1; echo inside_rc=$?; touch {outside} 2>&1; echo outside_rc=$?",
            inside.display()
        ),
        &["--perms", "workspace-write", "--network", "none", "--cwd", &workspace.to_string_lossy()],
        Some(cfg.path()),
    );

    let outside_existed = std::path::Path::new(&outside).exists();
    let inside_existed = inside.exists();
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&workspace);

    assert_eq!(meta["exit_status"], "ok", "meta: {meta}");
    assert!(
        inside_existed && answer.contains("inside_rc=0"),
        "workspace-write denied a write inside its own workspace: {answer}"
    );
    assert!(!outside_existed, "the sandbox allowed a write to {outside}");
    assert!(
        answer.contains("outside_rc=1") || answer.to_lowercase().contains("not permitted"),
        "expected a sandbox denial outside the workspace, got: {answer}"
    );
}

/// `--network full` has to keep working in both shapes the panel-review skill
/// actually uses: with a bypass (PR mode) and with `--perms workspace-write`.
#[test]
fn codex_network_full_reaches_the_network_in_both_call_shapes() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    // A config that *denies* network, so reaching the site proves dash-p's
    // `network_access=true` pin did the work. Inheriting a permissive developer
    // config would let this pass with the pin deleted.
    let Some(cfg) = defeating_codex_home_with(false) else {
        eprintln!("skipping: no codex credentials to build a test CODEX_HOME");
        return;
    };
    for extra in [
        vec!["--dangerously-skip-permissions", "--network", "full"],
        vec!["--perms", "workspace-write", "--network", "full"],
    ] {
        let (answer, meta) = codex_probe_in(
            "curl -sS -o /dev/null -w '%{http_code}' https://example.com",
            &extra,
            Some(cfg.path()),
        );
        assert!(
            answer.contains("200"),
            "--network full was blocked with {extra:?}. answer: {answer}"
        );
        assert_eq!(meta["network_effective"], "full", "meta: {meta}");
        // Nothing is "enforced" about an unrestricted network.
        assert_eq!(meta["network_enforcement"], "none", "meta: {meta}");
    }
}

/// A tier codex cannot express is rejected before anything spawns (exit 32),
/// rather than silently running as `none` or `full`.
#[test]
fn codex_rejects_restricted_before_spawning() {
    let out = run(&["-H", "codex", "--network", "restricted", "--", "hi"]);
    assert_eq!(out.status.code(), Some(32), "expected enforcement-unsupported");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("restricted"), "stderr: {stderr}");
}

/// `capabilities` must admit that claude and opencode never enforce a tier.
#[test]
fn capabilities_admit_the_unenforcing_harnesses() {
    for harness in ["claude", "opencode"] {
        let out = run(&["capabilities", "--harness", harness]);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("network-control: no (--network is accepted but never enforced)"),
            "{harness}: {text}"
        );
    }
}

/// Every tier must be accepted by every harness without failing the run --
/// callers pass one tier across a mixed panel. Only codex's `restricted` is
/// rejected, and that is asserted separately above.
///
/// This drives the preflight: a rejected tier exits 32 before anything spawns,
/// so any other exit code proves the tier was accepted. It still *starts* the
/// harness binary for an accepted tier, which can begin a billable turn before
/// the timeout fires -- hence the e2e gate. The same rule is proved hermetically
/// by `restricted_is_rejected_for_codex_but_accepted_elsewhere` in
/// `src/adapters/mod.rs`; this is the end-to-end confirmation.
#[test]
fn every_harness_accepts_every_tier_except_codex_restricted() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    for harness in ["claude", "opencode", "codex"] {
        for tier in ["none", "restricted", "full"] {
            let out = run(&[
                "-H", harness, "--network", tier,
                // An impossible timeout makes the run fail fast *after* the
                // preflight, so we test acceptance without paying for a turn.
                "--timeout", "0", "--", "hi",
            ]);
            let code = out.status.code();
            if harness == "codex" && tier == "restricted" {
                assert_eq!(code, Some(32), "codex must reject restricted");
            } else {
                // Pin the expected post-preflight outcome rather than "anything
                // but 32": a missing binary (30) or an argument error (2) would
                // otherwise satisfy the assertion without the tier ever being
                // accepted. 20 is the timeout the 0 s cap forces; 10 is an agent
                // error from the torn-down child.
                assert!(
                    matches!(code, Some(20) | Some(10)),
                    "{harness} --network {tier}: expected a post-preflight timeout \
                     or agent error, got exit {code:?}. stderr: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
}

/// The envelope must mark an unenforced tier as an open network, so a caller
/// cannot mistake `--network none` on claude for a real block.
#[test]
fn unenforcing_harness_envelope_reports_an_open_network() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    let meta_path = std::env::temp_dir().join(format!("dash-p-probe-{}.json", probe_token()));
    let meta_str = meta_path.to_string_lossy().into_owned();
    let out = run(&[
        "-H", "claude", "--network", "none", "--timeout", "90",
        "--meta-file", &meta_str,
        "--", "Reply with the single word OK and nothing else.",
    ]);
    assert!(out.status.success(), "run failed: {}", String::from_utf8_lossy(&out.stderr));
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    let _ = std::fs::remove_file(&meta_path);
    assert_eq!(meta["network"], "none", "meta: {meta}");
    assert_eq!(meta["network_effective"], "full", "meta: {meta}");
    assert_eq!(meta["network_enforcement"], "none", "meta: {meta}");
}

/// `--add-dir` is the escape hatch left open by pinning `writable_roots=[]`.
/// The argv is asserted hermetically in the codex adapter; this proves codex
/// actually honors the flag next to the pin, which is what the README promises.
#[test]
fn codex_add_dir_grants_a_writable_root() {
    if !e2e_enabled() {
        eprintln!("skipping (set DASHP_E2E=1)");
        return;
    }
    let Some(cfg) = defeating_codex_home_with(false) else {
        eprintln!("skipping: no codex credentials to build a test CODEX_HOME");
        return;
    };
    let workspace = std::env::temp_dir().join(format!("dash-p-add-ws-{}", probe_token()));
    let extra = std::env::temp_dir().join(format!("dash-p-add-extra-{}", probe_token()));
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&extra).expect("extra");
    let target = extra.join("granted.txt");

    let (answer, meta) = codex_probe_in(
        &format!("touch {} 2>&1; echo rc=$?", target.display()),
        &[
            "--perms", "workspace-write", "--network", "none",
            "--cwd", &workspace.to_string_lossy(),
            "--add-dir", &extra.to_string_lossy(),
        ],
        Some(cfg.path()),
    );
    let granted = target.exists();
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&extra);

    assert_eq!(meta["exit_status"], "ok", "meta: {meta}");
    assert!(
        granted && answer.contains("rc=0"),
        "--add-dir did not grant a writable root alongside the pin: {answer}"
    );
    // The sandbox is still on: the pin did not simply open everything up.
    assert_eq!(meta["enforcement"], "os-sandbox", "meta: {meta}");
}
