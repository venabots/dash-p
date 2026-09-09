# dash-p

One non-interactive interface in front of any coding agent.

## Use

```bash
dash-p "your prompt here"
dash-p --output-format json "summarize this" < diff.txt
dash-p --output-format stream-json "audit src/" | jq .
dash-p --model opus "explain quicksort to a 10-year-old"
dash-p --harness claude "which harness am I?"
```

If no prompt argument is given, the prompt is read from stdin.

## Commands

```bash
dash-p "<prompt>"                 # sugar for `run` with defaults
dash-p run [flags] -- "<prompt>"  # explicit run
dash-p list harnesses             # installed + implemented/reserved + version
dash-p list models [--harness X]  # best-effort model discovery
dash-p capabilities [--harness X] # per-harness perms->enforcement, network, outputs
dash-p --help | --version
```

`run`/`list`/`capabilities` are recognised only as the first argument (like
git); any other first token is treated as a prompt, so a prompt starting with
one of those words can be forced with `dash-p run -- "run the tests"`.
`capabilities` is what lets an orchestrator stop hardcoding harness knowledge:

```
$ dash-p capabilities --harness codex
harness: codex
perms:
  read-only        os-sandbox
  workspace-write  os-sandbox
  full             none
network (with --perms workspace-write):
  none             os-sandbox
  restricted       unsupported
  full             none
network-control: yes (sandbox blocks network)
output-modes: text, json, stream-json
```

## Harnesses

`-H` / `--harness <name|path>` selects which agent CLI to drive. (`--agent` is
left alone so it forwards to claude's own `--agent <subagent>` flag.) Implemented
today:

- **`claude`** (default) — `claude -p` print mode, a plain subprocess.
  Authoritative metadata: model, usage, and cost come straight from claude's
  own JSON envelope.
- **`codex`** — the natively non-interactive `codex exec`, a plain subprocess.
- **`opencode`** — `opencode run --format json`, a plain subprocess. opencode
  has no OS sandbox and doesn't expose the resolved model, so enforcement is
  reported as `agent-policy` at best and `model_resolved` as `unknown` — see
  Caveats.

`gemini` and `pi` are recognised and reserved (selecting one fails fast until
it's wired up). A value that isn't a known name is treated as a path to a
**claude-compatible** binary and driven via `claude -p` — handy for a fork or a
wrapper shim. The default is `claude`.

Passing **`--pty`** drives the agent's interactive TUI under a PTY instead of
its native non-interactive mode — a fallback for environments where the latter
(e.g. `claude -p`) is unavailable. It can't expose model/usage (see Caveats), so
prefer the native drive.

## How it works

The default **`claude`** harness simply runs `claude -p --output-format json`
and parses the result envelope (answer, usage, cost, and the `modelUsage` key
that gives the authoritative model). codex similarly runs `codex exec --json`,
and opencode runs `opencode run --format json`; both fold their JSONL event
streams into the same answer + metadata envelope.
Neither needs a PTY.

The **`--pty`** fallback is the original mechanism — driving the interactive TUI
under a PTY, for environments where `claude -p` doesn't work:

1. Spawns `claude "<prompt>" --settings '<inline-json>'` on a real PTY
   (`openpty`/`forkpty` via `portable-pty`). The prompt is a positional arg,
   so interactive mode submits it immediately.
2. A small ANSI responder answers the DA1 / DA2 / DSR / XTVERSION / window-size
   queries Ink issues at startup (it is _stateful_ across reads, so a query
   split across a PTY read boundary is still answered). Without these the TUI
   hangs.
3. Registers `SessionStart` and `Stop` hooks via `--settings` — never touches
   your `~/.claude/` config. A relay script appends the hook payload to a
   per-run FIFO the driver polls.
4. On `Stop`, reads the final assistant message (from the payload's
   `last_assistant_message` for text, or the transcript JSONL for json /
   stream-json), prints it, and tears the child's process group down.

## Flags

```
--harness <name|path> | -H                claude (default) | codex | … | /path
--output-format <text|json|stream-json>   default: text
--model <name>
--dangerously-skip-permissions
--perms <read-only|workspace-write|full>   permission tier (by intent)
--network <none|restricted|full>           network tier (by intent)
--require-enforcement <os-sandbox|any>     demand an enforcement class (else exit 32)
--cwd <path>                               working directory for the child
--meta-file <path>                         write the run-metadata envelope here
--timeout <seconds>                        wrapper wall-time cap (default 300)
--pty                                      drive the interactive TUI under a PTY
                                           (when native non-interactive mode is unavailable)
--cols <n> / --rows <n>                    PTY size (with --pty; default 120x40)
--debug | -d                               wrapper debug traces on stderr
--                                         end-of-options; rest is the prompt
```

Unrecognised flags are forwarded to `claude`. `-p`/`--print` is accepted but
ignored — dash-p already emulates print mode, so the flag is redundant, and
swallowing it lets callers that invoke `claude -p "..."` point at dash-p
unchanged. A user-supplied `--settings` is rejected (we inject our own settings
for the Stop hook).

`--model default` is the explicit way to ask for the harness's own default
(reported as `model_requested: "default"`); any other value passes through and
the harness validates it live. When the harness rejects a model, exit is `31`
with its own message — e.g. codex's _"The 'x' model is not supported…"_.

> **Note:** `--flag=value` works for any flag, and common claude value-flags
> (`--allowedTools`, `--system-prompt`, `--add-dir`, `--resume`, …) forward
> with their values. A _space-separated_ value for an _unrecognised_ flag is
> the one remaining gap — pass it as `--flag=value`.

## Permissions & enforcement

`--perms` requests a permission tier _by intent_; each harness maps it to its
native mechanism, and the metadata reports the **enforcement class** actually
achieved — honestly, instead of a uniform-looking flag that lies.

| intent            | codex (`codex exec`)                     | claude                               |
| ----------------- | ---------------------------------------- | ------------------------------------ |
| `read-only`       | `--sandbox read-only` (os-sandbox)       | `--disallowedTools …` (agent-policy) |
| `workspace-write` | `--sandbox workspace-write` (os-sandbox) | bypassPermissions (none)             |
| `full`            | `--sandbox danger-full-access` (none)    | bypassPermissions (none)             |

`--require-enforcement os-sandbox` makes the difference enforceable: it fails
fast (exit 32) when the harness can't meet the demand, before anything runs.

```bash
dash-p --harness claude --perms read-only --require-enforcement os-sandbox "…"
# dash-p: claude can only enforce read-only via agent-policy, not os-sandbox
# (exit 32)
```

It does not _add_ a sandbox to a harness that lacks one: where a harness has no
mechanism, the tier is passed through and reported unenforced rather than
faked.

**The sandbox is pinned when it is the promise.** A codex sandbox is only a
guarantee while the user's own `config.toml` cannot undo it, and it can, two
ways — both observed here:

- `approval_policy = "on-request"` with `approvals_reviewer = "auto_review"`
  sends a sandbox denial to an approving reviewer, which re-runs the command
  _outside_ the sandbox. A `--network none` run escalated and fetched HTTP 200.
- `writable_roots = ["/Users/<you>"]` widened `workspace-write` enough to write
  to `$HOME`.

So whenever dash-p reports `os-sandbox` it also passes
`-c approval_policy="never"` and `-c sandbox_workspace_write.writable_roots=[]`.
That is exactly `--perms read-only|workspace-write` without a bypass. A bypass
and `--perms full` remove the sandbox, and without `--perms` dash-p promises
nothing, so those keep codex's own behavior untouched.

The trade: under `--perms`, a sandboxed run can no longer escalate to install a
dependency, and extra `writable_roots` from your config do not apply. That is
what asking for a sandbox means. `--add-dir` still works — it is explicit caller
intent, and it is forwarded to codex's own flag — so a run that genuinely needs a
second writable root can still say so.

**What `os-sandbox` covers, exactly.** It means codex's sandbox holds for the
commands the agent runs: the filesystem boundary, the network switch, the
writable roots, and the approval path that could undo any of them. Two things
sit outside that boundary, and dash-p does not claim them:

- **MCP servers.** Servers in your codex config run as codex's own subprocesses,
  not under the command sandbox. A configured MCP server with network or
  filesystem access is a path the seatbelt does not cover. Run with a
  `CODEX_HOME` that has no MCP servers if that matters to you.
- **`$TMPDIR`.** codex's workspace-write tier makes the temp dir writable by
  design, and dash-p keeps that (pinning it off would break ordinary tool use).

Neither is introduced by dash-p, but `os-sandbox` should be read as "codex's
sandbox, held to the tier you asked for", not "nothing can reach out".

### Network

`--network` requests an egress tier the same way. Only codex can hold one, and
only through its sandbox:

| intent       | codex                                                          | claude / opencode |
| ------------ | -------------------------------------------------------------- | ----------------- |
| `none`       | `-c sandbox_workspace_write.network_access=false` (os-sandbox) | not enforced      |
| `restricted` | rejected — no domain allowlist exists (exit 32)                | not enforced      |
| `full`       | `-c sandbox_workspace_write.network_access=true`               | not enforced      |

dash-p pins that switch explicitly whenever `--network` is passed, so the
caller's intent beats a `[sandbox_workspace_write] network_access = true` in
the user's own `~/.codex/config.toml`. Without the pin the sandbox stays on and
keeps blocking the filesystem while the network is wide open — a run that
_looks_ sandboxed but is not.

Two combinations to know:

- **`--perms read-only`** blocks network unconditionally (codex exposes the
  switch only for workspace-write), so `--network full` there really runs with
  no network. dash-p warns on stderr and records `network_effective: "none"`
  rather than letting it pass silently.
- **`--perms full`** and `--dangerously-skip-permissions` remove the sandbox
  altogether, so no tier is held whatever was requested.

`restricted` is rejected on codex instead of being quietly rounded to `none` or
`full`. claude and opencode accept every tier without failing — callers pass one
tier across mixed harnesses.

**`network_effective` never claims more restriction than dash-p can prove.**
When nothing enforces the tier, the run really does have an open network, so the
envelope reports `network_effective: "full"` with `network_enforcement: "none"`
— never an echo of the requested tier. So `--network none` on claude reports
`full`/`none`, which is the truth, and dash-p warns on stderr. The same applies
to codex without `--perms`: the sandbox mode then comes from codex's own config,
which dash-p cannot confirm, so it claims nothing. Pass `--perms` to get a tier
that is actually held.

## Output contract

- **stdout** carries only the agent's answer (`text`), or `{answer, metadata}`
  (`--output-format json`).
- **`--meta-file <path>`** writes the authoritative run-metadata envelope to a
  side channel, distinct from the answer:

  ```json
  {
    "harness": "codex", "drive": "exec", "harness_version": "codex-cli 0.153.4",
    "model_requested": "default", "model_resolved": "gpt-6-astra",
    "perms": "workspace-write", "enforcement": "os-sandbox",
    "network": "none", "network_effective": "none",
    "network_enforcement": "os-sandbox",
    "duration_ms": 84213, "exit_status": "ok",
    "session_id": "…", "num_turns": 1, "total_cost_usd": 0.04,
    "usage": { "input_tokens": 1200, "output_tokens": 800, … }
  }
  ```

  `model_resolved` is read from the transcript (the launcher's truth), not the
  agent's self-report; it is `"unknown"` when the harness never exposed it.

  The policy fields separate what was asked for from what was actually done:
  `perms`/`network` are the requested tiers, `network_effective` is the tier
  that really applied, and `enforcement`/`network_enforcement` are the classes
  achieved — `os-sandbox`, `agent-policy`, or `none`. A caller can therefore
  tell a real sandbox from a flag that did nothing. The two cases to branch on:

  - `network_enforcement: "os-sandbox"` — the tier in `network_effective` is
    genuinely held. It can be _stricter_ than requested (codex read-only blocks
    egress whatever you asked for), and dash-p warns on stderr when it is.
  - `network_enforcement: "none"` — nothing held the tier, so
    `network_effective` reads `"full"`. Never an echo of the request.

  All five are `null` when the tier was not requested.
  `drive` is adapter-provided — `"print"` (claude native), `"exec"` (codex), or
  `"pty"` for the `--pty` fallback (`"unknown"` when no adapter ran) — so a
  `"pty"` run's `unknown`/0 model+usage reads as a mode limitation, not missing
  data (and it never claims `"pty"` for a harness with no PTY drive).

## Exit codes

Exit codes are a stable API orchestrators can branch on.

| Code  | `exit_status`             | Meaning                                       |
| ----- | ------------------------- | --------------------------------------------- |
| `0`   | `ok`                      | Success.                                      |
| `10`  | `agent-error`             | Assistant errored, or no message recoverable. |
| `20`  | `timeout`                 | Timed out (before or after the UI came up).   |
| `30`  | `harness-not-found`       | The selected harness has no adapter.          |
| `31`  | `invalid-model`           | Harness rejected the requested model.         |
| `32`  | `enforcement-unsupported` | Harness can't meet `--require-enforcement`.   |
| `130` | `interrupted`             | Interrupted (SIGINT/SIGTERM).                 |
| `2`   | `internal`                | Wrapper internal error (spawn/PTY/IO).        |

## Caveats

- **macOS / Linux only** (no Windows; needs a Unix PTY).
- **Requires `claude` on `$PATH`** (or set `DASHP_CLAUDE_BIN`, below).
- **`--pty` can't report model/usage.** claude writes its transcript only
  in print mode or on a clean TUI exit — not while the PTY session is alive, and
  the Stop payload omits both — so a `--pty` run honestly reports
  `model_resolved: "unknown"` and usage `0`. Use the default (native) drive
  for authoritative metadata.
- **Per-message streaming, not per-token.** `stream-json` emits transcript
  lines as `claude` flushes them, then a trailing `result` envelope.
  Per-token streaming needs `claude -p --include-partial-messages`, which is
  print-mode only.
- **API instability.** `claude` is not designed to be driven this way. A
  release that changes the hook payload or adds a new startup terminal probe
  can break this; failures surface rather than hide.

### `DASHP_CLAUDE_BIN`

If `claude` on your `PATH` is a wrapper that injects its own `--settings`
(e.g. the **cmux** shim), it will clobber ours and no hooks fire. Point
directly at the real binary:

```bash
DASHP_CLAUDE_BIN=/path/to/real/claude dash-p "say hi"
```

Equivalently, point `--harness` straight at the real binary:
`dash-p --harness /path/to/real/claude "say hi"`.

## Build & test

```bash
cargo build --release          # binary at target/release/dash-p
cargo test                     # unit tests (hermetic, no claude needed)

# End-to-end against the real claude binary:
DASHP_E2E=1 DASHP_CLAUDE_BIN=/path/to/claude \
  cargo test --test integration -- --test-threads=1
```

## Install

Via the Homebrew tap (builds from source with the Rust toolchain):

```bash
brew install venabots/tap/dash-p
```

Or straight from source:

```bash
cargo install --path .
```

**Releasing.** Push a `vX.Y.Z` tag. `.github/workflows/bump-tap.yml` recomputes
the source tarball's sha and repoints the [`venabots/homebrew-tap`](https://github.com/venabots/homebrew-tap)
formula at the new release. Requires a `HOMEBREW_TAP_TOKEN` repo secret with
write access to the tap.

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## License

MIT.
