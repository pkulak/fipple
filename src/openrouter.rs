use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Settings;

const MAX_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

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
            "timestamp_granularities": ["segment", "word"],
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
        self.post_with_sleep(path, body, thread::sleep)
    }

    fn post_with_sleep(
        &self,
        path: &str,
        body: &Value,
        mut sleep: impl FnMut(Duration),
    ) -> Result<String> {
        let url = format!("{}/{path}", self.settings.base_url.trim_end_matches('/'));
        for attempt in 0..MAX_ATTEMPTS {
            let response = self
                .http
                .post(&url)
                .bearer_auth(&self.settings.api_key)
                .json(body)
                .send()
                .with_context(|| format!("POST {url}"))?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_seconds);
            let text = response.text().with_context(|| format!("POST {url}"))?;
            if status.is_success() {
                return Ok(text);
            }
            if attempt + 1 < MAX_ATTEMPTS && is_retryable(status) {
                let backoff = INITIAL_BACKOFF.saturating_mul(1 << attempt);
                let delay = retry_after
                    .or_else(|| retry_after_body(&text))
                    .map_or(backoff, |hint| hint.max(backoff))
                    .min(MAX_RETRY_DELAY);
                eprintln!(
                    "POST {url} returned {status} (attempt {}/{MAX_ATTEMPTS}); retrying in {delay:?}",
                    attempt + 1,
                );
                sleep(delay);
                continue;
            }
            bail!("POST {url} returned {status}: {text}");
        }
        unreachable!()
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

/// Reads an upstream retry delay from OpenRouter's JSON error metadata.
fn retry_after_body(body: &str) -> Option<Duration> {
    let json: Value = serde_json::from_str(body).ok()?;
    let metadata = json.get("error")?.get("metadata")?;
    if let Some(seconds) = metadata
        .get("retry_after_seconds")
        .and_then(parse_json_seconds)
    {
        return Some(seconds);
    }
    metadata
        .get("headers")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| parse_json_seconds(value))
}

fn parse_json_seconds(value: &Value) -> Option<Duration> {
    match value {
        Value::Number(n) => seconds_to_duration(n.as_f64()?),
        Value::String(s) => parse_seconds(s),
        _ => None,
    }
}

fn parse_seconds(s: &str) -> Option<Duration> {
    seconds_to_duration(s.trim().parse().ok()?)
}

fn seconds_to_duration(seconds: f64) -> Option<Duration> {
    if seconds.is_finite() && seconds >= 0.0 {
        Some(Duration::from_secs_f64(
            seconds.min(MAX_RETRY_DELAY.as_secs_f64()),
        ))
    } else {
        None
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// A one-off local HTTP server serving `(status, Retry-After header, body)` responses.
    fn serve(responses: Vec<(u16, Option<&'static str>, &'static str)>) -> Settings {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/v1", server.server_addr().to_ip().unwrap());
        let queue = Mutex::new(VecDeque::from(responses));
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let (status, retry_after, body) = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected extra request");
                let mut response = tiny_http::Response::from_string(body).with_status_code(status);
                if let Some(value) = retry_after {
                    response = response.with_header(
                        format!("Retry-After: {value}")
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                }
                request.respond(response).unwrap();
            }
        });
        Settings {
            base_url: url,
            ..Settings::new("test-key")
        }
    }

    #[test]
    fn retries_429_using_openrouter_metadata() {
        let rate_limit = r#"{"error":{"message":"Provider returned 429","code":429,"metadata":{"retry_after_seconds":3,"retry_after_seconds_raw":3,"headers":{"Retry-After":"3"}}}}"#;
        let settings = serve(vec![(429, None, rate_limit), (200, None, r#"{"ok":true}"#)]);
        let client = OpenRouter::new(&settings).unwrap();
        let mut sleeps = Vec::new();

        let reply = client
            .post_with_sleep("audio/transcriptions", &json!({}), |d| sleeps.push(d))
            .unwrap();

        assert_eq!(reply, r#"{"ok":true}"#);
        assert_eq!(sleeps, vec![Duration::from_secs(3)]);
    }

    #[test]
    fn retries_429_using_retry_after_header_and_metadata_header_fallback() {
        let metadata_header_only =
            r#"{"error":{"code":429,"metadata":{"headers":{"Retry-After":"7"}}}}"#;
        let settings = serve(vec![
            (429, Some("5"), "slow down"),
            (429, None, metadata_header_only),
            (200, None, "done"),
        ]);
        let client = OpenRouter::new(&settings).unwrap();
        let mut sleeps = Vec::new();

        let reply = client
            .post_with_sleep("chat/completions", &json!({}), |d| sleeps.push(d))
            .unwrap();

        assert_eq!(reply, "done");
        assert_eq!(sleeps, vec![Duration::from_secs(5), Duration::from_secs(7)]);
    }

    #[test]
    fn caps_retry_after_delay() {
        let settings = serve(vec![(429, Some("3600"), "slow down"), (200, None, "done")]);
        let client = OpenRouter::new(&settings).unwrap();
        let mut sleeps = Vec::new();

        client
            .post_with_sleep("chat/completions", &json!({}), |d| sleeps.push(d))
            .unwrap();

        assert_eq!(sleeps, vec![MAX_RETRY_DELAY]);
    }

    #[test]
    fn backs_off_on_5xx_and_fails_after_max_attempts() {
        let settings = serve(vec![
            (500, None, "err 1"),
            (502, None, "err 2"),
            (503, None, "err 3"),
            (504, None, "err 4"),
            (500, None, "final error"),
        ]);
        let client = OpenRouter::new(&settings).unwrap();
        let mut sleeps = Vec::new();

        let error = client
            .post_with_sleep("chat/completions", &json!({}), |d| sleeps.push(d))
            .unwrap_err();

        assert!(error.to_string().contains("final error"), "{error:#}");
        assert_eq!(
            sleeps,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ]
        );
    }

    #[test]
    fn does_not_retry_client_errors() {
        let settings = serve(vec![(400, None, "bad request")]);
        let client = OpenRouter::new(&settings).unwrap();
        let mut sleeps = Vec::new();

        let error = client
            .post_with_sleep("chat/completions", &json!({}), |d| sleeps.push(d))
            .unwrap_err();

        assert!(error.to_string().contains("400"), "{error:#}");
        assert!(sleeps.is_empty());
    }
}
