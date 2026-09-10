//! Authoritative run metadata -- the trusted, structured record of what
//! actually ran, written to a side channel (`--meta-file`) distinct from the
//! agent's answer on stdout.
//!
//! The point of this envelope is honesty: `model_resolved` is the launcher's
//! truth (read from the transcript), not the agent's self-report, and it is
//! `"unknown"` when the harness genuinely never exposed the build -- never an
//! echo of the request pretending to be the resolved value.

use crate::args::Options;
use crate::policy::Enforced;
use crate::transcript::Summary;

/// The terminal status of a run, mapped to a stable exit code and label. The
/// codes are a real API orchestrators branch on, so they are fixed:
///
/// `0` ok · `10` agent-error · `20` timeout · `30` harness-not-found ·
/// `31` invalid-model · `32` enforcement-unsupported · `130` interrupted ·
/// `2` internal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Ok,
    AgentError,
    Timeout,
    HarnessNotFound,
    InvalidModel,
    EnforcementUnsupported,
    Interrupted,
    Internal,
}

impl ExitStatus {
    pub fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::AgentError => 10,
            Self::Timeout => 20,
            Self::HarnessNotFound => 30,
            Self::InvalidModel => 31,
            Self::EnforcementUnsupported => 32,
            Self::Interrupted => 130,
            Self::Internal => 2,
        }
    }

    /// Stable machine-readable label for the metadata envelope.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::AgentError => "agent-error",
            Self::Timeout => "timeout",
            Self::HarnessNotFound => "harness-not-found",
            Self::InvalidModel => "invalid-model",
            Self::EnforcementUnsupported => "enforcement-unsupported",
            Self::Interrupted => "interrupted",
            Self::Internal => "internal",
        }
    }
}

/// The authoritative metadata envelope.
pub struct Metadata {
    pub harness: String,
    /// How the harness was actually driven, per the selected adapter:
    /// `"print"` (claude native non-interactive), `"exec"` (codex), or `"pty"`
    /// (the `--pty` interactive-TUI fallback), or `"unknown"` when no adapter
    /// ran (harness-not-found). Adapter-provided, not inferred from `--pty`, so
    /// it never claims `"pty"` for a harness that has no PTY drive. Reported so a
    /// `"pty"` run's `unknown`/0 model+usage isn't mistaken for missing data.
    pub drive: &'static str,
    /// Best-effort harness version; `None` (serialized `null`) when unprobed.
    pub harness_version: Option<String>,
    /// The model the caller asked for; `"default"` when none was specified.
    pub model_requested: String,
    /// The model the harness actually ran, per the transcript; `"unknown"`
    /// when the harness never exposed it. Never an echo of the request.
    pub model_resolved: String,
    /// Requested permission tier (`--perms`), or `None` (serialized `null`).
    pub perms: Option<String>,
    /// Enforcement class achieved for the requested perms, or `None`.
    pub enforcement: Option<String>,
    /// Requested network tier (`--network`), or `None`.
    pub network: Option<String>,
    /// The network tier that actually applied. Differs from `network` when the
    /// harness could not give the requested tier -- codex's read-only sandbox
    /// blocks network unconditionally, so a `full` request really runs `none`.
    pub network_effective: Option<String>,
    /// Enforcement class of `network_effective`, so a caller can tell a real
    /// sandbox (`os-sandbox`) from a flag that did nothing (`none`).
    pub network_enforcement: Option<String>,
    pub duration_ms: u64,
    pub exit_status: ExitStatus,
    pub session_id: String,
    pub num_turns: u32,
    pub total_cost_usd: f64,
    pub usage: crate::transcript::Usage,
}

impl Metadata {
    /// Build the envelope from the request and (when available) the run's
    /// summary. `summary` is `None` when the run failed before producing one.
    pub fn build(
        opts: &Options,
        summary: Option<&Summary>,
        duration_ms: u64,
        exit_status: ExitStatus,
        enforced: Enforced,
        drive: &'static str,
    ) -> Self {
        let model_requested = opts.model.clone().unwrap_or_else(|| "default".to_string());
        let model_resolved = summary
            .map(|s| s.model.as_str())
            .filter(|m| !m.is_empty())
            .unwrap_or("unknown")
            .to_string();
        Self {
            harness: opts.harness.name().to_string(),
            drive,
            harness_version: None,
            model_requested,
            model_resolved,
            perms: opts.perms.map(|p| p.label().to_string()),
            enforcement: enforced.perms.map(|e| e.label().to_string()),
            network: opts.network.map(|n| n.label().to_string()),
            network_effective: enforced
                .network
                .map(|p| p.effective.label().to_string()),
            network_enforcement: enforced
                .network
                .map(|p| p.enforcement.label().to_string()),
            duration_ms,
            exit_status,
            session_id: summary.map(|s| s.session_id.clone()).unwrap_or_default(),
            num_turns: summary.map(|s| s.num_turns).unwrap_or(0),
            total_cost_usd: summary.map(|s| s.total_cost_usd).unwrap_or(0.0),
            usage: summary.map(|s| s.usage.clone()).unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "harness": self.harness,
            "drive": self.drive,
            "harness_version": self.harness_version,
            "model_requested": self.model_requested,
            "model_resolved": self.model_resolved,
            "perms": self.perms,
            "enforcement": self.enforcement,
            "network": self.network,
            "network_effective": self.network_effective,
            "network_enforcement": self.network_enforcement,
            "duration_ms": self.duration_ms,
            "exit_status": self.exit_status.label(),
            "session_id": self.session_id,
            "num_turns": self.num_turns,
            "total_cost_usd": self.total_cost_usd,
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens,
                "cache_read_input_tokens": self.usage.cache_read_input_tokens,
                "cache_creation_input_tokens": self.usage.cache_creation_input_tokens,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Summary, Usage};

    fn summary() -> Summary {
        Summary {
            final_text: "hi".into(),
            session_id: "sid".into(),
            model: "claude-opus-4-8".into(),
            is_error: false,
            num_turns: 2,
            total_cost_usd: 0.01,
            duration_api_ms: 0,
            usage: Usage {
                input_tokens: 12,
                output_tokens: 8,
                ..Default::default()
            },
            jsonl_replay: String::new(),
        }
    }

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(ExitStatus::Ok.code(), 0);
        assert_eq!(ExitStatus::AgentError.code(), 10);
        assert_eq!(ExitStatus::Timeout.code(), 20);
        assert_eq!(ExitStatus::HarnessNotFound.code(), 30);
        assert_eq!(ExitStatus::InvalidModel.code(), 31);
        assert_eq!(ExitStatus::EnforcementUnsupported.code(), 32);
        assert_eq!(ExitStatus::Interrupted.code(), 130);
        assert_eq!(ExitStatus::Internal.code(), 2);
    }

    #[test]
    fn resolved_model_comes_from_summary() {
        let opts = Options {
            model: Some("opus".into()),
            ..Options::default()
        };
        let m = Metadata::build(&opts, Some(&summary()), 100, ExitStatus::Ok, Enforced::default(), "print");
        assert_eq!(m.model_requested, "opus");
        assert_eq!(m.model_resolved, "claude-opus-4-8");
        assert_eq!(m.to_json()["exit_status"], "ok");
        assert_eq!(m.to_json()["usage"]["input_tokens"], 12);
    }

    #[test]
    fn drive_is_passed_through() {
        let m = Metadata::build(&Options::default(), Some(&summary()), 1, ExitStatus::Ok, Enforced::default(), "print");
        assert_eq!(m.to_json()["drive"], "print");

        let m = Metadata::build(&Options::default(), Some(&summary()), 1, ExitStatus::Ok, Enforced::default(), "pty");
        assert_eq!(m.to_json()["drive"], "pty");
    }

    #[test]
    fn requested_default_when_unspecified() {
        let m = Metadata::build(&Options::default(), Some(&summary()), 1, ExitStatus::Ok, Enforced::default(), "print");
        assert_eq!(m.model_requested, "default");
    }

    #[test]
    fn policy_fields_reflect_request_and_enforcement() {
        use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
        let opts = Options {
            perms: Some(Perms::ReadOnly),
            network: Some(Network::None),
            ..Options::default()
        };
        let m = Metadata::build(
            &opts,
            Some(&summary()),
            1,
            ExitStatus::Ok,
            Enforced {
                perms: Some(Enforcement::AgentPolicy),
                network: Some(NetworkPlan::os_sandbox(Network::None)),
            },
            "print",
        );
        let j = m.to_json();
        assert_eq!(j["perms"], "read-only");
        assert_eq!(j["enforcement"], "agent-policy");
        assert_eq!(j["network"], "none");
        assert_eq!(j["network_effective"], "none");
        assert_eq!(j["network_enforcement"], "os-sandbox");

        // Unset policy fields serialize as null.
        let m = Metadata::build(&Options::default(), Some(&summary()), 1, ExitStatus::Ok, Enforced::default(), "print");
        assert!(m.to_json()["perms"].is_null());
        assert!(m.to_json()["enforcement"].is_null());
        assert!(m.to_json()["network_effective"].is_null());
        assert!(m.to_json()["network_enforcement"].is_null());
    }

    #[test]
    fn envelope_separates_requested_from_effective_network() {
        // The case a caller cannot otherwise detect: `--network full` under
        // codex's read-only sandbox really runs with no network at all. The
        // request and the reality must both be on the record.
        use crate::policy::{Enforcement, Network, NetworkPlan, Perms};
        let opts = Options {
            perms: Some(Perms::ReadOnly),
            network: Some(Network::Full),
            ..Options::default()
        };
        let j = Metadata::build(
            &opts,
            Some(&summary()),
            1,
            ExitStatus::Ok,
            Enforced {
                perms: Some(Enforcement::OsSandbox),
                network: Some(NetworkPlan::os_sandbox(Network::None)),
            },
            "exec",
        )
        .to_json();
        assert_eq!(j["network"], "full");
        assert_eq!(j["network_effective"], "none");
        assert_eq!(j["network_enforcement"], "os-sandbox");
    }

    #[test]
    fn envelope_marks_an_unenforced_tier_as_a_no_op() {
        // claude/opencode accept --network but enforce nothing. The envelope has
        // to say so, or a caller cannot tell a real sandbox from a no-op flag.
        use crate::policy::{Network, NetworkPlan};
        let opts = Options {
            network: Some(Network::None),
            ..Options::default()
        };
        let j = Metadata::build(
            &opts,
            Some(&summary()),
            1,
            ExitStatus::Ok,
            Enforced {
                perms: None,
                network: Some(NetworkPlan::open()),
            },
            "print",
        )
        .to_json();
        // Requested none, but nothing held it, so the run really had an open
        // network. Echoing "none" back here would read like a real block.
        assert_eq!(j["network"], "none");
        assert_eq!(j["network_effective"], "full");
        assert_eq!(j["network_enforcement"], "none");
    }

    #[test]
    fn resolved_unknown_without_summary_or_model() {
        // No summary at all (failed run).
        let m = Metadata::build(&Options::default(), None, 1, ExitStatus::Timeout, Enforced::default(), "print");
        assert_eq!(m.model_resolved, "unknown");

        // Summary present but transcript never exposed the model.
        let mut s = summary();
        s.model = String::new();
        let m = Metadata::build(&Options::default(), Some(&s), 1, ExitStatus::Ok, Enforced::default(), "print");
        assert_eq!(m.model_resolved, "unknown");
    }
}
