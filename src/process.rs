use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::NaiveDateTime;
use serde::Deserialize;
use serde_json::Value;

use crate::Settings;
use crate::openrouter::OpenRouter;
use crate::prompt;

/// The format of an unfinished meeting folder's path under the meetings
/// folder.
const FOLDER_FORMAT: &str = "%Y/%m/%d/%H%M";

/// The upload limit for one transcription request.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

const MAX_TITLE_CHARS: usize = 60;

/// The folder for a meeting starting at `started`.
pub fn meeting_dir(meetings: &Path, started: NaiveDateTime) -> PathBuf {
    meetings.join(started.format(FOLDER_FORMAT).to_string())
}

/// The meeting subfolder holding everything besides `transcript.md` and
/// `summary.md`: the recorded tracks and their transcriptions.
pub const ASSETS: &str = "assets";

/// Whether a folder is a bare `YYYY/MM/DD/HHMM`, with no title yet.
pub fn is_unfinished(dir: &Path) -> bool {
    started(dir).is_some()
}

/// When the unfinished meeting in `dir` started, from its last four path
/// components.
fn started(dir: &Path) -> Option<NaiveDateTime> {
    let date = dir.strip_prefix(dir.ancestors().nth(4)?).ok()?;
    NaiveDateTime::parse_from_str(date.to_str()?, FOLDER_FORMAT).ok()
}

/// Transcribes, summarizes and titles the unfinished meeting in
/// `YYYY/MM/DD/HHMM`, then renames it to `HHMM <title>` and returns the new
/// path.
///
/// Each step whose output file already exists is skipped, so a retry after a
/// failure doesn't repeat the requests that succeeded.
pub fn process(dir: &Path, settings: &Settings) -> Result<PathBuf> {
    let started = started(dir)
        .with_context(|| format!("{} is not an unfinished meeting folder", dir.display()))?;
    let client = OpenRouter::new(settings)?;
    let assets = dir.join(ASSETS);

    transcribe(&client, &assets, "mic", None)?;
    transcribe(
        &client,
        &assets,
        "desktop",
        Some(&settings.transcribe_options),
    )?;

    let transcript_path = dir.join("transcript.md");
    if !transcript_path.exists() {
        let mic = read_transcription(&assets.join("mic.json"))?;
        let desktop = read_transcription(&assets.join("desktop.json"))?;
        fs::write(&transcript_path, transcript(&mic, &desktop, &settings.name))?;
    }

    let summary_path = dir.join("summary.md");
    if !summary_path.exists() {
        let user = prompt::summary_user(
            &started.format("%Y-%m-%d").to_string(),
            &started.format("%H:%M").to_string(),
            &timestamp(duration(&assets.join("mic.ogg"))?),
            &fs::read_to_string(&transcript_path)?,
        );
        let summary = client
            .chat(
                &settings.summary_model,
                &prompt::summary_system(&settings.name),
                &user,
            )
            .context("summarizing")?;
        fs::write(&summary_path, format!("{}\n", summary.trim()))?;
    }

    let summary = fs::read_to_string(&summary_path)?;
    let reply = client
        .chat(&settings.title_model, prompt::TITLE_SYSTEM, &summary)
        .context("titling")?;
    let title = clean_title(&reply);
    if title.is_empty() {
        bail!("the title model returned an empty title: {reply:?}");
    }

    let finished = dir.with_file_name(format!("{} {title}", started.format("%H%M")));
    fs::rename(dir, &finished)
        .with_context(|| format!("renaming {} to {}", dir.display(), finished.display()))?;
    Ok(finished)
}

/// Writes `<track>.json` from `<track>.ogg` in `assets`, unless it already
/// exists.
fn transcribe(
    client: &OpenRouter,
    assets: &Path,
    track: &str,
    provider_options: Option<&Value>,
) -> Result<()> {
    let out = assets.join(format!("{track}.json"));
    if out.exists() {
        return Ok(());
    }

    // MAI-Transcribe 2 rejects Opus, so upload an MP3 copy. MP3 at Opus's
    // 48 kHz can't go below 32 kbps, so resample to reach 24 kbps.
    let mp3 = tempfile::Builder::new()
        .prefix("fipple-")
        .suffix(".mp3")
        .tempfile()?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(assets.join(format!("{track}.ogg")))
        .args([
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "24k",
            "-f",
            "mp3",
        ])
        .arg(mp3.path()))?;
    let audio = fs::read(mp3.path())?;
    if audio.len() > MAX_UPLOAD_BYTES {
        bail!("{track}.ogg: track too long for one request");
    }

    let body = client
        .transcribe(&audio, provider_options)
        .with_context(|| format!("transcribing {track}.ogg"))?;
    serde_json::from_str::<Transcription>(&body)
        .with_context(|| format!("parsing the {track}.ogg transcription"))?;
    fs::write(out, body)?;
    Ok(())
}

#[derive(Deserialize)]
struct Transcription {
    segments: Vec<Segment>,
}

#[derive(Deserialize)]
struct Segment {
    start: f64,
    text: String,
    /// The provider's 0-based speaker index, from diarization.
    speaker: Option<u32>,
}

fn read_transcription(path: &Path) -> Result<Transcription> {
    serde_json::from_str(&fs::read_to_string(path)?)
        .with_context(|| format!("parsing {}", path.display()))
}

/// Interleaves both tracks by start time, mic first on ties, one line per
/// segment.
fn transcript(mic: &Transcription, desktop: &Transcription, name: &str) -> String {
    let mic_lines = mic.segments.iter().map(|s| (s, name.to_string()));
    let desktop_lines = desktop.segments.iter().map(|s| {
        let label = match s.speaker {
            Some(index) => format!("Speaker {}", index + 1),
            None => "Them".to_string(),
        };
        (s, label)
    });
    // The sort is stable, so mic segments stay ahead of desktop ones on ties.
    let mut lines: Vec<_> = mic_lines
        .chain(desktop_lines)
        .filter(|(s, _)| !s.text.trim().is_empty())
        .collect();
    lines.sort_by(|(a, _), (b, _)| a.start.total_cmp(&b.start));
    lines
        .iter()
        .map(|(s, label)| format!("[{}] **{label}:** {}\n", timestamp(s.start), s.text.trim()))
        .collect()
}

/// Formats seconds as `HH:MM:SS`.
fn timestamp(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        total % 3600 / 60,
        total % 60
    )
}

/// The length of an audio file in seconds.
fn duration(path: &Path) -> Result<f64> {
    let out = run(Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path))?;
    out.trim()
        .parse()
        .with_context(|| format!("ffprobe gave {out:?} as the duration of {}", path.display()))
}

/// Turns the title model's reply into something safe for a folder name.
fn clean_title(reply: &str) -> String {
    let kept: String = reply
        .chars()
        .filter(|&c| c != '/' && (c.is_whitespace() || !c.is_control()))
        .collect();
    let unquoted = kept
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'));
    let collapsed = unquoted.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Runs a command and returns its stdout, failing with its stderr.
fn run(command: &mut Command) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
