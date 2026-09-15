//! Selection of which agent CLI ("harness") dash-p drives.
//!
//! dash-p aims to be one non-interactive interface in front of any coding
//! agent. claude, codex, opencode, and pi are implemented (see
//! [`crate::adapters`]). The remaining names below are recognised and reserved
//! so the `--harness` surface is stable as backends are added; selecting one
//! that isn't wired up yet fails fast with a clear message (see
//! [`crate::adapters::for_harness`]).
//!
//! A value that is not a known name is treated as a path/binary and driven with
//! the Claude protocol, so a fork or wrapper of `claude` can be pointed at
//! directly (this subsumes the `DASHP_CLAUDE_BIN` escape hatch).

/// Known harness names, in the order shown in help/error text.
pub const KNOWN_NAMES: &[&str] = &["claude", "codex", "opencode", "gemini", "pi", "anthropic-api"];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Harness {
    /// Default claude harness, driven via `claude -p` (print mode). The
    /// undocumented `--pty` flag switches it to the interactive-TUI-under-a-PTY
    /// drive for environments where `claude -p` is unavailable.
    #[default]
    Claude,
    Codex,
    Opencode,
    Gemini,
    Pi,
    /// The Anthropic Messages API, called directly (no binary).
    AnthropicApi,
    /// A binary name or path not in the known list, driven with the Claude
    /// protocol (it must be a claude-compatible CLI).
    Custom(String),
}

impl Harness {
    /// Resolve a `--harness` value. Known names match case-insensitively;
    /// anything else is taken as a custom binary path.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "opencode" => Self::Opencode,
            "gemini" => Self::Gemini,
            "pi" => Self::Pi,
            "anthropic-api" => Self::AnthropicApi,
            _ => Self::Custom(s.to_string()),
        }
    }

    /// Human-facing name for diagnostics.
    pub fn name(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Gemini => "gemini",
            Self::Pi => "pi",
            Self::AnthropicApi => "anthropic-api",
            Self::Custom(s) => s,
        }
    }

    /// An API harness calls a provider over HTTP and has no binary.
    pub fn is_api(&self) -> bool {
        matches!(self, Self::AnthropicApi)
    }

    /// Best-effort harness version: run `<bin> --version` and return the first
    /// non-empty trimmed line. `None` if the binary is absent or errors, and
    /// for an API harness, which has no binary.
    pub fn probe_version(&self) -> Option<String> {
        let out = std::process::Command::new(self.bin()?)
            .arg("--version")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    }

    /// The binary dash-p spawns for this harness; `None` for an API harness.
    pub fn bin(&self) -> Option<&str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Codex => Some("codex"),
            Self::Opencode => Some("opencode"),
            Self::Gemini => Some("gemini"),
            Self::Pi => Some("pi"),
            Self::AnthropicApi => None,
            Self::Custom(s) => Some(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_claude() {
        assert_eq!(Harness::default(), Harness::Claude);
    }

    #[test]
    fn parses_known_names_case_insensitively() {
        assert_eq!(Harness::parse("claude"), Harness::Claude);
        assert_eq!(Harness::parse("Codex"), Harness::Codex);
        assert_eq!(Harness::parse("OPENCODE"), Harness::Opencode);
        assert_eq!(Harness::parse("gemini"), Harness::Gemini);
        assert_eq!(Harness::parse("pi"), Harness::Pi);
        assert_eq!(Harness::parse("Anthropic-API"), Harness::AnthropicApi);
    }

    #[test]
    fn api_harnesses_have_no_binary() {
        assert!(Harness::AnthropicApi.is_api());
        assert_eq!(Harness::AnthropicApi.bin(), None);
        assert_eq!(Harness::AnthropicApi.probe_version(), None);
        assert!(!Harness::Claude.is_api());
    }

    #[test]
    fn unknown_value_is_custom_path_preserving_case() {
        assert_eq!(
            Harness::parse("/opt/bin/My-Claude"),
            Harness::Custom("/opt/bin/My-Claude".to_string())
        );
    }

    #[test]
    fn name_round_trips_for_known_harnesses() {
        for n in KNOWN_NAMES {
            assert_eq!(Harness::parse(n).name(), *n);
        }
    }
}
