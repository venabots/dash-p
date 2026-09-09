//! Shared helpers for the two claude adapters -- the print-mode default and
//! the `--pty` drive. They map a requested permission tier to the same native
//! claude flags and report the same enforcement classes; only the invocation
//! mechanism differs.

use crate::args::Options;
use crate::harness::Harness;
use crate::policy::{Enforcement, Network, NetworkPlan, Perms};

/// Resolve the claude binary to spawn. `DASHP_CLAUDE_BIN` overrides it (for
/// tests, or a cmux-style shim that would clobber our flags); a custom harness
/// already carries its own path.
pub fn resolve_bin(harness: &Harness) -> String {
    if matches!(harness, Harness::Claude)
        && let Ok(b) = std::env::var("DASHP_CLAUDE_BIN")
    {
        return b;
    }
    harness.bin().to_string()
}

/// Pin a relative, path-like program to an absolute path against the launch dir
/// when `cwd` is set, so `--cwd` doesn't relocate where the binary itself
/// resolves (`./shim`, `bin/claude`). A bare PATH name (no separator) is left
/// unchanged. Uses `join` (the binary need not exist yet) and errors only if the
/// current dir can't be read -- never silently falls back to the relative path.
pub fn pin_program(program: String, cwd: Option<&str>) -> std::io::Result<String> {
    if cwd.is_some() && program.contains('/') && std::path::Path::new(&program).is_relative() {
        Ok(std::env::current_dir()?.join(&program).to_string_lossy().into_owned())
    } else {
        Ok(program)
    }
}

/// Tools denied for the `read-only` tier: file writes, command execution, and
/// network. claude's plan mode would be the obvious mapping, but
/// `--permission-mode plan` *silently overrides `--model`* (it substitutes its
/// own model), so a read-only review would run the wrong model. Denying the
/// mutating tools instead keeps the requested model and the same agent-policy
/// enforcement, and is a better fit for review anyway (read/grep/reason, no
/// writes). Keep this list complete and in sync as claude adds write/effect
/// tools (note: `MultiEdit` is not a current tool name).
const READ_ONLY_DISALLOWED: &str = "Edit Write NotebookEdit Bash WebFetch WebSearch";

/// Native claude flags for the requested permission tier. read-only denies the
/// mutating tools (agent-policy); write tiers use bypassPermissions
/// (`--dangerously-skip-permissions`), as does `--dangerously-skip-permissions`
/// requested directly.
pub fn perms_args(opts: &Options) -> Vec<String> {
    let mut v = Vec::new();
    if matches!(opts.perms, Some(Perms::ReadOnly)) {
        v.push("--disallowedTools".to_string());
        v.push(READ_ONLY_DISALLOWED.to_string());
    }
    let bypass = opts.skip_permissions
        || matches!(opts.perms, Some(Perms::WorkspaceWrite) | Some(Perms::Full));
    if bypass {
        v.push("--dangerously-skip-permissions".to_string());
    }
    v
}

/// The enforcement class claude achieves for a permission tier. claude has no
/// OS sandbox: read-only is policy-only, write tiers are unenforced.
pub fn perms_enforcement(perms: Perms) -> Enforcement {
    match perms {
        Perms::ReadOnly => Enforcement::AgentPolicy,
        Perms::WorkspaceWrite | Perms::Full => Enforcement::Unenforced,
    }
}

/// claude has no network control of any kind -- no sandbox, and no flag that
/// gates egress. Every tier is accepted (callers pass one tier across mixed
/// harnesses, so failing here would break a run that codex handles fine), but
/// the run really does get an open network, so that is what gets reported:
/// `network_effective: "full"` with `network_enforcement: "none"`. Echoing the
/// requested tier back would make `--network none` read like a real block.
pub fn network_plan(_perms: Option<Perms>, _network: Network, _bypass: bool) -> NetworkPlan {
    NetworkPlan::open()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_disallows_mutating_tools_not_plan_mode() {
        let opts = Options {
            perms: Some(Perms::ReadOnly),
            ..Options::default()
        };
        // Must NOT use plan mode (it silently overrides --model).
        let args = perms_args(&opts);
        assert!(!args.iter().any(|a| a == "plan"));
        assert_eq!(args, vec!["--disallowedTools", READ_ONLY_DISALLOWED]);
        // The denial list blocks the core write/exec tools.
        for tool in ["Edit", "Write", "Bash"] {
            assert!(READ_ONLY_DISALLOWED.contains(tool));
        }
        assert_eq!(perms_enforcement(Perms::ReadOnly), Enforcement::AgentPolicy);
    }

    #[test]
    fn write_tiers_bypass() {
        let opts = Options {
            perms: Some(Perms::WorkspaceWrite),
            ..Options::default()
        };
        assert_eq!(perms_args(&opts), vec!["--dangerously-skip-permissions"]);
        assert_eq!(perms_enforcement(Perms::Full), Enforcement::Unenforced);
    }

    #[test]
    fn skip_permissions_flag_bypasses() {
        let opts = Options {
            skip_permissions: true,
            ..Options::default()
        };
        assert_eq!(perms_args(&opts), vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn every_network_tier_is_accepted_and_reported_as_an_open_network() {
        // claude cannot gate egress at all. Accepting the tier keeps a caller's
        // uniform `--network` working across mixed harnesses, but the run really
        // has an open network -- so `effective` is Full for every request. An
        // echo of the requested tier would make `--network none` read like a
        // real block.
        for network in [Network::None, Network::Restricted, Network::Full] {
            for perms in [None, Some(Perms::ReadOnly), Some(Perms::Full)] {
                let plan = network_plan(perms, network, false);
                assert_eq!(plan.effective, Network::Full);
                assert_eq!(plan.enforcement, Enforcement::Unenforced);
            }
        }
    }
}
