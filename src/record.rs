use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;

/// A running recording of the default microphone and output.
pub struct Recording {
    pub dir: PathBuf,
    pub started: Instant,
    pipelines: Vec<Pipeline>,
}

/// `pw-record` piping raw audio into an `ffmpeg` that writes Opus.
struct Pipeline {
    recorder: Child,
    encoder: Child,
}

impl Recording {
    /// Creates `<meetings>/YYYY/MM/DD/HHMM` and starts recording into its
    /// assets folder.
    pub fn start(meetings: &Path) -> Result<Recording> {
        let dir = fipple::meeting_dir(meetings, Local::now().naive_local());
        let day = dir.parent().unwrap();
        fs::create_dir_all(day).with_context(|| format!("creating {}", day.display()))?;
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let assets = dir.join(fipple::ASSETS);
        fs::create_dir(&assets)?;
        let mut recording = Recording {
            dir,
            started: Instant::now(),
            pipelines: Vec::new(),
        };
        // No target is set, so PipeWire follows the default devices.
        for (file, properties) in [
            ("mic.ogg", None),
            ("desktop.ogg", Some("{ stream.capture.sink = true }")),
        ] {
            match Pipeline::start(&assets.join(file), properties) {
                Ok(pipeline) => recording.pipelines.push(pipeline),
                Err(error) => {
                    // Don't leave a recorder running or a folder that would
                    // block another start this minute.
                    let dir = recording.dir.clone();
                    recording.stop();
                    discard(&dir)?;
                    return Err(error);
                }
            }
        }
        Ok(recording)
    }

    /// Stops both recorders and waits for the files to be written. Returns
    /// how long the recording ran.
    pub fn stop(self) -> Duration {
        for pipeline in &self.pipelines {
            // SAFETY: kill has no memory-safety preconditions. The recorder
            // hasn't been waited on, so its PID can't have been reused.
            unsafe { libc::kill(pipeline.recorder.id() as libc::pid_t, libc::SIGINT) };
        }
        for mut pipeline in self.pipelines {
            // pw-record exits 1 after SIGINT, so only ffmpeg's status means
            // anything.
            let _ = pipeline.recorder.wait();
            match pipeline.encoder.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => eprintln!("ffmpeg exited with {status}"),
                Err(error) => eprintln!("waiting for ffmpeg: {error}"),
            }
        }
        self.started.elapsed()
    }
}

/// Deletes a meeting folder, and its day, month and year folders if that
/// leaves them empty.
pub fn discard(dir: &Path) -> Result<()> {
    fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    for parent in dir.ancestors().skip(1).take(3) {
        // Fails once a folder still holds other meetings.
        if fs::remove_dir(parent).is_err() {
            break;
        }
    }
    Ok(())
}

impl Pipeline {
    fn start(out: &Path, properties: Option<&str>) -> Result<Pipeline> {
        let mut recorder = Command::new("pw-record");
        recorder.args([
            "--raw",
            "--rate",
            "48000",
            "--channels",
            "1",
            "--format",
            "s16",
        ]);
        if let Some(properties) = properties {
            recorder.args(["--properties", properties]);
        }
        let mut recorder = recorder
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .context("running pw-record")?;
        let audio = recorder.stdout.take().unwrap();
        // If this fails, pw-record dies writing to the closed pipe.
        let encoder = Command::new("ffmpeg")
            .args([
                "-v", "error", "-f", "s16le", "-ar", "48000", "-ac", "1", "-i", "pipe:0", "-c:a",
                "libopus", "-b:a", "32k",
            ])
            .arg(out)
            .stdin(audio)
            .spawn()
            .context("running ffmpeg")?;
        Ok(Pipeline { recorder, encoder })
    }
}
