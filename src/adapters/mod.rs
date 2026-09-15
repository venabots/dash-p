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
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
use crate::transcript::Summary;

pub mod claude;
pub mod claude_common;
pub mod claude_pty;
pub mod codex;
pub mod exec;
pub mod opencode;
pub mod pi;
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

    /// The network tier that actually applies, and how strongly it is held.
    ///
    /// Depends on `perms` (some harnesses gate network only through their
    /// sandbox) and on `bypass` (`--dangerously-skip-permissions` removes the
    /// sandbox, and with it any network control). `Err(reason)` when the
    /// harness cannot express the requested tier at all -- rejected with exit
    /// 32 rather than silently downgraded to something else.
    fn network_plan(
        &self,
        perms: Option<Perms>,
        network: Network,
        bypass: bool,
    ) -> Result<NetworkPlan, String>;
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
        Harness::Pi => Some(Box::new(pi::PiAdapter)),
        Harness::Gemini => None,
    }
}

/// Resolve the requested `--network` tier against the harness: the tier that
/// will actually apply and how strongly it is held. `Ok(None)` when no tier was
/// requested; `Err` when the harness cannot express the requested one (exit 32).
pub fn resolve_network(
    adapter: &dyn Adapter,
    opts: &Options,
) -> Result<Option<NetworkPlan>, String> {
    let Some(network) = opts.network else {
        return Ok(None);
    };
    adapter
        .network_plan(opts.perms, network, opts.skip_permissions)
        .map(Some)
        .map_err(|why| format!("{}: {why}", opts.harness.name()))
}

/// Verify the harness can meet a `--require-enforcement` demand for the
/// requested perms/network tiers, *before* spawning anything. Returns an
/// explanatory message (for exit 32) when it cannot. `network` is the
/// already-resolved plan from [`resolve_network`], so the demand is checked
/// against the tier that will really apply rather than the one asked for.
pub fn check_enforcement(
    adapter: &dyn Adapter,
    opts: &Options,
    network: Option<NetworkPlan>,
) -> Result<(), String> {
    let Some(req) = opts.require_enforcement else {
        return Ok(());
    };
    let harness = opts.harness.name();

    // A requested tier with no resolved plan means the caller skipped
    // `resolve_network`. Refuse rather than let a demanded network guarantee
    // silently go unchecked -- this preflight is the guarantee.
    if opts.network.is_some() && network.is_none() {
        return Err(format!(
            "{harness}: internal error -- --network was requested but not resolved; \
             call resolve_network before check_enforcement"
        ));
    }

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

    if let (Some(requested), Some(plan)) = (opts.network, network)
        && !req.satisfied_by(plan.enforcement)
    {
        // Name the effective tier too when it differs: "can only enforce
        // network=full via none" is confusing when the run would really get
        // no network at all.
        let tier = if plan.effective == requested {
            requested.label().to_string()
        } else {
            format!("{} (effectively {})", requested.label(), plan.effective.label())
        };
        return Err(format!(
            "{harness} can only enforce network={tier} via {}, not {}",
            plan.enforcement.label(),
            req.label(),
        ));
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
        let network = resolve_network(adapter.as_ref(), &opts).unwrap();
        let err = check_enforcement(adapter.as_ref(), &opts, network).unwrap_err();
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
        let network = resolve_network(adapter.as_ref(), &opts).unwrap();
        assert!(check_enforcement(adapter.as_ref(), &opts, network).is_ok());
    }

    #[test]
    fn resolve_network_is_none_when_unrequested() {
        let opts = Options {
            harness: crate::harness::Harness::Codex,
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        assert_eq!(resolve_network(adapter.as_ref(), &opts).unwrap(), None);
    }

    #[test]
    fn restricted_is_rejected_for_codex_but_accepted_elsewhere() {
        // codex has no domain allowlist, so `restricted` is rejected rather
        // than downgraded. claude/opencode/pi take every tier (callers pass one
        // tier across mixed harnesses) and report it unenforced.
        let codex = Options {
            harness: crate::harness::Harness::Codex,
            network: Some(Network::Restricted),
            ..Options::default()
        };
        let adapter = for_harness(&codex.harness, false).unwrap();
        let err = resolve_network(adapter.as_ref(), &codex).unwrap_err();
        assert!(err.starts_with("codex:"), "message names the harness: {err}");
        assert!(err.contains("restricted"), "got: {err}");

        for h in [
            crate::harness::Harness::Claude,
            crate::harness::Harness::Opencode,
            crate::harness::Harness::Pi,
        ] {
            let opts = Options {
                harness: h.clone(),
                network: Some(Network::Restricted),
                ..Options::default()
            };
            let adapter = for_harness(&h, false).unwrap();
            let plan = resolve_network(adapter.as_ref(), &opts).unwrap().unwrap();
            // Accepted, but nothing holds it, so the reported reality is an
            // open network rather than an echo of "restricted".
            assert_eq!(plan.effective, Network::Full);
            assert_eq!(plan.enforcement, Enforcement::Unenforced);
        }
    }

    #[test]
    fn require_enforcement_rejects_an_unenforced_network_tier() {
        // claude cannot enforce any network tier, so demanding one must fail
        // preflight rather than run with a flag that does nothing.
        let opts = Options {
            harness: crate::harness::Harness::Claude,
            network: Some(Network::None),
            require_enforcement: Some(RequireEnforcement::Any),
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        let network = resolve_network(adapter.as_ref(), &opts).unwrap();
        let err = check_enforcement(adapter.as_ref(), &opts, network).unwrap_err();
        assert!(err.contains("network=none"), "got: {err}");
    }

    #[test]
    fn a_downgrade_to_a_stricter_tier_still_runs() {
        // codex read-only blocks network unconditionally, so `--network full`
        // is really `none`. That is stricter than asked for, so it satisfies an
        // enforcement demand and the run proceeds; the envelope carries the
        // downgrade, not a failure.
        let opts = Options {
            harness: crate::harness::Harness::Codex,
            perms: Some(Perms::ReadOnly),
            network: Some(Network::Full),
            require_enforcement: Some(RequireEnforcement::Any),
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        let network = resolve_network(adapter.as_ref(), &opts).unwrap().unwrap();
        assert_eq!(network.effective, Network::None);
        assert!(check_enforcement(adapter.as_ref(), &opts, Some(network)).is_ok());
    }

    #[test]
    fn rejection_names_the_effective_tier_when_it_differs() {
        // claude leaves the network open, so `--network none` there is really
        // `full`. The rejection must name both tiers -- "can only enforce
        // network=none via none" hides that the run would have had a network.
        let opts = Options {
            harness: crate::harness::Harness::Claude,
            network: Some(Network::None),
            require_enforcement: Some(RequireEnforcement::Any),
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        let network = resolve_network(adapter.as_ref(), &opts).unwrap();
        let err = check_enforcement(adapter.as_ref(), &opts, network).unwrap_err();
        assert!(err.contains("network=none (effectively full)"), "got: {err}");
    }

    #[test]
    fn an_unresolved_network_request_is_refused_not_skipped() {
        // Passing `None` while `opts.network` is set means the caller skipped
        // resolve_network. Silently dropping the demand would lose the
        // guarantee this preflight exists to provide.
        let opts = Options {
            harness: crate::harness::Harness::Codex,
            network: Some(Network::None),
            require_enforcement: Some(RequireEnforcement::Any),
            ..Options::default()
        };
        let adapter = for_harness(&opts.harness, false).unwrap();
        let err = check_enforcement(adapter.as_ref(), &opts, None).unwrap_err();
        assert!(err.contains("not resolved"), "got: {err}");
    }

    #[test]
    fn adapter_drive_labels() {
        assert_eq!(for_harness(&crate::harness::Harness::Claude, false).unwrap().drive(), "print");
        assert_eq!(for_harness(&crate::harness::Harness::Claude, true).unwrap().drive(), "pty");
        assert_eq!(for_harness(&crate::harness::Harness::Codex, false).unwrap().drive(), "exec");
        assert_eq!(for_harness(&crate::harness::Harness::Pi, false).unwrap().drive(), "exec");
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
