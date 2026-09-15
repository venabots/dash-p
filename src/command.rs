//! Top-level command surface: `run`, `list`, `capabilities`, plus the
//! bare-prompt sugar that forwards to `run` with defaults.
//!
//! `run`/`list`/`capabilities` are recognised only as the *first* argument
//! (like git/docker); anything else is treated as a prompt for `run`, so
//! `dash-p "summarize this"` still works. A prompt that literally starts with
//! one of those words can be forced with `dash-p run -- "run the tests"`.

use std::io::Write;

use crate::adapters::{self, Adapter};
use crate::args::{self, ArgError, Options};
use crate::harness::{Harness, KNOWN_NAMES};
use crate::policy::{Enforcement, Network, Perms};

pub enum Command {
    Run(Box<Options>),
    ListHarnesses,
    ListModels { harness: Option<Harness> },
    Capabilities { harness: Option<Harness> },
    Help,
    Version,
}

/// Parse argv (excluding argv[0]) into a command.
pub fn parse(raw: &[String]) -> Result<Command, ArgError> {
    // `--help`/`--version` win wherever they appear among the options (before a
    // `--`), so `dash-p run --help` and a bare `dash-p --help` both work. A
    // prompt that literally needs the token can use `run -- "--help"`.
    let opt_tokens = || raw.iter().take_while(|a| a.as_str() != "--");
    if opt_tokens().any(|a| a == "-h" || a == "--help") {
        return Ok(Command::Help);
    }
    if opt_tokens().any(|a| a == "-V" || a == "--version") {
        return Ok(Command::Version);
    }

    match raw.first().map(String::as_str) {
        Some("help") => Ok(Command::Help),
        Some("run") => Ok(Command::Run(Box::new(args::parse(&raw[1..])?))),
        Some("list") => parse_list(&raw[1..]),
        Some("capabilities") | Some("caps") => Ok(Command::Capabilities {
            harness: harness_flag(&raw[1..])?,
        }),
        // Bare prompt / flags: sugar for `run`.
        _ => Ok(Command::Run(Box::new(args::parse(raw)?))),
    }
}

/// Top-level `--help` text.
pub const HELP: &str = "\
dash-p — one non-interactive interface in front of any coding-agent CLI.

A thin adapter, not an orchestrator: it runs one agent well and reports the
truth about what model ran and what was enforced. stdout carries only the
answer; metadata goes to --meta-file; logs go to stderr.

Usage:
  dash-p [run] [options] [--] \"<prompt>\"     run a one-shot prompt
  dash-p list harnesses                     installed/implemented harnesses + versions
  dash-p list models [--harness <name>]     best-effort model discovery
  dash-p capabilities [--harness <name>]    per-harness perms→enforcement, network, outputs
  dash-p --help | --version

`run` is the default; a bare prompt is sugar for it, and if no prompt is given
it is read from stdin. `run`/`list`/`capabilities` are only recognised as the
first argument.

Run options:
  -H, --harness <name|path>   claude (default) | codex | opencode | pi | anthropic-api
                              | path to a claude-compatible binary
      --model <id|default>    model id; 'default' requests the harness's own default
      --output-format <fmt>   text (default) | json ({answer,metadata}) | stream-json
      --perms <tier>          read-only | workspace-write | full   (permission tier, by intent)
      --network <tier>        none | restricted | full             (network tier, by intent)
                              codex always rejects 'restricted' (no domain allowlist).
                              With --perms workspace-write it blocks 'none' and opens
                              'full' (os-sandbox); --perms read-only blocks every tier.
                              Without --perms, and on claude/opencode/pi, 'none'/'full'
                              are accepted but not enforced. See `capabilities`.
      --require-enforcement <class>   os-sandbox | any   (else exit 32 before running)
      --meta-file <path>      write the authoritative run-metadata envelope here
      --cwd <path>            working directory for the agent
      --base-url <url>        API harnesses: provider URL (else the format's env var)
      --api-key-env <var>     API harnesses: env var that holds the key
      --max-tokens <n>        API harnesses: output token cap
      --timeout <seconds>     wall-time cap (default 300)
      --dangerously-skip-permissions
      --pty                   drive the agent's interactive TUI under a PTY, for
                              when its non-interactive mode is unavailable
      --cols <n> / --rows <n> PTY size (with --pty; default 120×40)
  -d, --debug                 wrapper + harness debug traces on stderr
      --                      end of options; the rest is the prompt

Exit codes:
  0  ok                  10 agent-error          20 timeout
  30 harness-not-found   31 invalid-model        32 enforcement-unsupported
  130 interrupted        2  internal
";

fn parse_list(rest: &[String]) -> Result<Command, ArgError> {
    match rest.first().map(String::as_str) {
        Some("harnesses") => Ok(Command::ListHarnesses),
        Some("models") => Ok(Command::ListModels {
            harness: harness_flag(&rest[1..])?,
        }),
        _ => Err(ArgError::Usage(
            "list: expected 'harnesses' or 'models'".to_string(),
        )),
    }
}

/// Scan for a `-H`/`--harness <name>` (or `=name`) flag in `rest`.
fn harness_flag(rest: &[String]) -> Result<Option<Harness>, ArgError> {
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (a.as_str(), None),
        };
        if flag == "-H" || flag == "--harness" {
            let val = match inline {
                Some(v) => v.to_string(),
                None => {
                    i += 1;
                    rest.get(i)
                        .cloned()
                        .ok_or_else(|| ArgError::MissingValue(flag.to_string()))?
                }
            };
            return Ok(Some(Harness::parse(&val)));
        }
        i += 1;
    }
    Ok(None)
}

const PERM_TIERS: [Perms; 3] = [Perms::ReadOnly, Perms::WorkspaceWrite, Perms::Full];
const NETWORK_TIERS: [Network; 3] = [Network::None, Network::Restricted, Network::Full];

/// `list harnesses`: every recognised harness, whether it has an adapter, and
/// whether its binary is installed (with version).
pub fn list_harnesses(w: &mut dyn Write) -> std::io::Result<()> {
    for name in KNOWN_NAMES {
        let h = Harness::parse(name);
        let status = if adapters::for_harness(&h, false).is_some() {
            "implemented"
        } else {
            "reserved"
        };
        let install = match adapters::api::Protocol::for_harness(&h) {
            Some(protocol) => match adapters::api::credential_status(protocol) {
                Ok(source) => format!("key set ({source})"),
                Err(_) => "no key".to_string(),
            },
            None => match h.probe_version() {
                Some(v) => format!("installed ({v})"),
                None => "not found".to_string(),
            },
        };
        writeln!(w, "{name:<14} {status:<12} {install}")?;
    }
    Ok(())
}

/// `capabilities`: the honest per-harness enforcement map, so callers stop
/// hardcoding harness knowledge. Defaults to every implemented harness.
pub fn capabilities(w: &mut dyn Write, harness: Option<Harness>) -> std::io::Result<()> {
    let targets: Vec<Harness> = match harness {
        Some(h) => vec![h],
        None => KNOWN_NAMES.iter().map(|n| Harness::parse(n)).collect(),
    };
    let mut first = true;
    for h in targets {
        let Some(adapter) = adapters::for_harness(&h, false) else {
            continue;
        };
        if !first {
            writeln!(w)?;
        }
        first = false;
        render_capabilities(w, &h, adapter.as_ref())?;
    }
    Ok(())
}

/// `list models`: best-effort, honest about each harness's limits. Neither
/// codex nor claude exposes a clean model-enumeration command, so we probe what
/// we can (codex's configured default, pi's own `--list-models`) and otherwise
/// point at the aliases.
pub fn list_models(w: &mut dyn Write, harness: Option<Harness>) -> std::io::Result<()> {
    let targets: Vec<Harness> = match harness {
        Some(h) => vec![h],
        None => KNOWN_NAMES.iter().map(|n| Harness::parse(n)).collect(),
    };
    let mut first = true;
    for h in targets {
        if adapters::for_harness(&h, false).is_none() {
            continue;
        }
        if !first {
            writeln!(w)?;
        }
        first = false;
        writeln!(w, "harness: {}", h.name())?;
        if let Some(protocol) = adapters::api::Protocol::for_harness(&h) {
            match adapters::api::list_models(protocol) {
                Ok(ids) => {
                    for id in ids {
                        writeln!(w, "  {id}")?;
                    }
                }
                Err(why) => writeln!(w, "  models: (unavailable: {why})")?,
            }
            writeln!(w, "  note: the provider's own list for this key; pass --model <id> (there is no default)")?;
            continue;
        }
        match h {
            Harness::Codex => {
                match crate::adapters::codex::configured_model() {
                    Some(m) => writeln!(w, "  configured default: {m}")?,
                    None => writeln!(w, "  configured default: (unknown)")?,
                }
                writeln!(w, "  note: codex models are provider-defined; pass -m <model> (e.g. gpt-5.5)")?;
            }
            Harness::Opencode => {
                writeln!(w, "  note: opencode models use provider/model syntax; pass -m <provider/model> (e.g. anthropic/claude-sonnet-4)")?;
                writeln!(w, "  note: opencode exposes no model-list API and does not report the resolved model; model_resolved is \"unknown\"")?;
            }
            Harness::Pi => {
                match crate::adapters::pi::list_models() {
                    Some(table) => {
                        for line in table.lines() {
                            writeln!(w, "  {line}")?;
                        }
                    }
                    None => writeln!(w, "  models: (unavailable; is pi installed?)")?,
                }
                writeln!(w, "  note: pi lists only providers with credentials; pass --model <provider/id> (e.g. anthropic/claude-sonnet-4-5)")?;
            }
            _ => {
                writeln!(w, "  aliases: opus, sonnet, haiku (or a full claude-* id)")?;
                writeln!(w, "  note: claude exposes no model-list API; model_resolved is read from the transcript")?;
            }
        }
    }
    Ok(())
}

fn render_capabilities(w: &mut dyn Write, h: &Harness, adapter: &dyn Adapter) -> std::io::Result<()> {
    writeln!(w, "harness: {}", h.name())?;
    writeln!(w, "perms:")?;
    for p in PERM_TIERS {
        writeln!(w, "  {:<16} {}", p.label(), adapter.perms_enforcement(p).label())?;
    }
    // Report the network tiers at workspace-write: harnesses that gate network
    // through their sandbox give it the most control there, so this is the best
    // case rather than a flattering average. The heading names the tier so the
    // dependency is not hidden.
    writeln!(w, "network (with --perms workspace-write):")?;
    let mut held = Enforcement::Unenforced;
    for n in NETWORK_TIERS {
        match adapter.network_plan(Some(Perms::WorkspaceWrite), n, false) {
            Ok(plan) => {
                if plan.enforcement != Enforcement::Unenforced {
                    held = plan.enforcement;
                }
                // Name the effective tier whenever it differs from the request,
                // so a downgrade is visible here and not only after a run.
                let effective = if plan.effective == n {
                    String::new()
                } else {
                    format!("  (effective: {})", plan.effective.label())
                };
                writeln!(w, "  {:<16} {}{}", n.label(), plan.enforcement.label(), effective)?;
            }
            Err(_) => writeln!(w, "  {:<16} unsupported", n.label())?,
        }
    }
    let net_label = match held {
        Enforcement::OsSandbox => "yes (sandbox blocks network)",
        Enforcement::NoTools => "yes (no tools: the model cannot reach the network)",
        Enforcement::AgentPolicy | Enforcement::Unenforced => {
            "no (--network is accepted but never enforced)"
        }
    };
    writeln!(w, "network-control: {net_label}")?;
    writeln!(w, "output-modes: text, json, stream-json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn help_and_version_detected() {
        assert!(matches!(parse(&v(&["--help"])).unwrap(), Command::Help));
        assert!(matches!(parse(&v(&["-h"])).unwrap(), Command::Help));
        assert!(matches!(parse(&v(&["help"])).unwrap(), Command::Help));
        assert!(matches!(parse(&v(&["run", "--help"])).unwrap(), Command::Help));
        assert!(matches!(parse(&v(&["--version"])).unwrap(), Command::Version));
        assert!(matches!(parse(&v(&["-V"])).unwrap(), Command::Version));
    }

    #[test]
    fn help_token_after_dashdash_is_a_prompt() {
        // `run -- "--help"` must run, not show help.
        match parse(&v(&["run", "--", "--help"])).unwrap() {
            Command::Run(o) => assert_eq!(o.prompt, "--help"),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn bare_prompt_is_run() {
        let cmd = parse(&v(&["hello", "world"])).unwrap();
        match cmd {
            Command::Run(o) => assert_eq!(o.prompt, "hello world"),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_subcommand_strips_keyword() {
        let cmd = parse(&v(&["run", "--model", "opus", "hi"])).unwrap();
        match cmd {
            Command::Run(o) => {
                assert_eq!(o.prompt, "hi");
                assert_eq!(o.model.as_deref(), Some("opus"));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn list_harnesses_and_models() {
        assert!(matches!(parse(&v(&["list", "harnesses"])).unwrap(), Command::ListHarnesses));
        match parse(&v(&["list", "models", "--harness", "codex"])).unwrap() {
            Command::ListModels { harness } => assert_eq!(harness, Some(Harness::Codex)),
            _ => panic!("expected ListModels"),
        }
    }

    #[test]
    fn list_without_target_errs() {
        assert!(matches!(parse(&v(&["list"])), Err(ArgError::Usage(_))));
        assert!(matches!(parse(&v(&["list", "bogus"])), Err(ArgError::Usage(_))));
    }

    fn caps_for(h: Harness) -> String {
        let mut buf = Vec::new();
        capabilities(&mut buf, Some(h)).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn capabilities_report_the_real_codex_network_map() {
        // The advertised map has to match what the sandbox actually does:
        // `none` is a real block, `restricted` has no codex equivalent, and
        // `full` restricts nothing.
        let out = caps_for(Harness::Codex);
        // Assert against the network block alone. `full  none` also appears in
        // the perms block, so a whole-output match would pass even if the
        // network section lost its `full` row.
        let (_, net) = out
            .split_once("network (with --perms workspace-write):")
            .unwrap_or_else(|| panic!("no network block: {out}"));
        assert!(net.contains("none             os-sandbox"), "{net}");
        assert!(net.contains("restricted       unsupported"), "{net}");
        assert!(net.contains("full             none"), "{net}");
        assert!(out.contains("network-control: yes (sandbox blocks network)"), "{out}");
    }

    #[test]
    fn capabilities_admit_harnesses_that_never_enforce_network() {
        // claude and opencode accept --network and do nothing with it. Saying
        // only "no" left a caller guessing whether the flag was even read.
        for h in [Harness::Claude, Harness::Opencode, Harness::Pi] {
            let out = caps_for(h.clone());
            assert!(
                out.contains("network-control: no (--network is accepted but never enforced)"),
                "{}: {out}",
                h.name()
            );
            for tier in ["none", "restricted", "full"] {
                assert!(out.contains(&format!("  {tier:<16} none")), "{}: {out}", h.name());
            }
        }
    }

    #[test]
    fn capabilities_report_pi_read_only_as_policy_only() {
        let out = caps_for(Harness::Pi);
        let (perms, _) = out.split_once("network (").unwrap_or_else(|| panic!("no network block: {out}"));
        assert!(perms.contains("read-only        agent-policy"), "{perms}");
        assert!(perms.contains("workspace-write  none"), "{perms}");
        assert!(perms.contains("full             none"), "{perms}");
    }

    #[test]
    fn capabilities_report_an_api_harness_as_holding_every_tier() {
        let out = caps_for(Harness::AnthropicApi);
        for tier in ["read-only", "workspace-write", "full"] {
            assert!(out.contains(&format!("  {tier:<16} no-tools")), "{out}");
        }
        let (_, net) = out.split_once("network (with --perms workspace-write):").unwrap();
        assert!(net.contains("none             no-tools"), "{net}");
        assert!(net.contains("full             no-tools  (effective: none)"), "{net}");
        assert!(out.contains("network-control: yes (no tools: the model cannot reach the network)"), "{out}");
    }

    #[test]
    fn capabilities_with_and_without_harness() {
        match parse(&v(&["capabilities"])).unwrap() {
            Command::Capabilities { harness } => assert_eq!(harness, None),
            _ => panic!("expected Capabilities"),
        }
        match parse(&v(&["capabilities", "--harness", "claude"])).unwrap() {
            Command::Capabilities { harness } => assert_eq!(harness, Some(Harness::Claude)),
            _ => panic!("expected Capabilities"),
        }
    }
}
