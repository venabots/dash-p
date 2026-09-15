# dash-p — Specification

A Rust CLI that puts **one non-interactive interface in front of any
coding-agent CLI**. It is a thin _adapter_, not an orchestrator: it normalizes
how you invoke and observe a one-shot agent run across harnesses while
preserving each agent's native behavior, and it makes the invocation surface
and the reporting trustworthy. Fan-out, parallelism, and synthesis stay with
the caller.

## 1. Premise & principles

The two things every harness is vague about — _what model actually ran_ and
_what was actually enforced_ — are the two an orchestrator most needs to trust.
dash-p's job is to tell the truth about both. Principles:

1. **Honest over uniform.** Where harnesses genuinely differ (enforcement
   strength, model identity), report the difference — never paper over it.
2. **Fail fast, never silently default.** An unknown model or an unmeetable
   enforcement demand errors out (exit 31 / 32) rather than silently degrading.
3. **One-shot first.** Sessions are an optional add-on, not the core model.
4. **stdout is sacred.** It carries only the agent's answer; metadata goes to
   `--meta-file`, logs to stderr.

Harnesses are driven by [`Adapter`]s in `src/adapters/`. Harnesses with a real
non-interactive mode are plain subprocess adapters: codex (`codex exec`) and
claude (`claude -p`, the default). The `--pty` flag selects a fallback adapter
for environments where the native non-interactive mode is unavailable; it
emulates print mode by driving the interactive TUI, which is where the PTY
machinery is needed:

1. A real PTY is required — Ink (claude's TUI runtime) bails on non-TTY stdin.
2. The terminal must answer DA1 / DA2 / XTVERSION / cursor-position /
   window-size probes during Ink startup, or the UI hangs.
3. We need a reliable "turn finished" signal, not screen-scraping.

We solve (1) with `portable-pty`, (2) with a small stateful ANSI responder
(`dec.rs`), and (3) with a `Stop` hook.

### Departure from the original

The Zig original typed the prompt into the TUI, which forced three timing
heuristics (wait-for-Ink-quiescence, Enter-debounce, bracketed-paste
handling). We pass the prompt as a **positional argument** so interactive mode
auto-submits it. The driver therefore has _no_ input-timing machinery; only
the `Stop` hook is load-bearing (`SessionStart` is just a readiness/debug
signal and a source of `transcript_path` for streaming).

## 2. Architecture

```
argv -> hook harness (FIFO + relay script + --settings)
     -> pty::spawn  claude "<prompt>" --settings <json>   [positional prompt]
        |                                   |
   main thread                         pump thread
   - poll FIFO for hook events         - read PTY master
   - on SessionStart: mark ready       - feed dec responder, write replies
   - on Stop: capture payload          - dismiss trust dialog (pre-session)
   - (stream) tail transcript -> stdout- detect child EOF
   - summarize, teardown
```

### Modules (`src/`)

| File                        | Responsibility                                                               |
| --------------------------- | ---------------------------------------------------------------------------- |
| `main.rs`                   | CLI entry; stdin prompt; adapter dispatch; format dispatch; exit codes.      |
| `args.rs`                   | Argparse; rejects `--settings`; forwards unknown flags.                      |
| `harness.rs`                | `--harness` selection; known names + custom path.                            |
| `adapters/mod.rs`           | `Adapter` trait; `for_harness` dispatch; shared `RunOutcome`/`DriverError`.  |
| `adapters/claude.rs`        | Default claude adapter: `claude -p`, parse the JSON result envelope.         |
| `adapters/claude_pty.rs`    | `--pty` fallback: PTY drive, pump thread, FIFO poll, Stop hook.              |
| `adapters/claude_common.rs` | Shared claude bits: bin resolution, perms flags, enforcement.                |
| `adapters/codex.rs`         | codex adapter: `codex exec --json`, fold events, rollout model lookup.       |
| `adapters/exec.rs`          | Shared subprocess loop: spawn, stdin prompt, stderr tail, timeout/interrupt. |
| `adapters/opencode.rs`      | opencode adapter: `opencode run --format json`, fold events (no OS sandbox). |
| `adapters/pi.rs`            | pi adapter: `pi -p --mode json`, prompt on stdin, fold events (no sandbox).  |
| `adapters/api/mod.rs`       | API adapters: one HTTP call per run, no tools; credential and base-URL rules. |
| `adapters/api/http.rs`      | The HTTP call (ureq) on a worker thread, held to `--timeout` and interrupts. |
| `adapters/api/anthropic.rs` | Anthropic Messages API: request, response, and error mapping.                |
| `adapters/api/openai.rs`    | OpenAI Chat Completions: request, response, and error mapping.               |
| `ansi.rs`                   | Terminal escape removal for text read from a harness.                        |
| `dec.rs`                    | Stateful DEC/XTerm query responder (carry buffer across reads).              |
| `hook.rs`                   | Temp dir + FIFO + relay script + inline `--settings` JSON; payload parse.    |
| `pty.rs`                    | PTY spawn (execs argv directly — no `sh -c`).                                |
| `stream.rs`                 | `read_at`-based transcript tailer (holds back torn lines).                   |
| `transcript.rs`             | Session JSONL parser → final text + usage + flags.                           |
| `emit.rs`                   | text / json / stream-json formatters.                                        |
| `signals.rs`                | SIGINT/SIGTERM → flag; lets the loop tear down and exit 130.                 |

Adapters live in `src/adapters/`: each backend agent CLI implements the
`Adapter` trait in its own module, so adding a harness is "drop a file in
`adapters/` and wire it into `for_harness`". The Claude protocol (PTY + Stop
hook) is one such adapter; harnesses with a real non-interactive mode (codex,
opencode, pi) are plain subprocess adapters with no PTY/hook machinery. They
share one drive loop (`adapters/exec.rs`) and differ only in argv, where the
prompt goes, and how the event stream folds. The API harnesses (`anthropic-api`,
`openai-api`) spawn nothing: they make one HTTP request in the format they are
named for, and report `no-tools` for every tier.

### 2.1 Concurrency

One pump thread owns both halves of the PTY master, so DEC responses and the
trust-dialog Enter are written from the same thread that reads — no
cross-thread re-entrancy and no mutex on the write path (the original needed
one because the PTY library owned the reader thread). The main thread owns the
FIFO and the child handle. Shared state is two atomics (`exited`,
`session_started`) and a diagnostics tail behind a mutex.

### 2.2 Completion + transcript race

`Stop` can fire a few ms before the assistant line is flushed. For `text` we
use the payload's `last_assistant_message` directly (no wait). For
`json`/`stream-json` we retry `parse_file` (≤40 × 50 ms) until non-empty,
falling back to the payload message.

### 2.3 Teardown

We kill the child's **process group** (it is a PTY session leader): SIGTERM,
≤300 ms grace, then SIGKILL, then reap. The temp dir/FIFO are removed by the
`HookHarness` `Drop`. SIGINT/SIGTERM set a flag the loop observes, so Ctrl-C
does not orphan the child.

### 2.4 Workspace-trust dialog

Detected by CSI-stripped substring match ("trust" + "folder") on pre-session
output only; dismissed with Enter. Gating to before `SessionStart` ensures a
later assistant message can never trigger a stray keystroke. (`--dangerously-
skip-permissions` does not suppress this dialog.)

## 3. Output fidelity

| Format        | Stdout                                                             |
| ------------- | ------------------------------------------------------------------ |
| `text`        | Final assistant message + `\n`.                                    |
| `json`        | `{answer, metadata}` — the answer plus the authoritative envelope. |
| `stream-json` | Transcript JSONL lines live, then the trailing `result` object.    |

### 3.1 Metadata side channel

`--meta-file <path>` writes the authoritative run metadata (`meta.rs`) as a
JSON object, distinct from the answer on stdout: `harness`, `drive`
(`print` | `exec` | `pty`, adapter-provided; `unknown` when no adapter ran),
`harness_version` (best-effort), `model_requested`,
`model_resolved`, `duration_ms`, `exit_status`, `session_id`, `num_turns`,
`total_cost_usd`, `usage`.
`model_resolved` is read from the transcript's assistant events
(`message.model`) — the launcher's truth, not the agent's self-report — and is
`"unknown"` rather than an echo when the harness never exposed it. Requesting a
meta file forces the transcript read even for `text` output.

**Per-harness reality.** The default adapters expose `model_resolved` + usage
authoritatively:

- **claude** (default, `claude -p`): the print-mode JSON envelope carries
  `result`, `usage`, `total_cost_usd`, `session_id`, and `modelUsage` — an
  object keyed by the model that actually ran. `model_resolved` is that key,
  read straight from claude's own output.
- **codex** (`codex exec`): usage from the `--json` stream; model from the
  session rollout file.
- **`--pty`** (the PTY fallback): claude writes its transcript only in print
  mode or on a clean TUI exit — **not** while the PTY-driven session is alive —
  and the Stop payload omits model + usage, so this fallback honestly reports
  `model_resolved: "unknown"` and usage `0`. Use the default (native) drive for
  authoritative metadata; reach for `--pty` only where `claude -p` is
  unavailable.

### 3.2 Permissions & enforcement

`--perms`/`--network` request a tier by intent (`policy.rs`). Each adapter maps
the tier to its harness's native mechanism and reports the **enforcement
class** it actually achieves — `os-sandbox`, `no-tools`, `agent-policy`, or
`none`. `no-tools` means the model has no tools at all, so it meets an
`os-sandbox` demand:

| intent            | codex                          | enforcement | claude                | enforcement  |
| ----------------- | ------------------------------ | ----------- | --------------------- | ------------ |
| `read-only`       | `--sandbox read-only`          | os-sandbox  | `--disallowedTools …` | agent-policy |
| `workspace-write` | `--sandbox workspace-write`    | os-sandbox  | bypassPermissions     | none         |
| `full`            | `--sandbox danger-full-access` | none        | bypassPermissions     | none         |

pi has no sandbox. `read-only` passes `--tools read,grep,find,ls` (agent-policy);
the write tiers keep pi's default tools and report `none`. opencode reports
`none` for every tier. The API harnesses report `no-tools` for every tier, and
`NetworkPlan::no_tools()` for every network tier (`effective: none`). The claim
covers this machine: a provider can still reach the web on its own side, which
dash-p cannot observe. `Adapter::has_tools` is false for them, so a bypass flag
does not downgrade the class.

claude's `read-only` denies the mutating tools
(`--disallowedTools "Edit Write NotebookEdit Bash WebFetch WebSearch"`) rather
than `--permission-mode plan`: plan mode silently overrides `--model` (it
substitutes its own), which would run a read-only review on the wrong model.
Tool denial keeps the requested model and the same agent-policy class.

`--require-enforcement <os-sandbox|any>` is checked before spawn
(`adapters::check_enforcement`): if the harness's class for the requested tier
is weaker than demanded, exit 32 (`enforcement-unsupported`). dash-p never
_adds_ a sandbox a harness lacks; where there is no mechanism it passes the tier
through and reports it unenforced.

Network is resolved separately, by `Adapter::network_plan` →
`policy::NetworkPlan { effective, enforcement }`:

| intent       | codex (with `--perms read-only\|workspace-write`)              | claude / opencode / pi |
| ------------ | -------------------------------------------------------------- | ---------------------- |
| `none`       | `-c sandbox_workspace_write.network_access=false` (os-sandbox) | none                   |
| `restricted` | `Err` → exit 32 (codex has no domain allowlist)                | none                   |
| `full`       | `-c sandbox_workspace_write.network_access=true`               | none                   |

The codex column holds only with a sandboxed `--perms` tier. `restricted` is
rejected either way — the tier has no codex equivalent, so `--perms` cannot
change that. Without `--perms`, codex resolves the sandbox mode from its own
config, which dash-p cannot confirm, so `none` and `full` both resolve to
`open()` — `full` / `none`.

`build_argv` pins that switch whenever `--network` is passed, so the request
beats a `network_access = true` in the user's `config.toml` — without the pin
the sandbox stays on and blocks the filesystem while the network is open, which
is a run that looks enforced and is not. codex's read-only sandbox has no such
switch and blocks network unconditionally, so `--perms read-only --network full`
resolves to `effective: none`; `--perms full` and
`--dangerously-skip-permissions` remove the sandbox and hold nothing.

`NetworkPlan.effective` never claims more restriction than dash-p can prove:
`NetworkPlan::open()` takes no tier, so an unenforced plan always reports `Full`
and `--network none` on claude cannot read like a real block. Under-claiming is
the safe direction.

An OS sandbox is only a guarantee while nothing can undo it, and the user's own
`config.toml` can undo it two ways. Both were observed:

- `approval_policy = "on-request"` with `approvals_reviewer = "auto_review"`
  routes a sandbox denial to an approving reviewer, which re-runs the command
  outside the sandbox. That defeated `--network none` and returned HTTP 200.
- `writable_roots = ["/Users/<you>"]` widened workspace-write far enough to
  write to `$HOME`.

So `build_argv` pins `sandbox_integrity_overrides()` — `approval_policy="never"`
and `sandbox_workspace_write.writable_roots=[]` — whenever the sandbox is
load-bearing. `sandbox_is_load_bearing` is true for exactly
`--perms read-only|workspace-write` without a bypass: precisely the runs dash-p
reports as `os-sandbox`. A bypass and `--perms full` remove the sandbox, and
without `--perms` nothing is promised, so those keep codex's own behavior. An
invariant test asserts the pin set matches the runs that claim `os-sandbox`.

`writable_roots` is codex's list of _additional_ roots, so `[]` leaves the
workspace and `$TMPDIR` writable and removes only config-added roots. `--add-dir`
is forwarded to codex's native flag, which keeps an explicit escape hatch open
now that the config route is pinned shut.

The claim's scope: `os-sandbox` covers the commands the agent runs. MCP servers
declared in the user's codex config run as codex's own subprocesses, outside that
sandbox, and `$TMPDIR` stays writable at the workspace-write tier. dash-p does
not claim either, and the README states both.

A tier the harness cannot express is rejected (exit 32) rather than rounded to a
neighbour; a tier it downgrades is warned on stderr and recorded. The
`perms`/`enforcement`/`network`/`network_effective`/`network_enforcement`
metadata fields carry the requested tiers, the tier that actually applied, and
the achieved classes (`null` when unrequested).

### 3.3 Exit codes

A stable API: `0` ok · `10` agent-error · `20` timeout · `30`
harness-not-found · `31` invalid-model · `32` enforcement-unsupported · `130`
interrupted · `2` internal. The `exit_status` metadata field carries the
matching label.

### 3.5 Model validation

`--model default` is the explicit way to request the harness's own default
(reported as `model_requested: "default"`); any other value passes through and
the harness validates it live. Neither harness exposes a model-enumeration
command, so validation is in-band: when the harness rejects the model the
adapter classifies it (`RunOutcome::invalid_model`) and exit is 31
(`invalid-model`) with the harness's own message surfaced, rather than the
generic agent-error. The codex adapter detects this from its `error`/
`turn.failed` event text; the claude PTY path does not yet classify it
distinctly.

## 3.4 Commands

`command.rs` parses argv into a `Command`: `run` (the default; bare prompt is
sugar), `list harnesses`, `list models [--harness X]`, `capabilities
[--harness X]`. Subcommands are recognised only as the first argument, so a
bare prompt still works. `capabilities` renders the per-harness perms ->
enforcement map, the per-tier network map (including `unsupported`), and output
modes from the `Adapter` trait, so callers stop hardcoding harness knowledge.
Both maps come from the same methods a run uses, so the advertised behavior
cannot drift from the enforced one. `list models` is best-effort: codex
has no enumeration command, so it reports the configured default from
`config.toml`; claude exposes none, so it points at the aliases.

## 4. Public surface

`Adapter::run(opts, stream_out) -> Result<RunOutcome, DriverError>`, dispatched
via `adapters::for_harness`. CLI flags map onto `Options` (see `args.rs`).
`-H`/`--harness` chooses the backend (`--agent` is forwarded to claude's own
subagent flag). Implemented:
`claude` (`claude -p`, or the `--pty` fallback: PTY + Stop hook) and `codex`
(`codex exec`, a plain subprocess reading the `--json` event stream); a custom
path is driven as a claude-compatible binary.
`DASHP_CLAUDE_BIN` overrides the `claude` binary. The codex adapter reads
`model_resolved` from codex's session rollout file (`turn_context.payload.model`)
since the event stream omits it.

## 5. Test plan

1. `dec` — recorded VT bytes → expected replies, incl. split-across-reads.
2. `transcript` — fixtures → final message + usage totals.
3. `hook` — settings JSON shape; FIFO/script lifecycle; payload extraction.
4. `args` — every flag and rejection.
5. `emit` — golden text/json shapes.
6. `stream` — tailer line-buffering across appends.
7. `tests/integration.rs` — real `claude`, gated on `DASHP_E2E=1`.

## 6. Non-goals

- Windows (no Unix PTY).
- Per-token streaming (print-mode-only).
- Tool-approval prompting (use `--dangerously-skip-permissions`/`--allowedTools`).

## 7. Risks

| Risk                                | Mitigation                                                    |
| ----------------------------------- | ------------------------------------------------------------- |
| Hook payload schema change          | Parse defensively; fall back to transcript / payload message. |
| New Ink startup probe               | Add a case to `dec::DecResponder::respond`.                   |
| Wrapper injects `--settings` (cmux) | `DASHP_CLAUDE_BIN` to bypass.                                 |
| Child outlives parent               | Process-group SIGTERM→SIGKILL; SIGINT handler.                |
