use std::env;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fipple::Settings;
use serde::Deserialize;

/// The daemon's configuration, from `config.toml`.
pub struct Config {
    pub settings: Settings,
    /// Where meeting folders go.
    pub meetings: PathBuf,
    /// Run in each meeting folder after it's processed, with the folder as
    /// its argument.
    pub on_finished: Option<PathBuf>,
    pub reminders: Reminders,
    pub auto_record: Option<AutoRecord>,
}

/// Configuration for polling a script to start and stop recordings.
pub struct AutoRecord {
    /// Executable polled on `interval`. Exit 0 means a meeting is active,
    /// exit 1 means no meeting, and anything else is an error.
    pub script: PathBuf,
    /// How often to run `script`.
    pub interval: Duration,
    /// How long `script` must report no meeting before stopping a recording.
    pub stop_delay: Duration,
    /// Whether transitioning from inactive to active starts a recording.
    pub auto_start: bool,
}

/// The reminders that pop up while recording.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reminders {
    pub enabled: bool,
    /// How long a recording runs before the first reminder.
    pub delay_minutes: u64,
    /// How often a reminder pops up after that.
    pub interval_minutes: NonZeroU64,
}

impl Default for Reminders {
    fn default() -> Self {
        Reminders {
            enabled: true,
            delay_minutes: 10,
            interval_minutes: NonZeroU64::new(10).unwrap(),
        }
    }
}

/// `config.toml` as written. The README's "Configuration" section describes
/// each key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    api_key_command: String,
    #[serde(default = "default_dir")]
    dir: PathBuf,
    name: Option<String>,
    transcribe_model: Option<String>,
    transcribe_options: Option<serde_json::Value>,
    title_model: Option<String>,
    summary_model: Option<String>,
    #[serde(default)]
    echo_cancel: bool,
    on_finished: Option<PathBuf>,
    summary_instructions: Option<PathBuf>,
    title_instructions: Option<PathBuf>,
    #[serde(default)]
    reminders: Reminders,
    auto_record: Option<AutoRecordFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoRecordFile {
    script: PathBuf,
    #[serde(default = "default_interval_seconds")]
    interval_seconds: NonZeroU64,
    #[serde(default = "default_stop_delay_seconds")]
    stop_delay_seconds: u64,
    #[serde(default = "default_true")]
    auto_start: bool,
}

fn default_dir() -> PathBuf {
    PathBuf::from("~/Meetings")
}

fn default_interval_seconds() -> NonZeroU64 {
    NonZeroU64::new(5).unwrap()
}

fn default_stop_delay_seconds() -> u64 {
    15
}

fn default_true() -> bool {
    true
}

/// Reads `$XDG_CONFIG_HOME/fipple/config.toml` and runs its key command.
pub fn load() -> Result<Config> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| home.join(".config"), PathBuf::from);
    let path = config_home.join("fipple/config.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let file: File =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    let mut settings = Settings::new(api_key(&file.api_key_command)?);
    let overrides = [
        (file.name, &mut settings.name),
        (file.transcribe_model, &mut settings.transcribe_model),
        (file.title_model, &mut settings.title_model),
        (file.summary_model, &mut settings.summary_model),
    ];
    for (value, setting) in overrides {
        if let Some(value) = value {
            *setting = value;
        }
    }
    if let Some(options) = file.transcribe_options {
        settings.transcribe_options = options;
    }
    settings.echo_cancel = file.echo_cancel;
    settings.summary_instructions = file.summary_instructions.map(|p| expand(&home, p));
    settings.title_instructions = file.title_instructions.map(|p| expand(&home, p));
    Ok(Config {
        settings,
        meetings: expand(&home, file.dir),
        on_finished: file.on_finished.map(|p| expand(&home, p)),
        reminders: file.reminders,
        auto_record: file.auto_record.map(|a| AutoRecord {
            script: expand(&home, a.script),
            interval: Duration::from_secs(a.interval_seconds.get()),
            stop_delay: Duration::from_secs(a.stop_delay_seconds),
            auto_start: a.auto_start,
        }),
    })
}

/// Runs the key command with `sh` and returns what it prints, trimmed.
fn api_key(command: &str) -> Result<String> {
    let output = Command::new("sh")
        .args(["-c", command])
        .stderr(Stdio::inherit())
        .output()
        .context("running api_key_command")?;
    if !output.status.success() {
        bail!("api_key_command failed with {}", output.status);
    }
    let key = String::from_utf8(output.stdout).context("api_key_command printed invalid UTF-8")?;
    let key = key.trim();
    if key.is_empty() {
        bail!("api_key_command printed nothing");
    }
    Ok(key.to_string())
}

/// Replaces a leading `~` with the home folder.
fn expand(home: &Path, path: PathBuf) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_record_defaults() {
        let file: File = toml::from_str(
            r#"
            api_key_command = "echo key"

            [auto_record]
            script = "~/bin/in-meeting"
            "#,
        )
        .unwrap();
        let auto = file.auto_record.unwrap();
        assert_eq!(
            expand(Path::new("/home/alice"), auto.script),
            PathBuf::from("/home/alice/bin/in-meeting")
        );
        assert_eq!(auto.interval_seconds.get(), 5);
        assert_eq!(auto.stop_delay_seconds, 15);
        assert!(auto.auto_start);
    }

    #[test]
    fn auto_record_overrides() {
        let file: File = toml::from_str(
            r#"
            api_key_command = "echo key"

            [auto_record]
            script = "/usr/local/bin/in-meeting"
            interval_seconds = 10
            stop_delay_seconds = 0
            auto_start = false
            "#,
        )
        .unwrap();
        let auto = file.auto_record.unwrap();
        assert_eq!(auto.script, PathBuf::from("/usr/local/bin/in-meeting"));
        assert_eq!(auto.interval_seconds.get(), 10);
        assert_eq!(auto.stop_delay_seconds, 0);
        assert!(!auto.auto_start);
    }

    #[test]
    fn auto_record_omitted() {
        let file: File = toml::from_str(r#"api_key_command = "echo key""#).unwrap();
        assert!(file.auto_record.is_none());
    }

    #[test]
    fn auto_record_rejects_invalid_config() {
        let missing_script = toml::from_str::<File>(
            r#"
            api_key_command = "echo key"

            [auto_record]
            auto_start = false
            "#,
        );
        assert!(missing_script.is_err());

        let zero_interval = toml::from_str::<File>(
            r#"
            api_key_command = "echo key"

            [auto_record]
            script = "/bin/true"
            interval_seconds = 0
            "#,
        );
        assert!(zero_interval.is_err());

        let unknown_field = toml::from_str::<File>(
            r#"
            api_key_command = "echo key"

            [auto_record]
            script = "/bin/true"
            bogus = true
            "#,
        );
        assert!(unknown_field.is_err());
    }
}
