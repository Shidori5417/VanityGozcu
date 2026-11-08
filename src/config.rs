//! Configuration loading and validation.
//!
//! Settings come from a TOML file; secrets come only from environment variables and
//! are kept in [`SecretString`]. A config file that contains a token-like value is
//! refused.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use secrecy::SecretString;
use serde::Deserialize;
use thiserror::Error;

use crate::model::VanityTarget;

pub const ENV_CONFIG: &str = "VG_CONFIG";
pub const ENV_DISCORD_WEBHOOK: &str = "VG_DISCORD_WEBHOOK";
pub const ENV_TELEGRAM_TOKEN: &str = "VG_TELEGRAM_TOKEN";
pub const ENV_USER_TOKEN: &str = "VG_USER_TOKEN";
pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

const CODE_MIN_LEN: usize = 2;
const CODE_MAX_LEN: usize = 32;
const MAX_RECOMMENDED_TARGETS: usize = 25;

const SECRET_PATTERNS: [(&str, &str); 3] = [
    ("Discord token", r"[\w-]{24,}\.[\w-]{6}\.[\w-]{27,}"),
    ("Discord webhook URL", r"discord(app)?\.com/api/webhooks/"),
    ("Telegram bot token", r"\d{8,10}:[\w-]{35}"),
];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error(
        "config file contains what looks like a {kind} (line {line}); secrets must be \
         provided through environment variables, refusing to start"
    )]
    SecretInFile { kind: &'static str, line: usize },
    #[error("invalid target code {code:?}: {reason}")]
    InvalidCode { code: String, reason: &'static str },
    #[error("duplicate target code {0:?}")]
    DuplicateCode(String),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("internal pattern error: {0}")]
    Pattern(#[from] regex::Error),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct General {
    pub default_interval_secs: u64,
    pub jitter_percent: u8,
    pub confirm_checks: u32,
    pub request_timeout_ms: u64,
    pub database_path: PathBuf,
    pub log_dir: PathBuf,
    pub log_level: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            default_interval_secs: 30,
            jitter_percent: 15,
            confirm_checks: 2,
            request_timeout_ms: 5000,
            database_path: PathBuf::from("data/vanity.db"),
            log_dir: PathBuf::from("logs"),
            log_level: "info".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub max_requests_per_minute: u32,
    pub backoff_max_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests_per_minute: 40,
            backoff_max_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Notify {
    pub on_free: bool,
    pub on_taken: bool,
    pub on_claim: bool,
    pub on_error: bool,
    pub discord: DiscordNotify,
    pub telegram: TelegramNotify,
}

impl Default for Notify {
    fn default() -> Self {
        Self {
            on_free: true,
            on_taken: false,
            on_claim: true,
            on_error: true,
            discord: DiscordNotify::default(),
            telegram: TelegramNotify::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DiscordNotify {
    pub enabled: bool,
    pub mention: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TelegramNotify {
    pub enabled: bool,
    pub chat_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AutoClaim {
    pub enabled: bool,
    pub target_guild_id: Option<String>,
    pub max_attempts: u32,
    pub confirm_checks_override: u32,
}

impl Default for AutoClaim {
    fn default() -> Self {
        Self {
            enabled: false,
            target_guild_id: None,
            max_attempts: 3,
            confirm_checks_override: 1,
        }
    }
}

/// Secrets read from the environment. `Debug` never reveals their values.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    pub discord_webhook: Option<SecretString>,
    pub telegram_token: Option<SecretString>,
    pub user_token: Option<SecretString>,
}

impl Secrets {
    pub fn from_env() -> Self {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(SecretString::from)
        };
        Self {
            discord_webhook: read(ENV_DISCORD_WEBHOOK),
            telegram_token: read(ENV_TELEGRAM_TOKEN),
            user_token: read(ENV_USER_TOKEN),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub ratelimit: RateLimitConfig,
    pub notify: Notify,
    pub auto_claim: AutoClaim,
    pub targets: Vec<VanityTarget>,
    #[serde(skip)]
    pub secrets: Secrets,
}

/// Request budget of the configured targets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub required_per_min: f64,
    pub max_per_min: u32,
}

impl Budget {
    pub fn exceeded(&self) -> bool {
        self.required_per_min > f64::from(self.max_per_min)
    }

    /// Factor by which all intervals stretch to fit the budget (1.0 when it fits).
    pub fn slowdown(&self) -> f64 {
        (self.required_per_min / f64::from(self.max_per_min)).max(1.0)
    }
}

/// A validated config plus non-fatal warnings and the file it came from.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
    pub source: Option<PathBuf>,
}

/// Validates and lowercases a vanity code.
pub fn normalize_code(raw: &str) -> Result<String, ConfigError> {
    let code = raw.to_lowercase();
    let reject = |reason| ConfigError::InvalidCode {
        code: raw.to_owned(),
        reason,
    };
    if !(CODE_MIN_LEN..=CODE_MAX_LEN).contains(&code.chars().count()) {
        return Err(reject("length must be 2-32 characters"));
    }
    if !code
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(reject("only a-z, 0-9 and '-' are allowed"));
    }
    Ok(code)
}

/// Fails if `raw` contains a Discord token, a Discord webhook URL or a Telegram bot token.
pub fn scan_for_secrets(raw: &str) -> Result<(), ConfigError> {
    for (kind, pattern) in SECRET_PATTERNS {
        if let Some(m) = Regex::new(pattern)?.find(raw) {
            let line = raw[..m.start()].matches('\n').count() + 1;
            return Err(ConfigError::SecretInFile { kind, line });
        }
    }
    Ok(())
}

impl Config {
    /// Loads `VG_CONFIG` (must exist) or `./config.toml`. When neither is present the
    /// defaults are used so that `check` works without a config.
    pub fn load() -> Result<Loaded, ConfigError> {
        let (path, explicit) = Self::resolve_path();
        Self::load_from(&path, explicit, Secrets::from_env())
    }

    /// The config path (`VG_CONFIG` or `./config.toml`) and whether it was explicit.
    pub fn resolve_path() -> (PathBuf, bool) {
        match std::env::var_os(ENV_CONFIG) {
            Some(p) if !p.is_empty() => (PathBuf::from(p), true),
            _ => (PathBuf::from(DEFAULT_CONFIG_PATH), false),
        }
    }

    pub fn load_from(
        path: &Path,
        must_exist: bool,
        secrets: Secrets,
    ) -> Result<Loaded, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let mut loaded = Self::parse(&raw, secrets)?;
                loaded.source = Some(path.to_owned());
                Ok(loaded)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => {
                Self::parse("", secrets)
            }
            Err(source) => Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Parses and validates raw TOML text.
    pub fn parse(raw: &str, secrets: Secrets) -> Result<Loaded, ConfigError> {
        scan_for_secrets(raw)?;
        let mut config: Config = toml::from_str(raw)?;
        config.secrets = secrets;
        let mut warnings = Vec::new();
        config.validate(&mut warnings)?;
        Ok(Loaded {
            config,
            warnings,
            source: None,
        })
    }

    fn validate(&mut self, warnings: &mut Vec<String>) -> Result<(), ConfigError> {
        let invalid = |msg: &str| Err(ConfigError::Invalid(msg.to_owned()));
        if self.general.default_interval_secs == 0 {
            return invalid("general.default_interval_secs must be at least 1");
        }
        if self.general.jitter_percent > 100 {
            return invalid("general.jitter_percent must be between 0 and 100");
        }
        if self.general.confirm_checks == 0 {
            return invalid("general.confirm_checks must be at least 1");
        }
        if self.general.request_timeout_ms == 0 {
            return invalid("general.request_timeout_ms must be at least 1");
        }
        if self.ratelimit.max_requests_per_minute == 0 {
            return invalid("ratelimit.max_requests_per_minute must be at least 1");
        }

        let mut seen = HashSet::new();
        for target in &mut self.targets {
            target.code = normalize_code(&target.code)?;
            if target.interval_secs == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "target {:?}: interval_secs must be at least 1",
                    target.code
                )));
            }
            if !seen.insert(target.code.clone()) {
                return Err(ConfigError::DuplicateCode(target.code.clone()));
            }
        }

        if self.targets.len() > MAX_RECOMMENDED_TARGETS {
            warnings.push(format!(
                "{} targets configured; this tool is designed for at most {MAX_RECOMMENDED_TARGETS}",
                self.targets.len()
            ));
        }

        if self.auto_claim.enabled && self.secrets.user_token.is_none() {
            self.auto_claim.enabled = false;
            warnings.push(format!(
                "auto_claim.enabled = true but {ENV_USER_TOKEN} is not set; auto-claim is disabled"
            ));
        }

        let budget = self.budget();
        if budget.exceeded() {
            let default = self.default_interval();
            warnings.push(format!(
                "request budget exceeded: targets need {:.1} req/min but \
                 ratelimit.max_requests_per_minute is {}; checks will actually run {:.2}x \
                 slower (default interval {:.1}s -> effective {:.1}s)",
                budget.required_per_min,
                budget.max_per_min,
                budget.slowdown(),
                default.as_secs_f64(),
                default.as_secs_f64() * budget.slowdown(),
            ));
        }
        Ok(())
    }

    pub fn default_interval(&self) -> Duration {
        Duration::from_secs(self.general.default_interval_secs)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.general.request_timeout_ms)
    }

    /// Requests per minute the targets need at their configured intervals.
    pub fn budget(&self) -> Budget {
        let default = self.default_interval();
        let required_per_min = self
            .targets
            .iter()
            .map(|t| 60.0 / t.effective_interval(default).as_secs_f64())
            .sum();
        Budget {
            required_per_min,
            max_per_min: self.ratelimit.max_requests_per_minute,
        }
    }

    /// Per-target interval after stretching to fit the request budget.
    pub fn effective_intervals(&self) -> Vec<(String, Duration)> {
        let default = self.default_interval();
        let slowdown = self.budget().slowdown();
        self.targets
            .iter()
            .map(|t| {
                (
                    t.code.clone(),
                    t.effective_interval(default).mul_f64(slowdown),
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::Priority;
    use secrecy::ExposeSecret;

    fn parse(raw: &str) -> Result<Loaded, ConfigError> {
        Config::parse(raw, Secrets::default())
    }

    #[test]
    fn example_config_parses_cleanly() {
        let loaded = parse(include_str!("../config.example.toml")).unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let c = loaded.config;
        assert_eq!(c.general.default_interval_secs, 30);
        assert_eq!(c.ratelimit.max_requests_per_minute, 40);
        assert_eq!(c.targets.len(), 2);
        assert_eq!(c.targets[0].code, "ornekkod");
        assert_eq!(c.targets[0].interval_secs, Some(20));
        assert_eq!(c.targets[0].priority, Priority::High);
        assert_eq!(c.targets[1].priority, Priority::Normal);
        assert_eq!(c.auto_claim.confirm_checks_override, 1);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let loaded = parse("").unwrap();
        assert_eq!(loaded.config.general.log_level, "info");
        assert!(loaded.config.targets.is_empty());
    }

    #[test]
    fn code_is_lowercased() {
        let loaded = parse("[[targets]]\ncode = \"MyCode-1\"").unwrap();
        assert_eq!(loaded.config.targets[0].code, "mycode-1");
    }

    #[test]
    fn bad_codes_are_rejected() {
        for bad in ["a", &"a".repeat(33), "has space", "under_score", "tür", ""] {
            assert!(
                matches!(normalize_code(bad), Err(ConfigError::InvalidCode { .. })),
                "{bad:?} should be rejected"
            );
        }
        for good in ["ab", "a-b", "x1", &"a".repeat(32)] {
            assert!(normalize_code(good).is_ok(), "{good:?}");
        }
        let err = parse("[[targets]]\ncode = \"bad code\"").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidCode { .. }));
    }

    #[test]
    fn duplicates_are_rejected_case_insensitively() {
        let err = parse("[[targets]]\ncode = \"abc\"\n[[targets]]\ncode = \"ABC\"").unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateCode(c) if c == "abc"));
    }

    #[test]
    fn secrets_in_file_are_refused() {
        let discord_token = format!("{}.{}.{}", "A".repeat(24), "B".repeat(6), "C".repeat(27));
        let telegram = format!("{}:{}", "1".repeat(9), "a".repeat(35));
        let cases = [
            format!("[x]\ntoken = \"{discord_token}\""),
            "hook = \"https://discord.com/api/webhooks/1/abc\"".to_owned(),
            "hook = \"https://discordapp.com/api/webhooks/1/abc\"".to_owned(),
            format!("# bot\ntg = \"{telegram}\""),
        ];
        for raw in cases {
            let err = parse(&raw).unwrap_err();
            assert!(matches!(err, ConfigError::SecretInFile { .. }), "{raw}");
            // The offending value must not leak into the message.
            assert!(!err.to_string().contains("aaaaaaaaaa"));
        }
        let err = parse("a = 1\nb = \"https://discord.com/api/webhooks/1/x\"").unwrap_err();
        assert!(matches!(err, ConfigError::SecretInFile { line: 2, .. }));
    }

    #[test]
    fn invalid_numbers_are_rejected() {
        for raw in [
            "[general]\ndefault_interval_secs = 0",
            "[general]\njitter_percent = 101",
            "[general]\nconfirm_checks = 0",
            "[ratelimit]\nmax_requests_per_minute = 0",
            "[[targets]]\ncode = \"abc\"\ninterval_secs = 0",
        ] {
            assert!(matches!(parse(raw), Err(ConfigError::Invalid(_))), "{raw}");
        }
    }

    #[test]
    fn many_targets_warn() {
        let mut raw = String::from("[ratelimit]\nmax_requests_per_minute = 1000\n");
        for i in 0..26 {
            raw.push_str(&format!("[[targets]]\ncode = \"target{i}\"\n"));
        }
        let loaded = parse(&raw).unwrap();
        assert!(loaded.warnings.iter().any(|w| w.contains("26 targets")));
    }

    #[test]
    fn budget_within_limits_has_no_warning() {
        let raw = "[ratelimit]\nmax_requests_per_minute = 4\n\
                   [[targets]]\ncode = \"aa\"\n[[targets]]\ncode = \"bb\"";
        let loaded = parse(raw).unwrap();
        let b = loaded.config.budget();
        assert!((b.required_per_min - 4.0).abs() < 1e-9);
        assert!(!b.exceeded());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn budget_exceeded_warns_with_effective_interval() {
        // 4 targets at 10s = 24 req/min; budget 12 => 2x slower => 20s.
        let mut raw = String::from("[ratelimit]\nmax_requests_per_minute = 12\n");
        for code in ["aa", "bb", "cc", "dd"] {
            raw.push_str(&format!(
                "[[targets]]\ncode = \"{code}\"\ninterval_secs = 10\n"
            ));
        }
        let loaded = parse(&raw).unwrap();
        let b = loaded.config.budget();
        assert!((b.required_per_min - 24.0).abs() < 1e-9);
        assert!(b.exceeded());
        assert!((b.slowdown() - 2.0).abs() < 1e-9);
        let warning = loaded
            .warnings
            .iter()
            .find(|w| w.contains("budget"))
            .unwrap();
        assert!(warning.contains("24.0 req/min"), "{warning}");
        assert!(warning.contains("2.00x"), "{warning}");
        assert!(warning.contains("effective 60.0s"), "{warning}");
        for (_, interval) in loaded.config.effective_intervals() {
            assert_eq!(interval, Duration::from_secs(20));
        }
    }

    #[test]
    fn budget_uses_priority_adjusted_intervals() {
        // default 30s: high 15s (4/min) + normal 30s (2/min) + low 60s (1/min) = 7/min.
        let raw = "[[targets]]\ncode = \"aa\"\npriority = \"high\"\n\
                   [[targets]]\ncode = \"bb\"\n\
                   [[targets]]\ncode = \"cc\"\npriority = \"low\"";
        let b = parse(raw).unwrap().config.budget();
        assert!((b.required_per_min - 7.0).abs() < 1e-9);
    }

    #[test]
    fn auto_claim_without_user_token_is_disabled() {
        let raw = "[auto_claim]\nenabled = true";
        let loaded = parse(raw).unwrap();
        assert!(!loaded.config.auto_claim.enabled);
        assert!(loaded.warnings.iter().any(|w| w.contains(ENV_USER_TOKEN)));

        let secrets = Secrets {
            user_token: Some(SecretString::from("user-token-value")),
            ..Secrets::default()
        };
        let loaded = Config::parse(raw, secrets).unwrap();
        assert!(loaded.config.auto_claim.enabled);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn secrets_debug_output_is_redacted() {
        let secrets = Secrets {
            user_token: Some(SecretString::from("super-secret-value")),
            ..Secrets::default()
        };
        assert_eq!(
            secrets.user_token.as_ref().unwrap().expose_secret(),
            "super-secret-value"
        );
        assert!(!format!("{secrets:?}").contains("super-secret-value"));
    }

    #[test]
    fn missing_default_file_falls_back_to_defaults() {
        let path = Path::new("/nonexistent-dir/config.toml");
        let loaded = Config::load_from(path, false, Secrets::default()).unwrap();
        assert!(loaded.source.is_none());
        assert!(loaded.warnings.is_empty());
        let err = Config::load_from(path, true, Secrets::default()).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
    }
}
