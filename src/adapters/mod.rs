//! Harness adapters.
//!
//! Each backend agent CLI ("harness") is driven by an [`Adapter`]. dash-p's
//! goal is one non-interactive interface in front of any coding agent, while
//! preserving each agent's native behavior -- so an adapter shells out to the
//! real harness binary rather than reimplementing it.
//!
//! # Adding a harness
//!
//! 1. Add a module here (`adapters/<name>.rs`) with a unit struct that
//!    implements [`Adapter`].
//! 2. Wire it into [`for_harness`] so `--harness <name>` resolves to it.
//! 3. Add the name to [`crate::harness::KNOWN_NAMES`] (and the [`Harness`]
//!    enum) so the `--harness` surface lists it.
//!
//! The shared, harness-agnostic types ([`RunOutcome`], [`DriverError`], and the
//! `Summary`/emit layer) live outside the adapter so every backend reports the
//! same envelope. Where harnesses genuinely differ -- enforcement strength,
//! model identity -- the adapter is responsible for reporting the difference
//! honestly rather than papering over it.

use std::io::Write;

use crate::args::Options;
use crate::harness::Harness;
use crate::policy::{Enforcement, Network, Perms};
use crate::transcript::Summary;

pub mod claude;
pub mod claude_common;
pub mod claude_pty;
pub mod codex;
pub mod opencode;
pub mod procgroup;

/// A backend agent CLI that dash-p can drive to run a single prompt to
/// completion. Implementations shell out to the native harness binary.
pub trait Adapter {
    /// Run a single prompt to completion. When `stream_out` is `Some` and the
    /// output format is stream-json, the adapter writes transcript lines to it
    /// live, followed by the trailing `result` envelope.
    fn run(
        &self,
        opts: &Options,
        stream_out: Option<&mut dyn Write>,
    ) -> Result<RunOutcome, DriverError>;

    /// How this adapter drives the harness, for the `drive` metadata field:
    /// `"print"` (native non-interactive), `"pty"`, or `"exec"`. Reported so a
    /// `"pty"` run's `unknown`/0 model+usage isn't mistaken for missing data.
    fn drive(&self) -> &'static str;

    /// The enforcement class this harness achieves for a permission tier --
    /// reported honestly (an OS sandbox vs merely agent policy vs nothing).
    fn perms_enforcement(&self, perms: Perms) -> Enforcement;

    /// The enforcement class for a network tier, given the permission tier in
    /// effect (some harnesses gate network only via their sandbox, so it
    /// depends on `perms`). `Full` network is never "enforced".
    fn network_enforcement(&self, perms: Option<Perms>, network: Network) -> Enforcement;
}

/// Resolve a harness to the adapter that drives it. Returns `None` for a
/// recognised-but-unimplemented harness, so the caller can fail fast with a
/// clear message instead of silently behaving like another backend.
///
/// `pty` is the undocumented `--pty` escape hatch: for claude (and a
/// claude-compatible [`Harness::Custom`] path) it selects the interactive
/// TUI-under-a-PTY drive instead of `claude -p`. Harnesses with no PTY drive
/// ignore it for now.
///
/// A [`Harness::Custom`] path is assumed claude-compatible and driven with the
/// Claude protocol (handy for a fork or a wrapper shim).
pub fn for_harness(harness: &Harness, pty: bool) -> Option<Box<dyn Adapter>> {
    match harness {
        Harness::Claude | Harness::Custom(_) if pty => {
            Some(Box::new(claude_pty::ClaudePtyAdapter))
        }
        Harness::Claude | Harness::Custom(_) => Some(Box::new(claude::ClaudeAdapter)),
        Harness::Codex => Some(Box::new(codex::CodexAdapter)),
        Harness::Opencode => Some(Box::new(opencode::OpencodeAdapter)),
        Harness::Gemini | Harness::Pi => None,
    }
}

/// Verify the harness can meet a `--require-enforcement` demand for the
/// requested perms/network tiers, *before* spawning anything. Returns an
/// explanatory message (for exit 32) when it cannot.
pub fn check_enforcement(adapter: &dyn Adapter, opts: &Options) -> Result<(), String> {
    let Some(req) = opts.require_enforcement else {
        return Ok(());
    };
    let harness = opts.harness.name();

    // A bypass flag (`--dangerously-skip-permissions`) disables the harness's
    // sandbox/policy outright, so no enforcement is actually achieved regardless
    // of the requested tier. Reflect that here rather than trusting the tier map.
    if opts.skip_permissions {
        return Err(format!(
            "{harness} cannot meet --require-enforcement {}: \
             --dangerously-skip-permissions bypasses all enforcement",
            req.label(),
        ));
    }

    if let Some(perms) = opts.perms {
        let actual = adapter.perms_enforcement(perms);
        if !req.satisfied_by(actual) {
            return Err(format!(
                "{harness} can only enforce {} via {}, not {}",
                perms.label(),
                actual.label(),
                req.label(),
            ));
        }
    }

    if let Some(network) = opts.network {
        let actual = adapter.network_enforcement(opts.perms, network);
        if !req.satisfied_by(actual) {
            return Err(format!(
                "{harness} can only enforce network={} via {}, not {}",
                network.label(),
                actual.label(),
                req.label(),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::RequireEnforcement;

    #[test]
    fn bypass_fails_any_enforcement_demand() {
        // codex read-only is normally an OS sandbox, but --dangerously-skip-
        // permissions bypasses it, so a --require-enforcement demand must fail.
        let opts = Options {
            perms: Some(Perms::ReadOnly),
            require_enforcement: Some(RequireEnforcement::OsSandbox),
            skip_permissions: true,
            harness: crate::harness::Harness::Codex,
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        let err = check_enforcement(adapter.as_ref(), &opts).unwrap_err();
        assert!(err.contains("dangerously-skip-permissions"), "got: {err}");
    }

    #[test]
    fn enforcement_holds_without_bypass() {
        let opts = Options {
            perms: Some(Perms::ReadOnly),
            require_enforcement: Some(RequireEnforcement::OsSandbox),
            harness: crate::harness::Harness::Codex,
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        assert!(check_enforcement(adapter.as_ref(), &opts).is_ok());
    }

    #[test]
    fn adapter_drive_labels() {
        assert_eq!(for_harness(&crate::harness::Harness::Claude, false).unwrap().drive(), "print");
        assert_eq!(for_harness(&crate::harness::Harness::Claude, true).unwrap().drive(), "pty");
        assert_eq!(for_harness(&crate::harness::Harness::Codex, false).unwrap().drive(), "exec");
    }
}

pub struct RunOutcome {
    pub summary: Summary,
    pub duration_ms: u64,
    /// True if stream-json output was already written live to the caller's
    /// stream writer; the caller must not re-emit.
    pub streamed: bool,
    /// True when the run failed specifically because the harness rejected the
    /// requested model -- mapped to exit 31 (`invalid-model`) rather than the
    /// generic agent-error.
    pub invalid_model: bool,
}

#[derive(Debug)]
pub enum DriverError {
    SessionStartTimeout,
    StopTimeout,
    ChildExitedEarly(String),
    TranscriptUnavailable,
    Interrupted,
    Spawn(anyhow::Error),
    Io(std::io::Error),
}

impl DriverError {
    /// Map to the stable run status (and thus exit code) for this failure.
    pub fn status(&self) -> crate::meta::ExitStatus {
        use crate::meta::ExitStatus;
        match self {
            Self::SessionStartTimeout | Self::StopTimeout => ExitStatus::Timeout,
            Self::TranscriptUnavailable => ExitStatus::AgentError,
            Self::Interrupted => ExitStatus::Interrupted,
            Self::ChildExitedEarly(_) | Self::Spawn(_) | Self::Io(_) => ExitStatus::Internal,
        }
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionStartTimeout => {
                write!(f, "timed out waiting for claude to start (no SessionStart hook fired)")
            }
            Self::StopTimeout => write!(f, "timed out waiting for the assistant to finish"),
            Self::ChildExitedEarly(tail) => {
                write!(f, "claude exited before finishing. Last output:\n{tail}")
            }
            Self::TranscriptUnavailable => {
                write!(f, "Stop fired but no assistant message was recoverable")
            }
            Self::Interrupted => write!(f, "interrupted"),
            Self::Spawn(e) => write!(f, "failed to spawn the agent binary: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for DriverError {}
