mod echo;
mod openrouter;
mod process;
mod prompt;

use std::path::PathBuf;

pub use process::{ASSETS, is_unfinished, meeting_dir, process};

/// Everything processing needs besides the meeting folder.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The OpenRouter API root, such as `https://openrouter.ai/api/v1`.
    pub base_url: String,
    pub api_key: String,
    /// The label for the mic track in the transcript and summary.
    pub name: String,
    pub transcribe_model: String,
    /// Sent as `provider.options` with both tracks.
    pub transcribe_options: serde_json::Value,
    pub title_model: String,
    pub summary_model: String,
    /// Whether to cancel the desktop track's echo out of the mic track before
    /// transcribing it.
    pub echo_cancel: bool,
    /// A file whose text goes after the summary model's system prompt.
    pub summary_instructions: Option<PathBuf>,
    /// A file whose text goes after the title model's system prompt.
    pub title_instructions: Option<PathBuf>,
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
            echo_cancel: false,
            summary_instructions: None,
            title_instructions: None,
        }
    }
}
