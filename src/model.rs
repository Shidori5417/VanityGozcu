//! Shared domain types.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// Relative check frequency of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

impl Priority {
    /// Multiplier applied to the default interval (smaller means more frequent).
    pub fn interval_factor(self) -> f64 {
        match self {
            Priority::High => 0.5,
            Priority::Normal => 1.0,
            Priority::Low => 2.0,
        }
    }
}

/// A vanity code under watch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VanityTarget {
    pub code: String,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub auto_claim: Option<bool>,
}

impl VanityTarget {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            interval_secs: None,
            priority: Priority::Normal,
            auto_claim: None,
        }
    }

    /// Check interval before jitter: an explicit `interval_secs` wins, otherwise the
    /// default interval scaled by priority.
    pub fn effective_interval(&self, default: Duration) -> Duration {
        match self.interval_secs {
            Some(secs) => Duration::from_secs(secs),
            None => default.mul_f64(self.priority.interval_factor()),
        }
    }
}

/// Classified outcome of a single invite lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    /// The code belongs to a guild.
    Taken {
        guild_id: Option<String>,
        guild_name: Option<String>,
    },
    /// Discord reports the invite as unknown (error code 10006).
    Free,
    /// HTTP 429. `global` is Discord's own flag for a global limit.
    RateLimited { retry_after: Duration, global: bool },
    /// Transient failure: 5xx, timeout or network error.
    Error(String),
    /// Unexpected status.
    Unknown { status: u16 },
}

impl fmt::Display for CheckResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckResult::Taken {
                guild_id,
                guild_name,
            } => write!(
                f,
                "TAKEN (guild_id={}, guild_name={})",
                guild_id.as_deref().unwrap_or("?"),
                guild_name.as_deref().unwrap_or("?")
            ),
            CheckResult::Free => write!(f, "FREE"),
            CheckResult::RateLimited {
                retry_after,
                global,
            } => write!(
                f,
                "RATE LIMITED (retry_after={:.3}s, global={global})",
                retry_after.as_secs_f64()
            ),
            CheckResult::Error(msg) => write!(f, "ERROR ({msg})"),
            CheckResult::Unknown { status } => write!(f, "UNKNOWN (HTTP {status})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_interval_respects_priority_and_override() {
        let default = Duration::from_secs(30);
        let mut t = VanityTarget::new("abc");
        assert_eq!(t.effective_interval(default), Duration::from_secs(30));
        t.priority = Priority::High;
        assert_eq!(t.effective_interval(default), Duration::from_secs(15));
        t.priority = Priority::Low;
        assert_eq!(t.effective_interval(default), Duration::from_secs(60));
        t.interval_secs = Some(7);
        assert_eq!(t.effective_interval(default), Duration::from_secs(7));
    }
}
