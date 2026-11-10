//! Chat-completions client for the model provider (Groq's OpenAI-compatible API).
//!
//! This module knows nothing about Discord or any game. A feature describes
//! what it wants as a [`Chat`] and gets the model's text back; credentials,
//! the HTTP round trip, status handling and the concurrency limit live here,
//! once. Adding a feature that talks to the model means building a `Chat`,
//! not changing this file:
//!
//! ```ignore
//! let answer = ai.chat(Chat::new("Answer briefly.", question)).await?;
//! ```
//!
//! Switching provider or API shape is likewise a change to this file only.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::config::AiConfig;

const URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const TIMEOUT: Duration = Duration::from_secs(20);
/// Requests in flight at once, across every feature. One keeps free-tier usage
/// predictable: later requests wait their turn instead of racing into a 429.
const MAX_IN_FLIGHT: usize = 1;

/// One message exchange with the model. Build it with [`Chat::new`] and the
/// optional setters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Chat {
    system: String,
    user: String,
    temperature: f64,
    max_tokens: u32,
    json_schema: Option<(String, Value)>,
}

impl Chat {
    /// A request with a system instruction and a user message, using
    /// conversational defaults (temperature 0.7, up to 600 output tokens).
    pub fn new(system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            user: user.into(),
            temperature: 0.7,
            max_tokens: 600,
            json_schema: None,
        }
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Require the reply to be JSON matching `schema` (strict mode). The
    /// returned text is then that JSON, ready to parse.
    pub fn json_schema(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.json_schema = Some((name.into(), schema));
        self
    }
}

pub(crate) struct AiClient {
    http: Client,
    key: String,
    model: String,
    limit: Semaphore,
}

impl AiClient {
    pub fn new(config: AiConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("failed to configure Groq HTTP client")?;
        Ok(Self { http, key: config.key, model: config.model, limit: Semaphore::new(MAX_IN_FLIGHT) })
    }

    /// Send `chat` and return the text of the model's first reply.
    ///
    /// Waits for a free slot first (see [`MAX_IN_FLIGHT`]); the request itself
    /// is bounded by a 20 second timeout. Callers in a Discord interaction
    /// should acknowledge it before calling this.
    pub async fn chat(&self, chat: Chat) -> Result<String> {
        let _slot = self.limit.acquire().await.context("the AI request limiter was closed")?;
        let response = self
            .http
            .post(URL)
            .bearer_auth(&self.key)
            .json(&request_body(&self.model, &chat))
            .send()
            .await
            .context("failed to reach Groq")?;
        check_status(response.status())?;
        let body: Completion = response.json().await.context("failed to decode Groq response")?;
        first_message(body)
    }
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

fn request_body(model: &str, chat: &Chat) -> Value {
    let mut body = json!({
        "model": model,
        "temperature": chat.temperature,
        "max_completion_tokens": chat.max_tokens,
        "messages": [
            {"role": "system", "content": chat.system},
            {"role": "user", "content": chat.user},
        ],
    });
    if let Some((name, schema)) = &chat.json_schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": name, "strict": true, "schema": schema}
        });
    }
    body
}

fn check_status(status: StatusCode) -> Result<()> {
    match status {
        StatusCode::OK => Ok(()),
        StatusCode::TOO_MANY_REQUESTS => bail!("Groq request quota or rate limit reached (HTTP 429)"),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            bail!("Groq rejected the API credentials or model permissions (HTTP {status})")
        }
        status => bail!("Groq returned HTTP {status}"),
    }
}

fn first_message(body: Completion) -> Result<String> {
    body.choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .ok_or_else(|| anyhow!("Groq returned no message content"))
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use serde_json::json;

    use super::{Chat, Completion, check_status, first_message, request_body};

    #[test]
    fn a_plain_chat_has_two_messages_and_no_response_format() {
        let body = request_body("some-model", &Chat::new("be brief", "why is the sky blue?"));
        assert_eq!(
            body,
            json!({
                "model": "some-model",
                "temperature": 0.7,
                "max_completion_tokens": 600,
                "messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": "why is the sky blue?"},
                ],
            })
        );
    }

    #[test]
    fn settings_and_schema_are_sent_as_given() {
        let schema = json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false});
        let chat = Chat::new("s", "u").temperature(0.9).max_tokens(2000).json_schema("batch", schema.clone());
        let body = request_body("m", &chat);
        assert_eq!(body["temperature"], json!(0.9));
        assert_eq!(body["max_completion_tokens"], json!(2000));
        assert_eq!(
            body["response_format"],
            json!({"type": "json_schema", "json_schema": {"name": "batch", "strict": true, "schema": schema}})
        );
    }

    #[test]
    fn every_status_has_a_clear_outcome() {
        assert!(check_status(StatusCode::OK).is_ok());
        let limited = check_status(StatusCode::TOO_MANY_REQUESTS).unwrap_err().to_string();
        assert!(limited.contains("429"), "{limited}");
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let error = check_status(status).unwrap_err().to_string();
            assert!(error.contains("credentials") && error.contains(&status.as_u16().to_string()), "{error}");
        }
        let other = check_status(StatusCode::INTERNAL_SERVER_ERROR).unwrap_err().to_string();
        assert!(other.contains("500"), "{other}");
    }

    fn completion(json: &str) -> Completion {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_first_reply_is_returned() {
        let body = completion(r#"{"choices":[{"message":{"content":"hello"}},{"message":{"content":"ignored"}}]}"#);
        assert_eq!(first_message(body).unwrap(), "hello");
    }

    #[test]
    fn a_reply_without_content_is_an_error() {
        assert!(first_message(completion(r#"{"choices":[]}"#)).is_err());
        assert!(first_message(completion(r#"{"choices":[{"message":{"content":null}}]}"#)).is_err());
        assert!(first_message(completion(r#"{"choices":[{"message":{}}]}"#)).is_err());
    }
}
