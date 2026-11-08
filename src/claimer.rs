//! Optional auto-claim: `PATCH /guilds/{id}/vanity-url` with a user token, plus the
//! startup precheck (`GET /guilds/{id}`).
//!
//! Hard limits (GUVENLIK.md): one token, one configured guild, no proxies, no
//! rate-limit evasion and no captcha/MFA handling. Whenever Discord asks for any kind
//! of verification the attempt stops at once ([`ClaimOutcome::NeedsVerification`]).
//!
//! The token lives in a [`SecretString`] only until the `Authorization` header is
//! built in [`Claimer::new`]; that header is marked sensitive and prebuilt together
//! with the HTTP client, so the claim path does no header work. It shares one client
//! (and thus one warm connection, opened by the precheck) with the precheck.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Method, redirect};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use thiserror::Error;
use tracing::{debug, warn};

use crate::ratelimit::{RateLimiter, backoff_delay};

const USER_AGENT: &str = "VanityGozcu/0.1 (+https://github.com)";
/// Bucket name used to pause the shared limiter after a non-global 429 of the claim route.
const RATE_BUCKET: &str = "vanity-url";
/// A `Retry-After` longer than this ends the attempt instead of stalling the claim.
const MAX_CLAIM_WAIT: Duration = Duration::from_secs(30);
const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(5);
const FALLBACK_RETRY_AFTER: Duration = Duration::from_secs(1);
/// Discord error code "Two factor is required for this operation".
const MFA_REQUIRED: u64 = 60003;
const VANITY_FEATURE: &str = "VANITY_URL";
const PERM_ADMINISTRATOR: u64 = 0x8;
const PERM_MANAGE_GUILD: u64 = 0x20;
/// Longest Discord error message copied into a reason.
const MAX_MESSAGE_CHARS: usize = 200;

#[derive(Debug, Error)]
pub enum ClaimerError {
    #[error("the user token contains characters that are not valid in an HTTP header")]
    InvalidToken,
    #[error("invalid target guild id {0:?}: expected only digits")]
    InvalidGuildId(String),
    #[error("failed to build HTTP client: {0}")]
    ClientBuild(#[from] reqwest::Error),
}

/// Final result of a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Succeeded,
    /// Rejected or gave up; `reason` never contains the token.
    Failed {
        reason: String,
    },
    /// Discord wants captcha / MFA; attempts stopped, a human has to act.
    NeedsVerification,
    /// HTTP 401: the token is unusable, auto-claim must stay off for the session.
    InvalidToken,
}

/// Result of the startup precheck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrecheckOutcome {
    Ok { note: Option<String> },
    Failed { reason: String },
    NeedsVerification,
    InvalidToken,
}

/// Why an exchange ended without a success body.
#[derive(Debug)]
enum Stop {
    Failed(String),
    Verification,
    InvalidToken,
}

/// Verdict for one HTTP round trip.
#[derive(Debug)]
enum Step {
    Done(Vec<u8>),
    Stop(Stop),
    /// Try again; `wait` is Discord's `Retry-After` (otherwise the backoff applies).
    Retry {
        reason: String,
        wait: Option<Duration>,
        global: bool,
    },
}

/// Sends the vanity claim and the precheck over one prebuilt client.
#[derive(Debug)]
pub struct Claimer {
    client: reqwest::Client,
    patch_url: String,
    guild_url: String,
    guild_id: String,
    max_attempts: u32,
    retry_backoff: Duration,
    limiter: Arc<RateLimiter>,
}

impl Claimer {
    /// `base_url` is injectable so tests can point at a mock server. `limiter` is the
    /// checks' limiter: a 429 here pauses it too.
    pub fn new(
        base_url: &str,
        token: &SecretString,
        guild_id: &str,
        timeout: Duration,
        max_attempts: u32,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self, ClaimerError> {
        if guild_id.is_empty() || !guild_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ClaimerError::InvalidGuildId(guild_id.to_owned()));
        }
        // The only place the token is exposed.
        let mut auth =
            HeaderValue::from_str(token.expose_secret()).map_err(|_| ClaimerError::InvalidToken)?;
        auth.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .default_headers(headers)
            // The token must never follow a redirect.
            .redirect(redirect::Policy::none())
            .build()?;
        let base = base_url.trim_end_matches('/');
        let guild_url = format!("{base}/api/v10/guilds/{guild_id}");
        Ok(Self {
            client,
            patch_url: format!("{guild_url}/vanity-url"),
            guild_url,
            guild_id: guild_id.to_owned(),
            max_attempts: max_attempts.max(1),
            retry_backoff: DEFAULT_RETRY_BACKOFF,
            limiter,
        })
    }

    /// Base delay for 5xx / network retries (doubles per attempt, capped at 5 s).
    #[must_use]
    pub fn with_retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// Claims `code` for the configured guild. `code` must already be validated.
    pub async fn claim(&self, code: &str) -> ClaimOutcome {
        let body = serde_json::json!({ "code": code }).to_string();
        match self
            .exchange(Method::PATCH, &self.patch_url, Some(&body))
            .await
        {
            Ok(_) => ClaimOutcome::Succeeded,
            Err(Stop::Failed(reason)) => ClaimOutcome::Failed { reason },
            Err(Stop::Verification) => ClaimOutcome::NeedsVerification,
            Err(Stop::InvalidToken) => ClaimOutcome::InvalidToken,
        }
    }

    /// Checks that the guild exists for this account, has `VANITY_URL` and (when
    /// Discord reports permissions) that the account may manage it.
    pub async fn precheck(&self) -> PrecheckOutcome {
        let id = &self.guild_id;
        match self.exchange(Method::GET, &self.guild_url, None).await {
            Ok(body) => evaluate_guild(&body, id),
            Err(Stop::Failed(reason)) => PrecheckOutcome::Failed {
                reason: format!("cannot read target guild {id}: {reason}"),
            },
            Err(Stop::Verification) => PrecheckOutcome::NeedsVerification,
            Err(Stop::InvalidToken) => PrecheckOutcome::InvalidToken,
        }
    }

    /// One request with the retry policy: 429 waits `Retry-After` (and pauses the
    /// shared limiter), 5xx / network errors back off exponentially, everything else
    /// is final. At most `max_attempts` requests are made.
    async fn exchange(
        &self,
        method: Method,
        url: &str,
        body: Option<&str>,
    ) -> Result<Vec<u8>, Stop> {
        let mut last = String::new();
        for attempt in 1..=self.max_attempts {
            let mut request = self.client.request(method.clone(), url);
            if let Some(body) = body {
                request = request.body(body.to_owned());
            }
            let step = self.step(request).await;
            let (reason, wait, global) = match step {
                Step::Done(body) => return Ok(body),
                Step::Stop(stop) => return Err(stop),
                Step::Retry {
                    reason,
                    wait,
                    global,
                } => (reason, wait, global),
            };
            if let Some(wait) = wait {
                if global {
                    self.limiter.pause_global(wait);
                } else {
                    self.limiter.pause_bucket(RATE_BUCKET, wait);
                }
            }
            if attempt == self.max_attempts {
                last = reason;
                break;
            }
            if let Some(wait) = wait
                && wait > MAX_CLAIM_WAIT
            {
                return Err(Stop::Failed(format!(
                    "{reason}; waiting {:.0}s is too long, giving up",
                    wait.as_secs_f64()
                )));
            }
            let delay = wait.unwrap_or_else(|| {
                backoff_delay(self.retry_backoff, attempt - 1, MAX_RETRY_BACKOFF)
            });
            debug!(attempt, ?delay, %reason, "retrying Discord request");
            tokio::time::sleep(delay).await;
        }
        Err(Stop::Failed(format!(
            "gave up after {} attempts: {last}",
            self.max_attempts
        )))
    }

    async fn step(&self, request: reqwest::RequestBuilder) -> Step {
        let retry = |reason: String| Step::Retry {
            reason,
            wait: None,
            global: false,
        };
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return retry("request timed out".to_owned()),
            Err(e) => return retry(format!("network error: {}", e.without_url())),
        };
        let status = response.status().as_u16();
        let retry_header = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok());
        let global_header = response
            .headers()
            .get("x-ratelimit-global")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"true"));
        match response.bytes().await {
            Ok(body) => classify(status, retry_header, global_header, body.to_vec()),
            Err(e) => retry(format!("failed to read body: {}", e.without_url())),
        }
    }
}

/// Maps a response to the next step. Pure, so it is unit-testable.
fn classify(status: u16, retry_header: Option<f64>, global_header: bool, body: Vec<u8>) -> Step {
    if (200..300).contains(&status) {
        return Step::Done(body);
    }
    let json = serde_json::from_slice::<Value>(&body).ok();
    if requires_verification(json.as_ref()) {
        return Step::Stop(Stop::Verification);
    }
    match status {
        401 => Step::Stop(Stop::InvalidToken),
        429 => {
            let wait = json
                .as_ref()
                .and_then(|j| j.get("retry_after"))
                .and_then(Value::as_f64)
                .or(retry_header)
                .and_then(|s| Duration::try_from_secs_f64(s).ok())
                .unwrap_or(FALLBACK_RETRY_AFTER);
            let global = global_header
                || json
                    .as_ref()
                    .and_then(|j| j.get("global"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            Step::Retry {
                reason: format!("rate limited (retry after {:.1}s)", wait.as_secs_f64()),
                wait: Some(wait),
                global,
            }
        }
        500..=599 => Step::Retry {
            reason: format!("HTTP {status}"),
            wait: None,
            global: false,
        },
        _ => Step::Stop(Stop::Failed(describe_error(status, json.as_ref()))),
    }
}

/// Captcha (`captcha_key` / `captcha_sitekey`) or MFA (error code 60003, or an `mfa`
/// object) in an error body. Detected on any status, including 401 and 403.
fn requires_verification(json: Option<&Value>) -> bool {
    let Some(obj) = json.and_then(Value::as_object) else {
        return false;
    };
    obj.contains_key("captcha_key")
        || obj.contains_key("captcha_sitekey")
        || obj.get("code").and_then(Value::as_u64) == Some(MFA_REQUIRED)
        || obj.get("mfa").is_some_and(Value::is_object)
}

fn describe_error(status: u16, json: Option<&Value>) -> String {
    let code = json.and_then(|j| j.get("code")).and_then(Value::as_u64);
    let message = json.and_then(|j| j.get("message")).and_then(Value::as_str);
    match (code, message) {
        (Some(code), Some(message)) => format!(
            "Discord error {code}: {} (HTTP {status})",
            message.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
        ),
        (Some(code), None) => format!("Discord error {code} (HTTP {status})"),
        _ => format!("HTTP {status} without a Discord error body"),
    }
}

fn evaluate_guild(body: &[u8], guild_id: &str) -> PrecheckOutcome {
    let failed = |reason: String| PrecheckOutcome::Failed { reason };
    let Ok(guild) = serde_json::from_slice::<Value>(body) else {
        return failed(format!("unexpected answer for guild {guild_id}"));
    };
    let has_vanity = guild
        .get("features")
        .and_then(Value::as_array)
        .is_some_and(|f| f.iter().any(|v| v.as_str() == Some(VANITY_FEATURE)));
    if !has_vanity {
        return failed(format!(
            "guild {guild_id} has no {VANITY_FEATURE} feature (needs Boost level 3 or a Discord grant)"
        ));
    }
    let owner = guild.get("owner").and_then(Value::as_bool) == Some(true);
    let permissions = guild.get("permissions").and_then(|p| match p {
        Value::String(s) => s.parse::<u64>().ok(),
        other => other.as_u64(),
    });
    match permissions {
        Some(p) if !owner && p & (PERM_ADMINISTRATOR | PERM_MANAGE_GUILD) == 0 => failed(format!(
            "this account lacks the MANAGE_GUILD permission in guild {guild_id}"
        )),
        Some(_) => PrecheckOutcome::Ok { note: None },
        None if owner => PrecheckOutcome::Ok { note: None },
        None => {
            warn!(
                "guild answer carries no permissions; MANAGE_GUILD is verified on the first claim"
            );
            PrecheckOutcome::Ok {
                note: Some("MANAGE_GUILD is verified on the first claim".to_owned()),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::time::Instant;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "mfa.SUPER-SECRET-token-value-123";
    const GUILD: &str = "123456789012345678";
    const PATCH_PATH: &str = "/api/v10/guilds/123456789012345678/vanity-url";
    const GUILD_PATH: &str = "/api/v10/guilds/123456789012345678";

    fn claimer_with(server: &MockServer, max_attempts: u32, limiter: Arc<RateLimiter>) -> Claimer {
        Claimer::new(
            &server.uri(),
            &SecretString::from(TOKEN.to_owned()),
            GUILD,
            Duration::from_secs(30),
            max_attempts,
            limiter,
        )
        .unwrap()
        .with_retry_backoff(Duration::from_millis(10))
    }

    fn claimer(server: &MockServer, max_attempts: u32) -> Claimer {
        claimer_with(server, max_attempts, Arc::new(RateLimiter::new(600)))
    }

    async fn mount_patch(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("PATCH"))
            .and(path(PATCH_PATH))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn request_count(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    fn failed_reason(outcome: ClaimOutcome) -> String {
        match outcome {
            ClaimOutcome::Failed { reason } => reason,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn success_sends_code_and_authorization() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(PATCH_PATH))
            .and(header("authorization", TOKEN))
            .and(body_json(json!({"code": "mycode"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": "mycode"})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            claimer(&server, 3).claim("mycode").await,
            ClaimOutcome::Succeeded
        );
    }

    #[tokio::test]
    async fn code_taken_400_is_failed_with_discord_error() {
        let server = MockServer::start().await;
        mount_patch(
            &server,
            ResponseTemplate::new(400)
                .set_body_json(json!({"code": 50035, "message": "Invalid Form Body"})),
        )
        .await;
        let reason = failed_reason(claimer(&server, 3).claim("taken").await);
        assert!(
            reason.contains("50035") && reason.contains("Invalid Form Body"),
            "{reason}"
        );
        assert_eq!(request_count(&server).await, 1, "4xx must not be retried");
    }

    #[tokio::test]
    async fn missing_permission_403_and_404_are_failed() {
        for (status, code, message) in [
            (403, 50013, "Missing Permissions"),
            (404, 10004, "Unknown Guild"),
        ] {
            let server = MockServer::start().await;
            mount_patch(
                &server,
                ResponseTemplate::new(status)
                    .set_body_json(json!({"code": code, "message": message})),
            )
            .await;
            let reason = failed_reason(claimer(&server, 3).claim("abc").await);
            assert!(
                reason.contains(&code.to_string()) && reason.contains(message),
                "{reason}"
            );
            assert_eq!(request_count(&server).await, 1);
        }
    }

    #[tokio::test]
    async fn non_discord_error_body_is_failed_without_leaking_it() {
        let server = MockServer::start().await;
        mount_patch(
            &server,
            ResponseTemplate::new(403).set_body_string("<html>blocked</html>"),
        )
        .await;
        let reason = failed_reason(claimer(&server, 3).claim("abc").await);
        assert!(
            reason.contains("403") && !reason.contains("html"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn captcha_body_needs_verification_after_exactly_one_request() {
        for status in [400, 403] {
            let server = MockServer::start().await;
            mount_patch(
                &server,
                ResponseTemplate::new(status).set_body_json(json!({
                    "captcha_key": ["captcha-required"],
                    "captcha_sitekey": "abc",
                    "captcha_service": "hcaptcha"
                })),
            )
            .await;
            assert_eq!(
                claimer(&server, 5).claim("abc").await,
                ClaimOutcome::NeedsVerification
            );
            assert_eq!(
                request_count(&server).await,
                1,
                "must stop after the first answer"
            );
        }
    }

    #[tokio::test]
    async fn mfa_required_in_every_form_needs_verification() {
        let bodies = [
            (
                403,
                json!({"code": 60003, "message": "Two factor is required"}),
            ),
            (
                401,
                json!({"code": 60003, "message": "Two factor is required"}),
            ),
            (
                400,
                json!({"code": 60003, "mfa": {"ticket": "t", "methods": []}}),
            ),
            (403, json!({"message": "x", "mfa": {"ticket": "t"}})),
        ];
        for (status, body) in bodies {
            let server = MockServer::start().await;
            mount_patch(&server, ResponseTemplate::new(status).set_body_json(body)).await;
            assert_eq!(
                claimer(&server, 5).claim("abc").await,
                ClaimOutcome::NeedsVerification,
                "status {status}"
            );
            assert_eq!(request_count(&server).await, 1);
        }
    }

    #[tokio::test]
    async fn plain_401_is_invalid_token_without_retry() {
        let server = MockServer::start().await;
        mount_patch(
            &server,
            ResponseTemplate::new(401)
                .set_body_json(json!({"code": 0, "message": "401: Unauthorized"})),
        )
        .await;
        assert_eq!(
            claimer(&server, 5).claim("abc").await,
            ClaimOutcome::InvalidToken
        );
        assert_eq!(request_count(&server).await, 1);
    }

    #[tokio::test]
    async fn rate_limit_then_success_obeys_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(PATCH_PATH))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0.3")
                    .set_body_json(
                        json!({"message": "slow down", "retry_after": 0.3, "global": false}),
                    ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_patch(&server, ResponseTemplate::new(200).set_body_json(json!({}))).await;
        let started = Instant::now();
        let outcome = claimer(&server, 3).claim("abc").await;
        assert_eq!(outcome, ClaimOutcome::Succeeded);
        assert!(
            started.elapsed() >= Duration::from_millis(290),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(request_count(&server).await, 2);
    }

    #[tokio::test]
    async fn global_rate_limit_pauses_the_shared_limiter() {
        let server = MockServer::start().await;
        mount_patch(
            &server,
            ResponseTemplate::new(429)
                .set_body_json(json!({"message": "global", "retry_after": 0.6, "global": true})),
        )
        .await;
        let limiter = Arc::new(RateLimiter::with_burst(6000, 10));
        let reason = failed_reason(claimer_with(&server, 1, limiter.clone()).claim("abc").await);
        assert!(
            reason.contains("gave up after 1 attempts") && reason.contains("rate limited"),
            "{reason}"
        );
        let before = Instant::now();
        limiter.acquire().await;
        assert!(
            before.elapsed() >= Duration::from_millis(550),
            "{:?}",
            before.elapsed()
        );
    }

    #[tokio::test]
    async fn rate_limit_exhausts_max_attempts() {
        let server = MockServer::start().await;
        mount_patch(
            &server,
            ResponseTemplate::new(429).set_body_json(json!({"retry_after": 0.05, "global": false})),
        )
        .await;
        let reason = failed_reason(claimer(&server, 3).claim("abc").await);
        assert!(reason.contains("gave up after 3 attempts"), "{reason}");
        assert_eq!(request_count(&server).await, 3);
    }

    #[tokio::test]
    async fn huge_retry_after_gives_up_early() {
        let server = MockServer::start().await;
        mount_patch(
            &server,
            ResponseTemplate::new(429)
                .set_body_json(json!({"retry_after": 3600.0, "global": false})),
        )
        .await;
        let reason = failed_reason(claimer(&server, 3).claim("abc").await);
        assert!(reason.contains("too long"), "{reason}");
        assert_eq!(request_count(&server).await, 1);
    }

    #[tokio::test]
    async fn server_errors_retry_with_backoff_then_fail() {
        let server = MockServer::start().await;
        mount_patch(&server, ResponseTemplate::new(503)).await;
        let started = Instant::now();
        let reason = failed_reason(claimer(&server, 3).claim("abc").await);
        assert!(
            reason.contains("gave up after 3 attempts") && reason.contains("503"),
            "{reason}"
        );
        assert_eq!(request_count(&server).await, 3);
        // 10 ms + 20 ms between the three attempts.
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn server_error_then_success() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        mount_patch(&server, ResponseTemplate::new(200).set_body_json(json!({}))).await;
        assert_eq!(
            claimer(&server, 3).claim("abc").await,
            ClaimOutcome::Succeeded
        );
    }

    #[tokio::test]
    async fn network_error_retries_then_fails() {
        // Nothing listens on port 1: connection refused.
        let uri = "http://127.0.0.1:1".to_owned();
        let claimer = Claimer::new(
            &uri,
            &SecretString::from(TOKEN.to_owned()),
            GUILD,
            Duration::from_secs(2),
            2,
            Arc::new(RateLimiter::new(600)),
        )
        .unwrap()
        .with_retry_backoff(Duration::from_millis(1));
        let reason = failed_reason(claimer.claim("abc").await);
        assert!(
            reason.contains("gave up after 2 attempts") && reason.contains("network error"),
            "{reason}"
        );
        assert!(!reason.contains(&uri), "URLs must be stripped: {reason}");
    }

    fn guild_json(features: &[&str], extra: Value) -> Value {
        let mut v = json!({"id": GUILD, "name": "G", "features": features});
        if let (Some(obj), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
            obj.extend(extra.clone());
        }
        v
    }

    async fn precheck_with(status: u16, body: Value) -> PrecheckOutcome {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(GUILD_PATH))
            .and(header("authorization", TOKEN))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        claimer(&server, 2).precheck().await
    }

    #[tokio::test]
    async fn precheck_ok_with_and_without_permissions() {
        // MANAGE_GUILD (0x20) present.
        let ok = precheck_with(
            200,
            guild_json(&["VANITY_URL"], json!({"permissions": "32"})),
        )
        .await;
        assert_eq!(ok, PrecheckOutcome::Ok { note: None });
        // Owner without a permissions field.
        let ok = precheck_with(200, guild_json(&["VANITY_URL"], json!({"owner": true}))).await;
        assert_eq!(ok, PrecheckOutcome::Ok { note: None });
        // Neither: accepted with a note.
        let PrecheckOutcome::Ok { note: Some(note) } =
            precheck_with(200, guild_json(&["VANITY_URL"], json!({}))).await
        else {
            panic!("expected Ok with note");
        };
        assert!(note.contains("first claim"));
    }

    #[tokio::test]
    async fn precheck_missing_vanity_feature_fails() {
        let PrecheckOutcome::Failed { reason } =
            precheck_with(200, guild_json(&["COMMUNITY"], json!({"permissions": "8"}))).await
        else {
            panic!("expected Failed");
        };
        assert!(reason.contains("VANITY_URL"), "{reason}");
    }

    #[tokio::test]
    async fn precheck_without_manage_guild_fails() {
        let PrecheckOutcome::Failed { reason } = precheck_with(
            200,
            guild_json(&["VANITY_URL"], json!({"permissions": "1024"})),
        )
        .await
        else {
            panic!("expected Failed");
        };
        assert!(reason.contains("MANAGE_GUILD"), "{reason}");
    }

    #[tokio::test]
    async fn precheck_401_403_and_verification() {
        assert_eq!(
            precheck_with(401, json!({"code": 0, "message": "401: Unauthorized"})).await,
            PrecheckOutcome::InvalidToken
        );
        let PrecheckOutcome::Failed { reason } =
            precheck_with(403, json!({"code": 50001, "message": "Missing Access"})).await
        else {
            panic!("expected Failed");
        };
        assert!(
            reason.contains("50001") && reason.contains(GUILD),
            "{reason}"
        );
        assert_eq!(
            precheck_with(403, json!({"captcha_key": ["x"]})).await,
            PrecheckOutcome::NeedsVerification
        );
    }

    #[test]
    fn token_never_shows_up_in_debug_or_errors() {
        let secret = SecretString::from(TOKEN.to_owned());
        let limiter = Arc::new(RateLimiter::new(60));
        let claimer = Claimer::new(
            "http://127.0.0.1:1",
            &secret,
            GUILD,
            Duration::from_secs(1),
            3,
            limiter.clone(),
        )
        .unwrap();
        let debug = format!("{claimer:?}");
        assert!(
            !debug.contains(TOKEN) && !debug.contains("SUPER-SECRET"),
            "{debug}"
        );

        // Errors for bad input do not echo the token either.
        let bad = SecretString::from("bad\ntoken-SECRET".to_owned());
        let err = Claimer::new(
            "http://x",
            &bad,
            GUILD,
            Duration::from_secs(1),
            1,
            limiter.clone(),
        )
        .unwrap_err();
        assert!(!format!("{err} {err:?}").contains("SECRET"));
        let err = Claimer::new(
            "http://x",
            &secret,
            "12ab",
            Duration::from_secs(1),
            1,
            limiter,
        )
        .unwrap_err();
        assert!(matches!(err, ClaimerError::InvalidGuildId(_)));
        assert!(!format!("{err} {err:?}").contains("SUPER-SECRET"));

        // Outcomes (Debug) carry no token.
        for outcome in [
            ClaimOutcome::Succeeded,
            ClaimOutcome::NeedsVerification,
            ClaimOutcome::InvalidToken,
            ClaimOutcome::Failed {
                reason: describe_error(403, None),
            },
        ] {
            assert!(!format!("{outcome:?}").contains("SUPER-SECRET"));
        }
    }
}
