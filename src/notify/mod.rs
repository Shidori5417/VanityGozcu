//! Notification fan-out.
//!
//! [`Notifier`] is an enum of the concrete channels (no boxing). Webhook URLs and bot
//! tokens are secrets: they are never logged, and transport errors are stripped of
//! their URL with `reqwest::Error::without_url` before they are turned into text.

pub mod discord;
pub mod telegram;

use std::time::{Duration, SystemTime};

use chrono::DateTime;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::config::{Config, ENV_DISCORD_WEBHOOK, ENV_TELEGRAM_TOKEN};
use crate::model::{Event, EventRecord};

pub use discord::DiscordNotifier;
pub use telegram::{DEFAULT_TELEGRAM_BASE_URL, TelegramNotifier};

/// Total attempts for a delivery (first try + retries after HTTP 429).
const MAX_ATTEMPTS: u32 = 3;
/// A `Retry-After` longer than this fails the delivery instead of stalling it.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "VanityGozcu/0.1 (+https://github.com)";

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("{notifier}: request failed: {message}")]
    Transport {
        notifier: &'static str,
        message: String,
    },
    #[error("{notifier}: server answered HTTP {status}")]
    Status { notifier: &'static str, status: u16 },
    #[error("{notifier}: rate limited, giving up (retry after {retry_after:.1}s)")]
    RateLimited {
        notifier: &'static str,
        retry_after: f64,
    },
    #[error("{notifier}: {message}")]
    Config {
        notifier: &'static str,
        message: String,
    },
}

/// Channel-neutral notification content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub code: String,
    pub fields: Vec<(String, String)>,
    pub at: SystemTime,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Free,
    Taken,
    Success,
    Warning,
    /// Needs the user right now; sent regardless of the notification flags.
    Urgent,
    Test,
}

impl Message {
    pub fn from_record(record: &EventRecord) -> Self {
        let at = record.at;
        let code = record.event.code().to_owned();
        let (title, kind, fields) = match &record.event {
            Event::BecameFree { previous_owner, .. } => (
                format!("discord.gg/{code} is FREE"),
                Kind::Free,
                vec![(
                    "Previous owner".to_owned(),
                    previous_owner
                        .clone()
                        .unwrap_or_else(|| "unknown".to_owned()),
                )],
            ),
            Event::BecameTaken { guild_name, .. } => (
                format!("discord.gg/{code} was taken again"),
                Kind::Taken,
                vec![(
                    "New owner".to_owned(),
                    guild_name.clone().unwrap_or_else(|| "unknown".to_owned()),
                )],
            ),
            Event::CheckerDegraded { reason, .. } => (
                format!("Checker degraded for {code}"),
                Kind::Warning,
                vec![("Reason".to_owned(), reason.clone())],
            ),
            Event::ClaimSucceeded { .. } => {
                (format!("Claimed discord.gg/{code}"), Kind::Success, vec![])
            }
            Event::ClaimFailed { reason, .. } => (
                format!("Claim of discord.gg/{code} failed"),
                Kind::Warning,
                vec![("Reason".to_owned(), reason.clone())],
            ),
            Event::ClaimNeedsVerification { .. } => (
                format!("URGENT: manual action required - discord.gg/{code}"),
                Kind::Urgent,
                vec![
                    (
                        "Action".to_owned(),
                        "Manual action required: Discord asked for extra verification (captcha / \
                         2FA) for the claim. Auto-claim has stopped; claim the URL by hand and \
                         check your account."
                            .to_owned(),
                    ),
                    (
                        "Auto-claim".to_owned(),
                        "halted for this session".to_owned(),
                    ),
                ],
            ),
        };
        Self {
            title,
            code,
            fields,
            at,
            kind,
        }
    }

    pub fn test(at: SystemTime) -> Self {
        Self {
            title: "VanityGozcu test notification".to_owned(),
            code: "test".to_owned(),
            fields: vec![("Status".to_owned(), "notifications work".to_owned())],
            at,
            kind: Kind::Test,
        }
    }

    pub fn time_text(&self) -> String {
        let secs = self
            .at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        DateTime::from_timestamp(secs, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_default()
    }

    pub fn time_rfc3339(&self) -> String {
        let secs = self
            .at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        DateTime::from_timestamp(secs, 0)
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    }
}

/// Last four characters of a secret, the only part ever shown (GUVENLIK.md).
pub fn secret_tail(secret: &SecretString) -> String {
    let s = secret.expose_secret();
    let tail: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("...{tail}")
}

pub(crate) fn build_client(timeout: Duration) -> Result<reqwest::Client, NotifyError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .map_err(|e| NotifyError::Transport {
            notifier: "http",
            message: e.without_url().to_string(),
        })
}

#[derive(serde::Deserialize)]
struct RetryBody {
    /// Discord: top level.
    retry_after: Option<f64>,
    /// Telegram: `parameters.retry_after`.
    parameters: Option<RetryParameters>,
}

#[derive(serde::Deserialize)]
struct RetryParameters {
    retry_after: Option<f64>,
}

/// POSTs JSON, retrying (bounded) on HTTP 429 according to `Retry-After`.
pub(crate) async fn post_json<B: Serialize + Sync>(
    client: &reqwest::Client,
    notifier: &'static str,
    url: &str,
    body: &B,
) -> Result<(), NotifyError> {
    for attempt in 1..=MAX_ATTEMPTS {
        let response =
            client
                .post(url)
                .json(body)
                .send()
                .await
                .map_err(|e| NotifyError::Transport {
                    notifier,
                    message: if e.is_timeout() {
                        "request timed out".to_owned()
                    } else {
                        e.without_url().to_string()
                    },
                })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status != StatusCode::TOO_MANY_REQUESTS {
            return Err(NotifyError::Status {
                notifier,
                status: status.as_u16(),
            });
        }
        let header = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok());
        let from_body = match header {
            Some(_) => None,
            None => response
                .json::<RetryBody>()
                .await
                .ok()
                .and_then(|b| b.retry_after.or(b.parameters.and_then(|p| p.retry_after))),
        };
        let wait = header.or(from_body).unwrap_or(1.0).max(0.0);
        if attempt == MAX_ATTEMPTS || wait > MAX_RETRY_WAIT.as_secs_f64() {
            return Err(NotifyError::RateLimited {
                notifier,
                retry_after: wait,
            });
        }
        debug!(notifier, wait, attempt, "rate limited, retrying");
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    }
    // The loop always returns; kept for completeness of the type system.
    Err(NotifyError::RateLimited {
        notifier,
        retry_after: 0.0,
    })
}

/// One configured notification channel.
#[derive(Debug)]
pub enum Notifier {
    Discord(DiscordNotifier),
    Telegram(TelegramNotifier),
}

impl Notifier {
    pub fn name(&self) -> &'static str {
        match self {
            Notifier::Discord(_) => "discord",
            Notifier::Telegram(_) => "telegram",
        }
    }

    pub async fn send(&self, message: &Message) -> Result<(), NotifyError> {
        match self {
            Notifier::Discord(n) => n.send(message).await,
            Notifier::Telegram(n) => n.send(message).await,
        }
    }
}

/// Which event kinds are delivered (`[notify] on_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    pub on_free: bool,
    pub on_taken: bool,
    pub on_claim: bool,
    pub on_error: bool,
}

impl Flags {
    pub fn allows(&self, event: &Event) -> bool {
        match event {
            Event::BecameFree { .. } => self.on_free,
            Event::BecameTaken { .. } => self.on_taken,
            Event::CheckerDegraded { .. } => self.on_error,
            Event::ClaimSucceeded { .. } | Event::ClaimFailed { .. } => self.on_claim,
            // Needs a human right away: never filtered.
            Event::ClaimNeedsVerification { .. } => true,
        }
    }
}

/// The set of active notifiers plus the event filter.
#[derive(Debug)]
pub struct Notifiers {
    pub flags: Flags,
    pub notifiers: Vec<Notifier>,
}

impl Notifiers {
    pub fn new(flags: Flags, notifiers: Vec<Notifier>) -> Self {
        Self { flags, notifiers }
    }

    /// Builds the enabled notifiers. Channels that are enabled but unusable (missing
    /// secret or chat id) are reported in the returned error list instead of aborting.
    pub fn from_config(config: &Config) -> (Self, Vec<NotifyError>) {
        let n = &config.notify;
        let timeout = config.request_timeout();
        let mut notifiers = Vec::new();
        let mut problems = Vec::new();

        if n.discord.enabled {
            match &config.secrets.discord_webhook {
                None => problems.push(NotifyError::Config {
                    notifier: "discord",
                    message: format!("enabled but {ENV_DISCORD_WEBHOOK} is not set"),
                }),
                Some(url) => {
                    match DiscordNotifier::new(url.clone(), n.discord.mention.clone(), timeout) {
                        Ok(d) => {
                            info!("discord notifier enabled (webhook {})", secret_tail(url));
                            notifiers.push(Notifier::Discord(d));
                        }
                        Err(e) => problems.push(e),
                    }
                }
            }
        }
        if n.telegram.enabled {
            match (&config.secrets.telegram_token, &n.telegram.chat_id) {
                (None, _) => problems.push(NotifyError::Config {
                    notifier: "telegram",
                    message: format!("enabled but {ENV_TELEGRAM_TOKEN} is not set"),
                }),
                (_, None) => problems.push(NotifyError::Config {
                    notifier: "telegram",
                    message: "enabled but [notify.telegram] chat_id is not set".to_owned(),
                }),
                (Some(token), Some(chat_id)) => {
                    match TelegramNotifier::new(
                        DEFAULT_TELEGRAM_BASE_URL,
                        token.clone(),
                        chat_id.clone(),
                        timeout,
                    ) {
                        Ok(t) => {
                            info!("telegram notifier enabled (token {})", secret_tail(token));
                            notifiers.push(Notifier::Telegram(t));
                        }
                        Err(e) => problems.push(e),
                    }
                }
            }
        }
        let flags = Flags {
            on_free: n.on_free,
            on_taken: n.on_taken,
            on_claim: n.on_claim,
            on_error: n.on_error,
        };
        (Self::new(flags, notifiers), problems)
    }

    pub fn is_empty(&self) -> bool {
        self.notifiers.is_empty()
    }

    /// Sends `message` through every notifier; one result per notifier.
    pub async fn send_all(
        &self,
        message: &Message,
    ) -> Vec<(&'static str, Result<(), NotifyError>)> {
        let mut out = Vec::with_capacity(self.notifiers.len());
        for n in &self.notifiers {
            out.push((n.name(), n.send(message).await));
        }
        out
    }

    /// Delivers an event if the flags allow it. Failures are logged, never returned.
    pub async fn dispatch(&self, record: &EventRecord) {
        if !self.flags.allows(&record.event) {
            debug!(
                kind = record.event.kind(),
                "notification disabled by config"
            );
            return;
        }
        let message = Message::from_record(record);
        for (name, result) in self.send_all(&message).await {
            match result {
                Ok(()) => info!(
                    notifier = name,
                    kind = record.event.kind(),
                    "notification sent"
                ),
                Err(e) => error!(
                    notifier = name,
                    kind = record.event.kind(),
                    "notification failed: {e}"
                ),
            }
        }
        if self.notifiers.is_empty() {
            warn!(
                kind = record.event.kind(),
                "no notifier configured, event not delivered"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn secret_tail_shows_only_four_chars() {
        let s = SecretString::from("https://discord.com/api/webhooks/1/SUPERSECRETabcd".to_owned());
        assert_eq!(secret_tail(&s), "...abcd");
    }

    #[test]
    fn flags_map_events() {
        let f = Flags {
            on_free: true,
            on_taken: false,
            on_claim: false,
            on_error: true,
        };
        assert!(f.allows(&Event::BecameFree {
            code: "a".into(),
            previous_owner: None
        }));
        assert!(!f.allows(&Event::BecameTaken {
            code: "a".into(),
            guild_name: None
        }));
        assert!(!f.allows(&Event::ClaimSucceeded { code: "a".into() }));
        assert!(!f.allows(&Event::ClaimFailed {
            code: "a".into(),
            reason: "r".into()
        }));
        // Verification requests are urgent and bypass every flag.
        let none = Flags {
            on_free: false,
            on_taken: false,
            on_claim: false,
            on_error: false,
        };
        assert!(none.allows(&Event::ClaimNeedsVerification { code: "a".into() }));
        let msg = Message::from_record(&EventRecord {
            event: Event::ClaimNeedsVerification { code: "a".into() },
            at: SystemTime::UNIX_EPOCH,
        });
        assert_eq!(msg.kind, Kind::Urgent);
        assert!(msg.title.contains("manual action required"));
        assert!(msg.fields[0].1.contains("Manual action required"));
        assert!(f.allows(&Event::CheckerDegraded {
            code: "a".into(),
            reason: "r".into()
        }));
    }

    #[test]
    fn missing_secrets_are_reported_not_fatal() {
        let loaded = Config::parse(
            "[notify.discord]\nenabled = true\n[notify.telegram]\nenabled = true\n",
            crate::config::Secrets::default(),
        )
        .unwrap();
        let (n, problems) = Notifiers::from_config(&loaded.config);
        assert!(n.is_empty());
        assert_eq!(problems.len(), 2);
        assert!(problems[0].to_string().contains("VG_DISCORD_WEBHOOK"));
        assert!(problems[1].to_string().contains("VG_TELEGRAM_TOKEN"));
    }
}
