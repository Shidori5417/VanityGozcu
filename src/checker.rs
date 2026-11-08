//! Invite lookups and response classification.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use thiserror::Error;

use crate::model::CheckResult;
use crate::ratelimit::RateHeaders;

pub const DEFAULT_BASE_URL: &str = "https://discord.com";
const USER_AGENT: &str = "VanityGozcu/0.1 (+https://github.com)";
/// Discord JSON error code for "Unknown Invite".
const UNKNOWN_INVITE: u64 = 10006;
const FALLBACK_RETRY_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Error)]
pub enum CheckerError {
    #[error("failed to build HTTP client: {0}")]
    ClientBuild(#[from] reqwest::Error),
}

/// A classified result together with the rate limit headers of the response.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub result: CheckResult,
    pub rate: RateHeaders,
}

#[derive(Debug, Deserialize)]
struct InviteBody {
    guild: Option<Guild>,
}

#[derive(Debug, Deserialize)]
struct Guild {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: Option<u64>,
    retry_after: Option<f64>,
    #[serde(default)]
    global: bool,
}

/// Performs invite lookups over one shared HTTP client.
#[derive(Debug, Clone)]
pub struct Checker {
    client: reqwest::Client,
    base_url: String,
}

impl Checker {
    /// `base_url` is injectable so tests can point at a mock server.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, CheckerError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// `GET {base}/api/v10/invites/{code}`. `code` must already be validated
    /// (`config::normalize_code`).
    pub async fn check(&self, code: &str) -> Checked {
        let url = format!("{}/api/v10/invites/{code}", self.base_url);
        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = if e.is_timeout() {
                    "request timed out".to_owned()
                } else {
                    format!("network error: {}", e.without_url())
                };
                return Checked {
                    result: CheckResult::Error(msg),
                    rate: RateHeaders::default(),
                };
            }
        };

        let status = response.status();
        let rate = RateHeaders::from_headers(response.headers());
        let retry_after_header = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok());

        let body = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return Checked {
                    result: CheckResult::Error(format!("failed to read body: {}", e.without_url())),
                    rate,
                };
            }
        };

        let result = classify(status, retry_after_header, &body);
        Checked { result, rate }
    }
}

/// Maps a status, optional `Retry-After` header value (seconds) and body to a result.
fn classify(status: StatusCode, retry_after_header: Option<f64>, body: &[u8]) -> CheckResult {
    match status.as_u16() {
        200 => {
            let guild = serde_json::from_slice::<InviteBody>(body)
                .ok()
                .and_then(|b| b.guild);
            CheckResult::Taken {
                guild_id: guild.as_ref().and_then(|g| g.id.clone()),
                guild_name: guild.and_then(|g| g.name),
            }
        }
        404 => match serde_json::from_slice::<ErrorBody>(body) {
            Ok(ErrorBody {
                code: Some(UNKNOWN_INVITE),
                ..
            }) => CheckResult::Free,
            _ => CheckResult::Unknown { status: 404 },
        },
        429 => {
            let parsed = serde_json::from_slice::<ErrorBody>(body).ok();
            let secs = parsed
                .as_ref()
                .and_then(|b| b.retry_after)
                .or(retry_after_header);
            CheckResult::RateLimited {
                retry_after: secs
                    .and_then(|s| Duration::try_from_secs_f64(s).ok())
                    .unwrap_or(FALLBACK_RETRY_AFTER),
                global: parsed.is_some_and(|b| b.global),
            }
        }
        500..=599 => CheckResult::Error(format!("HTTP {}", status.as_u16())),
        other => CheckResult::Unknown { status: other },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn check_with(
        server: &MockServer,
        template: ResponseTemplate,
        timeout: Duration,
    ) -> Checked {
        Mock::given(method("GET"))
            .and(path("/api/v10/invites/testcode"))
            .and(header_exists("user-agent"))
            .respond_with(template)
            .mount(server)
            .await;
        let checker = Checker::new(&server.uri(), timeout).unwrap();
        checker.check("testcode").await
    }

    const T: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn status_200_is_taken() {
        let server = MockServer::start().await;
        let body = json!({"code": "testcode", "guild": {"id": "42", "name": "Test Guild"}});
        let c = check_with(&server, ResponseTemplate::new(200).set_body_json(body), T).await;
        assert_eq!(
            c.result,
            CheckResult::Taken {
                guild_id: Some("42".into()),
                guild_name: Some("Test Guild".into())
            }
        );
    }

    #[tokio::test]
    async fn status_404_10006_is_free() {
        let server = MockServer::start().await;
        let body = json!({"message": "Unknown Invite", "code": 10006});
        let c = check_with(&server, ResponseTemplate::new(404).set_body_json(body), T).await;
        assert_eq!(c.result, CheckResult::Free);
    }

    #[tokio::test]
    async fn status_404_other_is_unknown() {
        let server = MockServer::start().await;
        let body = json!({"message": "Something else", "code": 0});
        let c = check_with(&server, ResponseTemplate::new(404).set_body_json(body), T).await;
        assert_eq!(c.result, CheckResult::Unknown { status: 404 });

        let server = MockServer::start().await;
        let c = check_with(
            &server,
            ResponseTemplate::new(404).set_body_string("<html>"),
            T,
        )
        .await;
        assert_eq!(c.result, CheckResult::Unknown { status: 404 });
    }

    #[tokio::test]
    async fn status_429_uses_json_retry_after() {
        let server = MockServer::start().await;
        let body =
            json!({"message": "You are being rate limited.", "retry_after": 1.5, "global": true});
        let t = ResponseTemplate::new(429)
            .insert_header("retry-after", "2")
            .set_body_json(body);
        let c = check_with(&server, t, T).await;
        assert_eq!(
            c.result,
            CheckResult::RateLimited {
                retry_after: Duration::from_millis(1500),
                global: true
            }
        );
    }

    #[tokio::test]
    async fn status_429_falls_back_to_header() {
        let server = MockServer::start().await;
        let t = ResponseTemplate::new(429).insert_header("retry-after", "3");
        let c = check_with(&server, t, T).await;
        assert_eq!(
            c.result,
            CheckResult::RateLimited {
                retry_after: Duration::from_secs(3),
                global: false
            }
        );
    }

    #[tokio::test]
    async fn status_500_is_error() {
        let server = MockServer::start().await;
        let c = check_with(&server, ResponseTemplate::new(500), T).await;
        assert_eq!(c.result, CheckResult::Error("HTTP 500".into()));
    }

    #[tokio::test]
    async fn other_status_is_unknown() {
        let server = MockServer::start().await;
        let c = check_with(&server, ResponseTemplate::new(403), T).await;
        assert_eq!(c.result, CheckResult::Unknown { status: 403 });
    }

    #[tokio::test]
    async fn timeout_is_error() {
        let server = MockServer::start().await;
        let t = ResponseTemplate::new(200).set_delay(Duration::from_millis(500));
        let c = check_with(&server, t, Duration::from_millis(50)).await;
        assert!(matches!(c.result, CheckResult::Error(_)), "{:?}", c.result);
    }

    #[tokio::test]
    async fn unreachable_server_is_error() {
        let checker = Checker::new("http://127.0.0.1:1", T).unwrap();
        let c = checker.check("anything").await;
        assert!(matches!(c.result, CheckResult::Error(_)));
    }

    #[tokio::test]
    async fn rate_headers_are_captured() {
        let server = MockServer::start().await;
        let t = ResponseTemplate::new(404)
            .insert_header("x-ratelimit-remaining", "0")
            .insert_header("x-ratelimit-reset-after", "2.5")
            .insert_header("x-ratelimit-bucket", "bkt")
            .set_body_json(json!({"code": 10006}));
        let c = check_with(&server, t, T).await;
        assert_eq!(c.rate.remaining, Some(0));
        assert_eq!(c.rate.reset_after, Some(Duration::from_millis(2500)));
        assert_eq!(c.rate.bucket.as_deref(), Some("bkt"));
    }
}
