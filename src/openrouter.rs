use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Settings;

pub struct OpenRouter<'a> {
    http: reqwest::blocking::Client,
    settings: &'a Settings,
}

impl<'a> OpenRouter<'a> {
    pub fn new(settings: &'a Settings) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            // A long summary can take minutes.
            .timeout(Duration::from_secs(15 * 60))
            .build()?;
        Ok(OpenRouter { http, settings })
    }

    /// Returns the raw `verbose_json` response body.
    pub fn transcribe(&self, mp3: &[u8], provider_options: &Value) -> Result<String> {
        let body = json!({
            "model": self.settings.transcribe_model,
            "input_audio": {"data": STANDARD.encode(mp3), "format": "mp3"},
            "response_format": "verbose_json",
            "timestamp_granularities": ["segment"],
            "provider": {"options": provider_options},
        });
        self.post("audio/transcriptions", &body)
    }

    pub fn chat(&self, model: &str, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let response: ChatResponse = serde_json::from_str(&self.post("chat/completions", &body)?)
            .context("parsing the chat completion")?;
        response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .context("the chat completion has no content")
    }

    fn post(&self, path: &str, body: &Value) -> Result<String> {
        let url = format!("{}/{path}", self.settings.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.settings.api_key)
            .json(body)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let text = response.text().with_context(|| format!("POST {url}"))?;
        if !status.is_success() {
            bail!("POST {url} returned {status}: {text}");
        }
        Ok(text)
    }
}

#[derive(Deserialize)]
struct ChatResponse {
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
