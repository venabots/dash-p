//! Argument parsing for the `claude -p`-compatible surface. Recognised flags
//! are mapped onto the child `claude` invocation; anything we don't recognise
//! is forwarded verbatim so the wrapper stays useful as Claude Code evolves.
//!
//! `-p` / `--print` is accepted but ignored: dash-p *is* print mode (it
//! emulates `claude -p` by driving interactive mode), so the flag is redundant
//! rather than contradictory, and swallowing it lets callers that invoke
//! `claude -p "..."` point at dash-p unchanged. It must not be forwarded to
//! the child `claude` -- doing so would enable real print mode and break the
//! Stop-hook capture. A user-supplied `--settings` is rejected: we inject our
//! own `--settings` to register the Stop hook.
//!
//! `-H` / `--harness` selects which agent CLI to drive (see `crate::harness`).
//! `--agent` is deliberately not used: claude has its own `--agent <subagent>`
//! flag, which dash-p forwards through untouched.

use crate::harness::Harness;
use crate::policy::{Network, Perms, RequireEnforcement};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub prompt: String,
    pub harness: Harness,
    pub output_format: OutputFormat,
    pub model: Option<String>,
    pub skip_permissions: bool,
    /// Permission tier requested by intent (`--perms`). `None` leaves the
    /// harness at its own default and reports no enforcement.
    pub perms: Option<Perms>,
    /// Network tier requested by intent (`--network`).
    pub network: Option<Network>,
    /// Enforcement class demanded by `--require-enforcement` (exit 32 if unmet).
    pub require_enforcement: Option<RequireEnforcement>,
    pub cwd: Option<String>,
    /// Undocumented `--pty` escape hatch: drive the harness's interactive TUI
    /// under a PTY instead of its native non-interactive mode (for claude, the
    /// fallback when `claude -p` is unavailable).
    pub pty: bool,
    /// Path to write the authoritative metadata envelope to (a side channel
    /// distinct from the answer on stdout). `None` disables it.
    pub meta_file: Option<String>,
    pub timeout_ms: u64,
    pub debug: bool,
    pub cols: u16,
    pub rows: u16,
    /// Flags forwarded verbatim to the child `claude`.
    pub extra_args: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            harness: Harness::default(),
            output_format: OutputFormat::Text,
            model: None,
            skip_permissions: false,
            perms: None,
            network: None,
            require_enforcement: None,
            cwd: None,
            pty: false,
            meta_file: None,
            timeout_ms: 300_000,
            debug: false,
            cols: 120,
            rows: 40,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArgError {
    SettingsRejected,
    MissingValue(String),
    BadOutputFormat(String),
    BadNumber(String),
    BadValue { flag: String, value: String, allowed: String },
    Usage(String),
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SettingsRejected => write!(
                f,
                "--settings is rejected: dash-p injects its own settings to register the Stop hook"
            ),
            Self::MissingValue(flag) => write!(f, "flag {flag} requires a value"),
            Self::BadOutputFormat(v) => {
                write!(f, "invalid --output-format {v:?} (text|json|stream-json)")
            }
            Self::BadNumber(v) => write!(f, "invalid number: {v:?}"),
            Self::BadValue { flag, value, allowed } => {
                write!(f, "invalid {flag} {value:?} ({allowed})")
            }
            Self::Usage(msg) => write!(f, "{msg}"),
        }
    }
}

/// Claude long-options that take a value. We forward these (with their value)
/// verbatim so the value isn't absorbed into the prompt.
const KNOWN_VALUE_FLAGS: &[&str] = &[
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--system-prompt",
    "--system-prompt-file",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--permission-mode",
    "--permission-prompt-tool",
    "--fallback-model",
    "--setting-sources",
    "--add-dir",
    "--mcp-config",
    "--max-turns",
    "--resume",
    "--session-id",
    "--agent",
    "--agents",
    "--input-format",
];

/// Parse argv (excluding argv[0]). Positional tokens become the prompt.
pub fn parse(args: &[String]) -> Result<Options, ArgError> {
    let mut opts = Options::default();
    let mut prompt_parts: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut end_of_options = false;

    while i < args.len() {
        let a = &args[i];
        if end_of_options {
            prompt_parts.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            end_of_options = true;
            i += 1;
            continue;
        }

        // Support the `--flag=value` form for long options.
        let (flag, inline): (&str, Option<&str>) = if a.starts_with("--") {
            match a.split_once('=') {
                Some((f, v)) => (f, Some(v)),
                None => (a.as_str(), None),
            }
        } else {
            (a.as_str(), None)
        };

        match flag {
            // Accepted but ignored: dash-p already emulates print mode, so
            // -p/--print is redundant. It must be swallowed here rather than
            // forwarded -- passing it to the child claude would enable real
            // print mode and break the Stop-hook capture.
            "-p" | "--print" => {}
            "--settings" => return Err(ArgError::SettingsRejected),
            // `-H`/`--harness` (not `--agent`) selects the backend: claude has
            // its own native `--agent <subagent>` flag, so dash-p leaves that
            // name alone and forwards it through (see `KNOWN_VALUE_FLAGS`).
            "-H" | "--harness" => {
                opts.harness = Harness::parse(value(inline, args, &mut i, flag)?);
            }
            "--dangerously-skip-permissions" => opts.skip_permissions = true,
            // Undocumented: drive the interactive TUI under a PTY instead of the
            // harness's native non-interactive mode.
            "--pty" => opts.pty = true,
            "--debug" | "-d" => opts.debug = true,
            "--output-format" => {
                opts.output_format = parse_output_format(value(inline, args, &mut i, flag)?)?;
            }
            "--model" => {
                // `--model default` explicitly requests the harness's own
                // default (reported as model_requested "default"); any other
                // value passes through and the harness validates it live.
                let v = value(inline, args, &mut i, flag)?;
                opts.model = if v == "default" {
                    None
                } else {
                    Some(v.to_string())
                };
            }
            "--cwd" => opts.cwd = Some(value(inline, args, &mut i, flag)?.to_string()),
            "--meta-file" => opts.meta_file = Some(value(inline, args, &mut i, flag)?.to_string()),
            "--perms" => {
                let v = value(inline, args, &mut i, flag)?;
                opts.perms = Some(Perms::parse(v).ok_or_else(|| ArgError::BadValue {
                    flag: flag.to_string(),
                    value: v.to_string(),
                    allowed: "read-only|workspace-write|full".to_string(),
                })?);
            }
            "--network" => {
                let v = value(inline, args, &mut i, flag)?;
                opts.network = Some(Network::parse(v).ok_or_else(|| ArgError::BadValue {
                    flag: flag.to_string(),
                    value: v.to_string(),
                    allowed: "none|restricted|full".to_string(),
                })?);
            }
            "--require-enforcement" => {
                let v = value(inline, args, &mut i, flag)?;
                opts.require_enforcement =
                    Some(RequireEnforcement::parse(v).ok_or_else(|| ArgError::BadValue {
                        flag: flag.to_string(),
                        value: v.to_string(),
                        allowed: "os-sandbox|any".to_string(),
                    })?);
            }
            "--timeout" => {
                let v = value(inline, args, &mut i, flag)?;
                let secs: u64 = v.parse().map_err(|_| ArgError::BadNumber(v.to_string()))?;
                opts.timeout_ms = secs.saturating_mul(1000);
            }
            "--cols" => {
                let v = value(inline, args, &mut i, flag)?;
                opts.cols = v.parse().map_err(|_| ArgError::BadNumber(v.to_string()))?;
            }
            "--rows" => {
                let v = value(inline, args, &mut i, flag)?;
                opts.rows = v.parse().map_err(|_| ArgError::BadNumber(v.to_string()))?;
            }
            f if KNOWN_VALUE_FLAGS.contains(&f) => {
                // Forward a recognized claude value-flag together with its value
                // so the value isn't swallowed into the prompt.
                let v = value(inline, args, &mut i, flag)?.to_string();
                opts.extra_args.push(f.to_string());
                opts.extra_args.push(v);
            }
            other if other.starts_with('-') => {
                // Unknown flag: forward the original token verbatim (covers
                // boolean flags and `--flag=value`). A *space-separated* value
                // for an unrecognized flag can't be detected and would be taken
                // as the prompt -- pass such flags as `--flag=value`.
                opts.extra_args.push(a.clone());
            }
            _ => prompt_parts.push(a.clone()),
        }
        i += 1;
    }

    opts.prompt = prompt_parts.join(" ");
    Ok(opts)
}

fn parse_output_format(v: &str) -> Result<OutputFormat, ArgError> {
    match v {
        "text" => Ok(OutputFormat::Text),
        "json" => Ok(OutputFormat::Json),
        "stream-json" => Ok(OutputFormat::StreamJson),
        other => Err(ArgError::BadOutputFormat(other.to_string())),
    }
}

/// Resolve a flag's value: the inline `--flag=value` form if present, else the
/// next argv token.
fn value<'a>(
    inline: Option<&'a str>,
    args: &'a [String],
    i: &mut usize,
    flag: &str,
) -> Result<&'a str, ArgError> {
    match inline {
        Some(v) => Ok(v),
        None => take_value(args, i, flag),
    }
}

fn take_value<'a>(
    args: &'a [String],
    i: &mut usize,
    flag: &str,
) -> Result<&'a str, ArgError> {
    *i += 1;
    args.get(*i)
        .map(String::as_str)
        .ok_or_else(|| ArgError::MissingValue(flag.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn positional_prompt() {
        let o = parse(&v(&["hello", "world"])).unwrap();
        assert_eq!(o.prompt, "hello world");
        assert_eq!(o.output_format, OutputFormat::Text);
    }

    #[test]
    fn output_format_json() {
        let o = parse(&v(&["--output-format", "json", "hi"])).unwrap();
        assert_eq!(o.output_format, OutputFormat::Json);
        assert_eq!(o.prompt, "hi");
    }

    #[test]
    fn skip_permissions_and_model() {
        let o = parse(&v(&["--dangerously-skip-permissions", "--model", "opus", "hi"])).unwrap();
        assert!(o.skip_permissions);
        assert_eq!(o.model.as_deref(), Some("opus"));
    }

    #[test]
    fn print_flag_accepted_as_noop() {
        // -p/--print is redundant (dash-p is print mode) -- accepted, ignored,
        // and not forwarded to the child claude.
        let o = parse(&v(&["-p", "hi"])).unwrap();
        assert_eq!(o.prompt, "hi");
        assert!(o.extra_args.is_empty());

        let o = parse(&v(&["--print", "hello", "world"])).unwrap();
        assert_eq!(o.prompt, "hello world");
        assert!(o.extra_args.is_empty());
    }

    #[test]
    fn settings_rejected() {
        assert_eq!(
            parse(&v(&["--settings", "{}", "hi"])),
            Err(ArgError::SettingsRejected)
        );
    }

    #[test]
    fn timeout_seconds_to_ms() {
        let o = parse(&v(&["--timeout", "30", "hi"])).unwrap();
        assert_eq!(o.timeout_ms, 30_000);
    }

    #[test]
    fn end_of_options_routes_to_prompt() {
        let o = parse(&v(&["--", "--model", "literal"])).unwrap();
        assert_eq!(o.prompt, "--model literal");
    }

    #[test]
    fn unknown_flag_forwarded() {
        let o = parse(&v(&["--verbose", "hi"])).unwrap();
        assert!(o.extra_args.contains(&"--verbose".to_string()));
        assert_eq!(o.prompt, "hi");
    }

    #[test]
    fn bad_output_format() {
        assert!(matches!(
            parse(&v(&["--output-format", "yaml", "hi"])),
            Err(ArgError::BadOutputFormat(_))
        ));
    }

    #[test]
    fn inline_value_form() {
        let o = parse(&v(&["--output-format=json", "--model=opus", "hi"])).unwrap();
        assert_eq!(o.output_format, OutputFormat::Json);
        assert_eq!(o.model.as_deref(), Some("opus"));
        assert_eq!(o.prompt, "hi");
    }

    #[test]
    fn known_value_flag_forwarded_with_value() {
        let o = parse(&v(&["--allowedTools", "Bash(git *)", "hi"])).unwrap();
        let idx = o
            .extra_args
            .iter()
            .position(|s| s == "--allowedTools")
            .unwrap();
        assert_eq!(o.extra_args[idx + 1], "Bash(git *)");
        // The value did not leak into the prompt.
        assert_eq!(o.prompt, "hi");
    }

    #[test]
    fn known_value_flag_inline() {
        let o = parse(&v(&["--resume=abc123", "hi"])).unwrap();
        assert!(o.extra_args.windows(2).any(|w| w == ["--resume", "abc123"]));
        assert_eq!(o.prompt, "hi");
    }

    #[test]
    fn model_default_maps_to_none() {
        let o = parse(&v(&["--model", "default", "hi"])).unwrap();
        assert_eq!(o.model, None);
        let o = parse(&v(&["--model", "opus", "hi"])).unwrap();
        assert_eq!(o.model.as_deref(), Some("opus"));
    }

    #[test]
    fn meta_file_captured() {
        let o = parse(&v(&["--meta-file", "/tmp/m.json", "hi"])).unwrap();
        assert_eq!(o.meta_file.as_deref(), Some("/tmp/m.json"));
        assert_eq!(o.prompt, "hi");

        let o = parse(&v(&["--meta-file=/tmp/n.json", "hi"])).unwrap();
        assert_eq!(o.meta_file.as_deref(), Some("/tmp/n.json"));
    }

    #[test]
    fn perms_network_require_parsed() {
        let o = parse(&v(&[
            "--perms", "read-only",
            "--network", "none",
            "--require-enforcement", "os-sandbox",
            "hi",
        ]))
        .unwrap();
        assert_eq!(o.perms, Some(Perms::ReadOnly));
        assert_eq!(o.network, Some(Network::None));
        assert_eq!(o.require_enforcement, Some(RequireEnforcement::OsSandbox));
        assert_eq!(o.prompt, "hi");
    }

    #[test]
    fn bad_perms_value_rejected() {
        assert!(matches!(
            parse(&v(&["--perms", "yolo", "hi"])),
            Err(ArgError::BadValue { .. })
        ));
    }

    #[test]
    fn harness_defaults_to_claude() {
        let o = parse(&v(&["hi"])).unwrap();
        assert_eq!(o.harness, Harness::Claude);
    }

    #[test]
    fn harness_flag_selects_known_backend() {
        let o = parse(&v(&["--harness", "codex", "hi"])).unwrap();
        assert_eq!(o.harness, Harness::Codex);
        assert_eq!(o.prompt, "hi");

        let o = parse(&v(&["-H", "gemini", "hi"])).unwrap();
        assert_eq!(o.harness, Harness::Gemini);
    }

    #[test]
    fn pty_flag_defaults_off_and_sets() {
        assert!(!parse(&v(&["hi"])).unwrap().pty);
        assert!(parse(&v(&["--pty", "hi"])).unwrap().pty);
    }

    #[test]
    fn agent_flag_is_forwarded_not_harness_select() {
        // `--agent` is claude's own subagent flag; dash-p must NOT consume it
        // as harness selection -- it forwards it (with its value) to claude and
        // leaves the harness at the default.
        let o = parse(&v(&["--agent", "reviewer", "hi"])).unwrap();
        assert_eq!(o.harness, Harness::default());
        assert_eq!(o.prompt, "hi");
        assert_eq!(o.extra_args, vec!["--agent", "reviewer"]);
    }

    #[test]
    fn harness_flag_inline_and_custom_path() {
        let o = parse(&v(&["--harness=opencode", "hi"])).unwrap();
        assert_eq!(o.harness, Harness::Opencode);

        let o = parse(&v(&["--harness", "/opt/bin/claude-fork", "hi"])).unwrap();
        assert_eq!(o.harness, Harness::Custom("/opt/bin/claude-fork".into()));
        // The harness value is consumed, not leaked into the prompt or forwarded.
        assert_eq!(o.prompt, "hi");
        assert!(o.extra_args.is_empty());
    }

    #[test]
    fn settings_inline_form_rejected() {
        assert_eq!(
            parse(&v(&["--settings={}", "hi"])),
            Err(ArgError::SettingsRejected)
        );
    }
}
