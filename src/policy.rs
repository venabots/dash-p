//! Permission, network, and enforcement intent.
//!
//! A caller requests a permission/network tier *by intent*; each adapter maps
//! it to the harness's native mechanism and reports the **enforcement class**
//! actually achieved. The honesty principle: where a harness can only enforce a
//! tier by asking the agent nicely (policy) rather than an OS sandbox, we say
//! so -- and `--require-enforcement` lets a caller refuse anything weaker than
//! it demands (exit 32) instead of trusting a uniform-looking flag that lies.

/// Permission tier requested by intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perms {
    ReadOnly,
    WorkspaceWrite,
    Full,
}

/// Network access tier requested by intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    None,
    Restricted,
    Full,
}

/// How strongly a tier is actually enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// OS-level sandbox: the agent physically cannot exceed the tier.
    OsSandbox,
    /// Agent policy: enforced by instruction/permission-mode, not the OS.
    AgentPolicy,
    /// No tools at all: the model is called directly and can only answer, so
    /// nothing it returns runs. No tier can be exceeded, which is at least as
    /// strong as an OS sandbox.
    NoTools,
    /// Not enforced at all (the tier is requested but nothing stops the agent).
    Unenforced,
}

/// The enforcement class a caller demands via `--require-enforcement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequireEnforcement {
    /// Must be an OS sandbox, or no tools at all; anything weaker fails.
    OsSandbox,
    /// Any real enforcement (os-sandbox or agent-policy); only `Unenforced` fails.
    Any,
}

impl Perms {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Full => "full",
        }
    }
}

impl Network {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "restricted" => Some(Self::Restricted),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Restricted => "restricted",
            Self::Full => "full",
        }
    }
}

impl Enforcement {
    pub fn label(self) -> &'static str {
        match self {
            Self::OsSandbox => "os-sandbox",
            Self::AgentPolicy => "agent-policy",
            Self::NoTools => "no-tools",
            Self::Unenforced => "none",
        }
    }
}

/// The network tier a run actually gets, and how strongly it is held.
///
/// `effective` can differ from what was requested: codex's `read-only` sandbox
/// blocks network unconditionally, so `--perms read-only --network full` really
/// runs with no network at all. Reporting that difference is the whole point --
/// a caller must be able to tell a real block from a flag that did nothing.
///
/// The invariant that keeps `effective` honest: **it never claims more
/// restriction than dash-p can prove.** Nothing enforced means the network is,
/// or may be, wide open, so an unenforced plan always reports `Full` -- see
/// [`NetworkPlan::open`]. Under-claiming is the safe direction; over-claiming is
/// the exact failure this type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkPlan {
    pub effective: Network,
    /// Enforcement class of the **effective** tier. `Full` is never "enforced":
    /// an unrestricted network has no restriction to hold.
    pub enforcement: Enforcement,
}

impl NetworkPlan {
    /// Nothing holds the tier, so the network is (or may be) open.
    ///
    /// Takes no tier by design: a harness that enforces nothing cannot report a
    /// restricted `effective`, and making that unrepresentable is what stops
    /// `--network none` on claude from reading like a real block. A caller sees
    /// `network_effective: "full"` with `network_enforcement: "none"` and knows
    /// the flag did nothing.
    pub fn open() -> Self {
        Self { effective: Network::Full, enforcement: Enforcement::Unenforced }
    }

    /// A tier held by an OS sandbox: the agent physically cannot exceed it.
    pub fn os_sandbox(effective: Network) -> Self {
        Self { effective, enforcement: Enforcement::OsSandbox }
    }

    /// The model has no tools, so it has no network, whatever tier was asked
    /// for. Takes no tier for the same reason `open` takes none.
    pub fn no_tools() -> Self {
        Self { effective: Network::None, enforcement: Enforcement::NoTools }
    }
}

/// What a run actually enforced, for the metadata envelope. Grouped so the
/// perms and network verdicts travel together instead of as loose arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Enforced {
    /// Enforcement class achieved for the requested `--perms` tier.
    pub perms: Option<Enforcement>,
    /// Effective network tier and its enforcement class.
    pub network: Option<NetworkPlan>,
}

impl RequireEnforcement {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "os-sandbox" => Some(Self::OsSandbox),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OsSandbox => "os-sandbox",
            Self::Any => "any",
        }
    }

    /// Whether `actual` meets this demand.
    pub fn satisfied_by(self, actual: Enforcement) -> bool {
        match self {
            // A run with no tools cannot exceed any tier, so it meets the
            // strongest demand too.
            Self::OsSandbox => matches!(actual, Enforcement::OsSandbox | Enforcement::NoTools),
            Self::Any => actual != Enforcement::Unenforced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_os_sandbox_only_met_by_os_sandbox() {
        let req = RequireEnforcement::OsSandbox;
        assert!(req.satisfied_by(Enforcement::OsSandbox));
        assert!(!req.satisfied_by(Enforcement::AgentPolicy));
        assert!(!req.satisfied_by(Enforcement::Unenforced));
    }

    #[test]
    fn no_tools_meets_every_demand() {
        assert!(RequireEnforcement::OsSandbox.satisfied_by(Enforcement::NoTools));
        assert!(RequireEnforcement::Any.satisfied_by(Enforcement::NoTools));
        assert_eq!(Enforcement::NoTools.label(), "no-tools");
        assert_eq!(NetworkPlan::no_tools().effective, Network::None);
    }

    #[test]
    fn require_any_rejects_only_unenforced() {
        let req = RequireEnforcement::Any;
        assert!(req.satisfied_by(Enforcement::OsSandbox));
        assert!(req.satisfied_by(Enforcement::AgentPolicy));
        assert!(!req.satisfied_by(Enforcement::Unenforced));
    }

    #[test]
    fn parse_round_trips() {
        assert_eq!(Perms::parse("read-only"), Some(Perms::ReadOnly));
        assert_eq!(Perms::parse("nope"), None);
        assert_eq!(Network::parse("none"), Some(Network::None));
        assert_eq!(RequireEnforcement::parse("any"), Some(RequireEnforcement::Any));
        assert_eq!(Enforcement::OsSandbox.label(), "os-sandbox");
    }
}
