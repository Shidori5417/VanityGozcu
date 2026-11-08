//! Discord webhook notifier (embed).

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use super::{Kind, Message, NotifyError, build_client, post_json};

const NAME: &str = "discord";
const COLOR_FREE: u32 = 0x2E_CC_71;
const COLOR_TAKEN: u32 = 0x95_A5_A6;
const COLOR_URGENT: u32 = 0xED_42_45;
const COLOR_WARNING: u32 = 0xE7_4C_3C;
const COLOR_TEST: u32 = 0x34_98_DB;

#[derive(Debug)]
pub struct DiscordNotifier {
    client: reqwest::Client,
    /// Full webhook URL; contains the webhook token.
    url: SecretString,
    mention: Option<String>,
}

impl DiscordNotifier {
    /// `url` is the complete webhook URL (tests point it at a mock server).
    pub fn new(
        url: SecretString,
        mention: Option<String>,
        timeout: Duration,
    ) -> Result<Self, NotifyError> {
        Ok(Self {
            client: build_client(timeout)?,
            url,
            mention: mention.filter(|m| !m.trim().is_empty()),
        })
    }

    fn body(&self, message: &Message) -> Value {
        let color = match message.kind {
            Kind::Free | Kind::Success => COLOR_FREE,
            Kind::Taken => COLOR_TAKEN,
            Kind::Warning => COLOR_WARNING,
            Kind::Urgent => COLOR_URGENT,
            Kind::Test => COLOR_TEST,
        };
        let mut fields = vec![json!({
            "name": "Code",
            "value": format!("`{}`", message.code),
            "inline": true,
        })];
        for (name, value) in &message.fields {
            fields.push(json!({"name": name, "value": value, "inline": true}));
        }
        fields.push(json!({"name": "Time", "value": message.time_text(), "inline": false}));
        let mut body = json!({
            "embeds": [{
                "title": message.title,
                "color": color,
                "fields": fields,
                "timestamp": message.time_rfc3339(),
            }],
        });
        if let Some(mention) = &self.mention {
            body["content"] = Value::String(mention.clone());
        }
        body
    }

    pub async fn send(&self, message: &Message) -> Result<(), NotifyError> {
        post_json(
            &self.client,
            NAME,
            self.url.expose_secret(),
            &self.body(message),
        )
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::{Event, EventRecord};
    use std::time::SystemTime;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "WEBHOOKTOKENsecret1234";

    fn notifier(server: &MockServer, mention: Option<&str>) -> DiscordNotifier {
        let url = format!("{}/api/webhooks/42/{TOKEN}", server.uri());
        DiscordNotifier::new(
            SecretString::from(url),
            mention.map(str::to_owned),
            Duration::from_secs(2),
        )
        .unwrap()
    }

    fn free_message() -> Message {
        Message::from_record(&EventRecord {
            event: Event::BecameFree {
                code: "abc".into(),
                previous_owner: Some("Old Guild".into()),
            },
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        })
    }

    #[tokio::test]
    async fn posts_embed_with_code_owner_and_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/api/webhooks/42/{TOKEN}")))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        notifier(&server, None).send(&free_message()).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body.get("content").is_none());
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"], "discord.gg/abc is FREE");
        assert_eq!(embed["timestamp"], "2023-11-14T22:13:20+00:00");
        let fields = embed["fields"].as_array().unwrap();
        let find = |n: &str| fields.iter().find(|f| f["name"] == n).unwrap()["value"].clone();
        assert_eq!(find("Code"), "`abc`");
        assert_eq!(find("Previous owner"), "Old Guild");
        assert_eq!(find("Time"), "2023-11-14 22:13:20 UTC");
    }

    #[tokio::test]
    async fn mention_goes_into_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        notifier(&server, Some("<@123>"))
            .send(&free_message())
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["content"], "<@123>");
    }

    #[tokio::test]
    async fn retries_after_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        notifier(&server, None).send(&free_message()).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn gives_up_after_bounded_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;
        let err = notifier(&server, None)
            .send(&free_message())
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::RateLimited { .. }));
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert!(!err.to_string().contains(TOKEN));
    }

    #[tokio::test]
    async fn long_retry_after_fails_fast() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "3600"))
            .mount(&server)
            .await;
        let err = notifier(&server, None)
            .send(&free_message())
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::RateLimited { .. }));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn error_strings_never_contain_the_webhook_token() {
        // Connection refused: the transport error must not echo the URL.
        let n = DiscordNotifier::new(
            SecretString::from(format!("http://127.0.0.1:1/api/webhooks/42/{TOKEN}")),
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        let err = n.send(&free_message()).await.unwrap_err();
        assert!(!err.to_string().contains(TOKEN), "{err}");
        assert!(!format!("{err:?}").contains(TOKEN));
        assert!(!format!("{n:?}").contains(TOKEN));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = notifier(&server, None)
            .send(&free_message())
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::Status { status: 500, .. }));
        assert!(!err.to_string().contains(TOKEN));
    }
}
