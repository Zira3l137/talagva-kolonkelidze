//! Minimal Groq chat-completions client. No Discord dependencies or global state.

use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use reqwest::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::Category;
use super::Kind;
use crate::config::GroqConfig;

const URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const BATCH_SIZE: usize = 8;

pub(super) struct Groq {
    http: Client,
    key: String,
    model: String,
    language: String,
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

#[derive(Deserialize)]
struct Batch {
    prompts: Vec<String>,
}

impl Groq {
    pub fn new(config: GroqConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("failed to configure Groq HTTP client")?;
        Ok(Self { http, key: config.key, model: config.model, language: config.language })
    }

    pub async fn generate(
        &self,
        category: Category,
        kind: Kind,
        recent: &[String],
    ) -> Result<Vec<String>> {
        let language =
            if self.language == "ka" { "Georgian (ქართული)" } else { "English" };
        let category_instruction = match category {
            Category::Casual => {
                "Playful chaos, ridiculous opinions, social mishaps, and low-stakes embarrassment."
            }
            Category::Personal => {
                "Revealing stories, contradictions, questionable taste, and harmless confessions; curious, not therapeutic."
            }
            Category::Romantic => {
                "Crushes, flirting, romantic tension, dating mishaps, and cheesy charm; cheeky and non-explicit, without pressuring anyone to name or involve another player."
            }
        };
        let kind_instruction = match kind {
            Kind::Truth => "Write a single direct question for each item.",
            Kind::Dare => "Write an actionable but safe text-or-voice challenge for each item.",
        };
        let instructions = format!(
            "Generate {BATCH_SIZE} distinct {kind} prompts in {language} for a public Discord Truth or Dare game. \
            Category: {category}. {category_instruction} {kind_instruction} \
            Aim for the energy of a witty, mischievous friend: bold, funny, surprising, and occasionally revealing. \
            Harmless embarrassment, mild flirting, playful teasing, petty opinions, and awkward confessions are welcome. \
            Truths should invite a specific story, an unexpected admission, or a genuinely difficult choice. \
            Dares should create an entertaining moment through text or voice: a performance, improvisation, \
            absurd argument, or creative challenge. Make them immediately doable with no purchases or preparation. \
            Avoid generic icebreakers, repetitive 'favorite X' questions, therapy-speak, motivational exercises, \
            and recycled party-game clichés. Give each prompt one memorable twist without making it convoluted. \
            Vary the topics and formats; don't produce several versions of the same idea. \
            Use natural, conversational {language}, including idiomatic humor rather than literal translations. \
            Each prompt must be self-contained, one line, and 12–250 characters. \
            Keep it appropriate for a mixed-age group: non-explicit, physically safe, and voluntary. \
            Don't demand identifying information, expose someone else's secrets, or pressure anyone into \
            unwanted contact, cruelty, or participation. No mentions, URLs, or account changes. \
            Don't append disclaimers or reminders to individual prompts. Return only the requested JSON.",
            category = category.label(),
            kind = kind.label(),
        );
        // Do not send any messages or member data. Only prompts already served by this bot.
        let request = json!({
            "model": self.model,
            "temperature": 0.9,
            "max_completion_tokens": 2000,
            "messages": [
                {"role": "system", "content": instructions},
                {"role": "user", "content": format!(
                    "Avoid rewriting or repeating these previous prompts: {}",
                    serde_json::to_string(recent).context("failed to serialize recent prompts")?
                )},
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "truth_or_dare_batch",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "prompts": {"type": "array", "items": {"type": "string"}}
                        },
                        "required": ["prompts"],
                        "additionalProperties": false
                    }
                }
            }
        });

        let response = self
            .http
            .post(URL)
            .bearer_auth(&self.key)
            .json(&request)
            .send()
            .await
            .context("failed to reach Groq")?;

        match response.status() {
            StatusCode::OK => {}
            StatusCode::TOO_MANY_REQUESTS => {
                bail!("Groq request quota or rate limit reached (HTTP 429)")
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                bail!(
                    "Groq rejected the API credentials or model permissions (HTTP {})",
                    response.status()
                );
            }
            status => bail!("Groq returned HTTP {status}"),
        }

        let body: Completion = response.json().await.context("failed to decode Groq response")?;
        let content = body
            .choices
            .first()
            .and_then(|choice| choice.message.content.as_deref())
            .ok_or_else(|| anyhow!("Groq returned no message content"))?;
        parse_batch(content)
    }
}

fn parse_batch(content: &str) -> Result<Vec<String>> {
    let response: Batch = serde_json::from_str(content)
        .context("Groq output did not match the prompt batch format")?;
    let mut prompts = Vec::with_capacity(BATCH_SIZE);
    for item in response.prompts {
        let prompt = item.trim();
        if !is_usable(prompt) {
            continue;
        }
        if !prompts.iter().any(|existing: &String| existing.eq_ignore_ascii_case(prompt)) {
            prompts.push(prompt.to_owned());
        }
        if prompts.len() == BATCH_SIZE {
            break;
        }
    }
    if prompts.is_empty() {
        bail!("Groq returned no usable prompts");
    }
    Ok(prompts)
}

/// Format checks are not content moderation. The local fallback and optional
/// future review/approval are still important for a public server.
fn is_usable(prompt: &str) -> bool {
    let len = prompt.chars().count();
    (12..=250).contains(&len)
        && !prompt.contains('@')
        && !prompt.contains("http://")
        && !prompt.contains("https://")
        && !prompt.contains('\n')
        && !prompt.contains('\r')
        && !prompt.contains("<@")
}

#[cfg(test)]
mod tests {
    use super::is_usable;
    use super::parse_batch;

    #[test]
    fn removes_invalid_and_repeated_prompts() {
        let response = r#"{"prompts":["  Tell us your favorite fictional villain.  ","Tell us your favorite fictional villain.","@everyone please spam", "ok"]}"#;
        assert_eq!(parse_batch(response).unwrap(), ["Tell us your favorite fictional villain."]);
    }

    #[test]
    fn rejects_unusable_response() {
        assert!(parse_batch(r#"{"prompts":[]}"#).is_err());
        assert!(parse_batch("not json").is_err());
        assert!(!is_usable("Send https://example.com an email"));
    }
}
