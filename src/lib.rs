mod openrouter;
mod process;
mod prompt;

pub use process::{FOLDER_FORMAT, is_unfinished, process};

/// Everything processing needs besides the meeting folder.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The OpenRouter API root, such as `https://openrouter.ai/api/v1`.
    pub base_url: String,
    pub api_key: String,
    /// The label for the mic track in the transcript and summary.
    pub name: String,
    pub transcribe_model: String,
    /// Sent as `provider.options` with the desktop track.
    pub transcribe_options: serde_json::Value,
    pub title_model: String,
    pub summary_model: String,
}

impl Settings {
    /// The defaults from the spec's "Configuration" table.
    pub fn new(api_key: impl Into<String>) -> Self {
        Settings {
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key: api_key.into(),
            name: "Me".into(),
            transcribe_model: "microsoft/mai-transcribe-2".into(),
            transcribe_options: serde_json::json!({"azure": {"diarization": {"enabled": true}}}),
            title_model: "anthropic/claude-haiku-5.5".into(),
            summary_model: "openai/gpt-6.1-sol".into(),
        }
    }
}
