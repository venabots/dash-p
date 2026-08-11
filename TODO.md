# TODO

Tracking known gaps and future work. The spike works end-to-end (text / json /
stream-json validated against the real `claude` binary), but several things are
deliberately incomplete.

## Known limitations (from the spike)

- [x] **claude metadata (`model_resolved`/usage).** RESOLVED. The default
      `claude` harness now drives `claude -p` (print mode); its JSON envelope's
      `modelUsage` key is the authoritative `model_resolved`, and usage/cost
      come straight from it. The PTY drive moved behind the `--pty` flag as
      a fallback. (Background: claude writes its transcript only in print mode
      or on a clean TUI exit — never while a PTY-driven session is alive, and
      the Stop payload omits model + usage; verified the transcript never
      appears even after 15s alive, and SIGTERM/SIGINT don't flush it. So a
      `--pty` run honestly reports `unknown`/0; use the default for metadata.)

- [ ] **Flag passthrough fidelity (partial).** `--flag=value` and a known set
      of claude value-flags (`KNOWN_VALUE_FLAGS` in `src/args.rs`) now forward
      with their values. Remaining gap: a _space-separated_ value for an
      _unrecognised_ flag is still absorbed into the prompt. Workaround:
      `--flag=value`. Fix: extend the set, or track arity generically.
- [ ] **Real prebuilt-binary download.** `scripts/install.js` is a no-op stub.
      Needs a GitHub release + CI matrix building `aarch64/x86_64 × macos/linux`,
      then a real fetch keyed on `process.platform`/`process.arch`.
- [ ] **E2E flakiness.** Occasionally one of the three integration tests fails
      with a non-zero exit when real sessions run back-to-back (transient API /
      startup hiccup, not a code bug). Consider a bounded retry in the test
      harness, and/or `--test-threads=1` spacing.

## Feature parity with the original

- [ ] **Session reuse:** `--resume <id>`, `--continue`, `--session-id <uuid>`.
- [ ] **More forwarded flags** with explicit handling (so values survive):
      `--allowedTools`, `--disallowedTools`, `--system-prompt`,
      `--append-system-prompt`, `--permission-mode`, `--fallback-model`,
      `--add-dir` (repeatable), `--mcp-config` (repeatable), `--setting-sources`,
      `--verbose`, `--max-turns`.
- [ ] **`--input-file <path>`** for multiline prompts (stdin already works).
- [ ] **Golden-output tests** asserting byte-for-byte parity with `claude -p`
      for text and json (capture fixtures from the real CLI).

## Robustness / polish

- [ ] **Auth / startup error handling (fast-fail).** Today there is no
      auth-specific handling. Three paths, verified with fake binaries:
      (a) claude prints an error and _exits_ → surfaced generically as
      `ChildExitedEarly` (exit 2) with a raw CSI-stripped output tail;
      (b) claude drops into an _interactive_ auth prompt and waits (e.g.
      expired OAuth) → we hang until `--timeout` (default 300s!) then exit 124
      — the worst case; (c) an API error that still completes a turn
      (`is_error: true`) → handled, exit 1. Fix: in `pump_loop`'s
      pre-SessionStart scan (same place as the trust dialog), match startup
      error markers (`Invalid API key`, `Please run /login`,
      `OAuth token expired`, `credit balance`, `Unauthorized`, `rate limit`)
      and tear down immediately with a clean message + a distinct
      `DriverError::StartupError` exit code, instead of waiting for the
      timeout. Caveat: screen-scraping, so same fragility as trust detection;
      gate to pre-session to avoid matching prompt/answer text.
- [ ] **Replace the 10ms main-loop sleep with `poll()`** on the FIFO fd (woken
      by a self-pipe/eventfd when the pump thread queues nothing — here only the
      FIFO matters since the pump owns PTY writes). Lower latency, no spin.
- [ ] **Forward SIGINT to the child** as well as flagging it, so Ctrl-C
      interrupts an in-flight turn promptly rather than only at teardown.
- [ ] **Trust-dialog handling:** investigate a config-based pre-trust (without
      polluting `~/.claude`) so we can drop the screen-scrape entirely; failing
      that, tighten the matcher to the exact dialog string.
- [ ] **Resilience to hook-payload schema drift:** add a regression fixture and
      keep the transcript fallback paths covered.
- [ ] **`--add-dir` / cwd semantics:** confirm the child's working directory
      matches user expectation (observed claude reporting `cwd: $HOME` in one run).

## Out of scope (documented non-goals)

- Windows (needs a Unix PTY; `portable-pty` supports ConPTY but the hook/FIFO
  plumbing is Unix-only today).
- Per-token streaming within a single assistant message (print-mode only, via
  `--include-partial-messages`).
- Tool-approval prompting (use `--dangerously-skip-permissions` /
  `--allowedTools`).

## Notes

- On machines where `claude` on PATH is a wrapper that injects its own
  `--settings` (e.g. cmux), set `DASHP_CLAUDE_BIN=/path/to/real/claude`.
