//! Telegram Bot API notifier (`sendMessage`, HTML parse mode).

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use super::{Kind, Message, NotifyError, build_client, post_json};

pub const DEFAULT_TELEGRAM_BASE_URL: &str = "https://api.telegram.org";
const NAME: &str = "telegram";

#[derive(Debug)]
pub struct TelegramNotifier {
    client: reqwest::Client,
    base_url: String,
    token: SecretString,
    chat_id: String,
}

/// Escapes the characters Telegram's HTML parse mode treats specially.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

impl TelegramNotifier {
    /// `base_url` is injectable (tests use a mock server).
    pub fn new(
        base_url: &str,
        token: SecretString,
        chat_id: String,
        timeout: Duration,
    ) -> Result<Self, NotifyError> {
        Ok(Self {
            client: build_client(timeout)?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            chat_id,
        })
    }

    fn body(&self, message: &Message) -> Value {
        let icon = match message.kind {
            Kind::Free => "\u{1F7E2}",
            Kind::Success => "\u{2705}",
            Kind::Taken => "\u{26AA}",
            Kind::Warning => "\u{26A0}\u{FE0F}",
            Kind::Urgent => "\u{1F6A8}",
            Kind::Test => "\u{1F514}",
        };
        let mut text = format!("{icon} <b>{}</b>\n", escape_html(&message.title));
        text.push_str(&format!(
            "Code: <code>{}</code>\n",
            escape_html(&message.code)
        ));
        for (name, value) in &message.fields {
            text.push_str(&format!("{}: {}\n", escape_html(name), escape_html(value)));
        }
        text.push_str(&format!("Time: {}", escape_html(&message.time_text())));
        json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        })
    }

    pub async fn send(&self, message: &Message) -> Result<(), NotifyError> {
        // The token is part of the URL path; it only lives in this temporary string.
        let url = format!(
            "{}/bot{}/sendMessage",
            self.base_url,
            self.token.expose_secret()
        );
        post_json(&self.client, NAME, &url, &self.body(message)).await
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

    const TOKEN: &str = "123456789:AAFakeTokenValueForTestsOnly";

    fn notifier(base: &str) -> TelegramNotifier {
        TelegramNotifier::new(
            base,
            SecretString::from(TOKEN.to_owned()),
            "555".to_owned(),
            Duration::from_secs(2),
        )
        .unwrap()
    }

    fn message(owner: &str) -> Message {
        Message::from_record(&EventRecord {
            event: Event::BecameFree {
                code: "abc".into(),
                previous_owner: Some(owner.into()),
            },
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        })
    }

    #[test]
    fn escapes_html() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[tokio::test]
    async fn posts_send_message_with_escaped_html() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/bot{TOKEN}/sendMessage")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        notifier(&server.uri())
            .send(&message("<b>Evil</b> & Co"))
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["chat_id"], "555");
        assert_eq!(body["parse_mode"], "HTML");
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<b>discord.gg/abc is FREE</b>"), "{text}");
        assert!(text.contains("<code>abc</code>"));
        assert!(text.contains("Previous owner: &lt;b&gt;Evil&lt;/b&gt; &amp; Co"));
        assert!(text.contains("2023-11-14 22:13:20 UTC"));
    }

    #[tokio::test]
    async fn retries_429_using_body_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(
                json!({"ok": false, "parameters": {"retry_after": 0}, "retry_after": 0}),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        notifier(&server.uri()).send(&message("x")).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn error_strings_never_contain_the_token() {
        let n = notifier("http://127.0.0.1:1");
        let err = n.send(&message("x")).await.unwrap_err();
        assert!(!err.to_string().contains("AAFakeToken"), "{err}");
        assert!(!format!("{err:?}").contains("AAFakeToken"));
        assert!(!format!("{n:?}").contains("AAFakeToken"));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = notifier(&server.uri())
            .send(&message("x"))
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::Status { status: 401, .. }));
        assert!(!err.to_string().contains("AAFakeToken"));
    }
}
