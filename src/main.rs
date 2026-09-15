//! dash-p: one non-interactive interface in front of any coding-agent CLI.
//!
//! A thin adapter, not an orchestrator: it normalizes how you *invoke* and
//! *observe* a one-shot agent run across harnesses while preserving each
//! agent's native behavior. stdout carries only the answer; the authoritative
//! run metadata (`--meta-file`) reports the truth about what model ran and what
//! was actually enforced -- the two things every harness is otherwise vague
//! about. Commands: `run` (default; bare prompt is sugar), `list`,
//! `capabilities`. Adapters live in `src/adapters/` (claude via `claude -p`, or
//! the undocumented `--pty` drive with a Stop hook; codex via `codex exec`;
//! opencode via `opencode run`; pi via `pi -p --mode json`).

mod adapters;
mod args;
mod command;
mod dec;
mod emit;
mod harness;
mod hook;
mod meta;
mod policy;
mod pty;
mod signals;
mod stream;
mod transcript;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use args::{Options, OutputFormat};
use command::Command;
use meta::{ExitStatus, Metadata};

/// Build the metadata envelope and stamp the best-effort harness version onto
/// it. Version probing spawns `<bin> --version`, so it is kept out of
/// `Metadata::build` (which stays hermetic for unit tests) and done here.
fn build_meta(
    opts: &Options,
    summary: Option<&transcript::Summary>,
    duration_ms: u64,
    status: ExitStatus,
    enforced: policy::Enforced,
    drive: &'static str,
) -> Metadata {
    let mut m = Metadata::build(opts, summary, duration_ms, status, enforced, drive);
    m.harness_version = opts.harness.probe_version();
    m
}

/// Write the authoritative metadata envelope to `--meta-file` when requested.
/// Best-effort: a write failure warns on stderr but does not change the run's
/// outcome (the answer on stdout is what matters).
fn write_meta_file(opts: &Options, metadata: &Metadata) {
    let Some(path) = &opts.meta_file else { return };
    let json = metadata.to_json().to_string();
    if let Err(e) = std::fs::write(path, format!("{json}\n")) {
        eprintln!("dash-p: failed writing --meta-file {path}: {e}");
    }
}

fn main() -> ExitCode {
    signals::install();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let command = match command::parse(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dash-p: {e}");
            return ExitCode::from(2);
        }
    };

    match command {
        Command::Run(opts) => run(*opts),
        Command::ListHarnesses => render(command::list_harnesses),
        Command::ListModels { harness } => render(|w| command::list_models(w, harness)),
        Command::Capabilities { harness } => render(|w| command::capabilities(w, harness)),
        Command::Help => render(|w| w.write_all(command::HELP.as_bytes())),
        Command::Version => render(|w| {
            writeln!(w, "{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        }),
    }
}

/// Run a discovery command (`list`/`capabilities`) to stdout.
fn render(f: impl FnOnce(&mut dyn Write) -> std::io::Result<()>) -> ExitCode {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match f(&mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dash-p: write failed: {e}");
            ExitCode::from(ExitStatus::Internal.code())
        }
    }
}

fn run(mut opts: Options) -> ExitCode {
    // Resolve the adapter that drives the selected harness. Reserved
    // harness names (codex, gemini, ...) have no adapter yet, so they fail
    // fast rather than silently behaving like claude.
    let adapter = match adapters::for_harness(&opts.harness, opts.pty) {
        Some(a) => a,
        None => {
            let implemented: Vec<&str> = harness::KNOWN_NAMES
                .iter()
                .copied()
                .filter(|n| adapters::for_harness(&harness::Harness::parse(n), false).is_some())
                .collect();
            eprintln!(
                "dash-p: the '{}' harness is recognised but not implemented yet \
                 (implemented: {}, or a path to a claude-compatible binary). \
                 Recognised names: {}.",
                opts.harness.name(),
                implemented.join(", "),
                harness::KNOWN_NAMES.join(", ")
            );
            write_meta_file(
                &opts,
                &build_meta(
                    &opts,
                    None,
                    0,
                    ExitStatus::HarnessNotFound,
                    policy::Enforced::default(),
                    "unknown",
                ),
            );
            return ExitCode::from(ExitStatus::HarnessNotFound.code());
        }
    };
    let drive = adapter.drive();

    // No positional prompt: read it from stdin (so multiline prompts and pipes
    // work without shell escaping).
    if opts.prompt.is_empty() {
        if std::io::stdin().is_terminal() {
            eprintln!("dash-p: no prompt given (pass a prompt argument or pipe one on stdin)");
            return ExitCode::from(2);
        }
        let mut s = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut s) {
            eprintln!("dash-p: failed reading stdin: {e}");
            return ExitCode::from(2);
        }
        opts.prompt = s.trim_end_matches('\n').to_string();
    }

    if opts.prompt.is_empty() {
        eprintln!("dash-p: empty prompt");
        return ExitCode::from(2);
    }

    // The enforcement class achieved for the requested perms tier (if any),
    // reported in metadata. A bypass flag disables enforcement outright, so
    // report `Unenforced` rather than the tier's nominal class (the preflight
    // in `check_enforcement` rejects the same combination up front).
    let perms_enforcement = opts.perms.map(|p| {
        if opts.skip_permissions {
            policy::Enforcement::Unenforced
        } else {
            adapter.perms_enforcement(p)
        }
    });

    // Resolve `--network` against the harness before spawning: a tier the
    // harness cannot express at all (codex has no domain allowlist, so
    // `restricted` is meaningless there) is a hard stop, not a silent downgrade
    // to whichever neighbouring tier happens to be implementable.
    let network = match adapters::resolve_network(adapter.as_ref(), &opts) {
        Ok(plan) => plan,
        Err(msg) => {
            eprintln!("dash-p: {msg}");
            write_meta_file(
                &opts,
                &build_meta(
                    &opts,
                    None,
                    0,
                    ExitStatus::EnforcementUnsupported,
                    policy::Enforced { perms: perms_enforcement, network: None },
                    drive,
                ),
            );
            return ExitCode::from(ExitStatus::EnforcementUnsupported.code());
        }
    };
    let enforced = policy::Enforced { perms: perms_enforcement, network };

    // A tier the harness *can* express but not at the level asked for is
    // reported in the envelope -- warn on stderr too, so a human running by
    // hand isn't left to infer it from a later confusing failure.
    if let (Some(requested), Some(plan)) = (opts.network, network)
        && plan.effective != requested
    {
        eprintln!(
            "dash-p: --network {} is unavailable with these settings; \
             running with network={} (enforcement: {})",
            requested.label(),
            plan.effective.label(),
            plan.enforcement.label(),
        );
    }

    // Fail fast, before spawning, if the harness can't meet a demanded
    // enforcement class. This is what turns "the prompt is a firewall, not a
    // sandbox" into an actual guarantee.
    if let Err(msg) = adapters::check_enforcement(adapter.as_ref(), &opts, network) {
        eprintln!("dash-p: {msg}");
        write_meta_file(
            &opts,
            &build_meta(&opts, None, 0, ExitStatus::EnforcementUnsupported, enforced, drive),
        );
        return ExitCode::from(ExitStatus::EnforcementUnsupported.code());
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // For stream-json, hand the driver our stdout so it can emit transcript
    // lines live as claude flushes them.
    let stream_arg: Option<&mut dyn Write> = if opts.output_format == OutputFormat::StreamJson {
        Some(&mut out)
    } else {
        None
    };

    match adapter.run(&opts, stream_arg) {
        Ok(outcome) => {
            let status = if outcome.invalid_model {
                ExitStatus::InvalidModel
            } else if outcome.summary.is_error {
                ExitStatus::AgentError
            } else {
                ExitStatus::Ok
            };
            let metadata = build_meta(
                &opts,
                Some(&outcome.summary),
                outcome.duration_ms,
                status,
                enforced,
                drive,
            );
            write_meta_file(&opts, &metadata);

            if !outcome.streamed {
                let res = match opts.output_format {
                    OutputFormat::Text => emit::emit_text(&mut out, &outcome.summary),
                    OutputFormat::Json => {
                        emit::emit_answer_json(&mut out, &outcome.summary, &metadata)
                    }
                    OutputFormat::StreamJson => {
                        // Reached only if no stream writer was available; fall
                        // back to a buffered replay + result envelope.
                        out.write_all(outcome.summary.jsonl_replay.as_bytes()).and_then(|_| {
                            emit::emit_result_envelope(&mut out, &outcome.summary, outcome.duration_ms)
                        })
                    }
                };
                if let Err(e) = res {
                    eprintln!("dash-p: write failed: {e}");
                    return ExitCode::from(ExitStatus::Internal.code());
                }
            }
            ExitCode::from(status.code())
        }
        Err(e) => {
            eprintln!("dash-p: {e}");
            let status = e.status();
            write_meta_file(&opts, &build_meta(&opts, None, 0, status, enforced, drive));
            ExitCode::from(status.code())
        }
    }
}
