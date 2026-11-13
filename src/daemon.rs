use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use std::{env, process};

use anyhow::{Context, Result, anyhow, bail};
use fipple::Settings;
use notify_rust::{Notification, Timeout};
use signal_hook::consts::SIGTERM;
use signal_hook::iterator::Signals;

use crate::record::{self, Recording};

/// Recordings shorter than this are taken to be accidental and discarded.
const MIN_RECORDING: Duration = Duration::from_secs(30);

/// How long notifications about something the user just did stay up.
const BRIEFLY: Timeout = Timeout::Milliseconds(5000);

/// Listens for client requests, records meetings and processes them one at a
/// time.
pub fn run() -> Result<()> {
    let settings = settings()?;
    let meetings = match env::var_os("FIPPLE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(&env::var_os("HOME").context("HOME is not set")?).join("Meetings"),
    };
    fs::create_dir_all(&meetings).with_context(|| format!("creating {}", meetings.display()))?;
    // Match the canonical paths the client sends.
    let meetings = meetings.canonicalize()?;

    let socket = crate::socket_path()?;
    match fs::remove_file(&socket) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("removing {}", socket.display()));
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;

    let daemon = Arc::new(Daemon {
        settings,
        meetings,
        state: Mutex::default(),
        queued: Condvar::new(),
    });
    {
        let mut state = daemon.state.lock().unwrap();
        for dir in unfinished(&daemon.meetings)? {
            daemon.enqueue(&mut state, dir);
        }
    }
    thread::spawn({
        let daemon = daemon.clone();
        move || daemon.work()
    });
    let mut signals = Signals::new([SIGTERM])?;
    thread::spawn({
        let daemon = daemon.clone();
        move || {
            signals.forever().next();
            daemon.shut_down();
        }
    });

    for stream in listener.incoming() {
        if let Err(error) = daemon.handle(stream?) {
            eprintln!("handling a request: {error:#}");
        }
    }
    Ok(())
}

/// The variables in the spec's "Configuration" table, besides `FIPPLE_DIR`.
fn settings() -> Result<Settings> {
    let api_key = env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .context("OPENROUTER_API_KEY is not set")?;
    let mut settings = Settings::new(api_key);
    let overrides = [
        ("FIPPLE_NAME", &mut settings.name),
        ("FIPPLE_TRANSCRIBE_MODEL", &mut settings.transcribe_model),
        ("FIPPLE_TITLE_MODEL", &mut settings.title_model),
        ("FIPPLE_SUMMARY_MODEL", &mut settings.summary_model),
    ];
    for (variable, setting) in overrides {
        if let Ok(value) = env::var(variable) {
            *setting = value;
        }
    }
    if let Ok(options) = env::var("FIPPLE_TRANSCRIBE_OPTIONS") {
        settings.transcribe_options = serde_json::from_str(&options)
            .context("FIPPLE_TRANSCRIBE_OPTIONS is not valid JSON")?;
    }
    Ok(settings)
}

/// The meeting folders in `meetings` that haven't been titled yet, oldest
/// first.
fn unfinished(meetings: &Path) -> Result<Vec<PathBuf>> {
    // Meetings sit four levels down, in YYYY/MM/DD/HHMM.
    let mut found = vec![meetings.to_path_buf()];
    for _ in 0..4 {
        found = found
            .iter()
            .map(|dir| subfolders(dir))
            .collect::<Result<Vec<_>>>()?
            .concat();
    }
    found.retain(|dir| fipple::is_unfinished(dir));
    found.sort();
    Ok(found)
}

fn subfolders(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            found.push(entry.path());
        }
    }
    Ok(found)
}

struct Daemon {
    settings: Settings,
    /// Where meeting folders go.
    meetings: PathBuf,
    state: Mutex<State>,
    /// Signalled when a folder joins the queue.
    queued: Condvar,
}

#[derive(Default)]
struct State {
    recording: Option<Recording>,
    processing: Option<PathBuf>,
    queue: VecDeque<PathBuf>,
    /// Folders whose last attempt failed, with the error.
    failed: Vec<(PathBuf, String)>,
}

impl Daemon {
    /// Answers one client request.
    fn handle(&self, stream: UnixStream) -> Result<()> {
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let request = line.trim_end_matches(['\r', '\n']);
        // Holding the lock while a recording stops makes SIGTERM wait for its
        // files to be written.
        let mut state = self.state.lock().unwrap();
        let reply = match request {
            "start" => self.start(&mut state),
            "stop" => self.stop(&mut state),
            "toggle" if state.recording.is_some() => self.stop(&mut state),
            "toggle" => self.start(&mut state),
            "status" => Ok(status(&state)),
            _ => match request.strip_prefix("process ") {
                Some(dir) if self.enqueue(&mut state, PathBuf::from(dir)) => {
                    Ok(format!("ok queued {dir}\n"))
                }
                Some(dir) => Err(anyhow!("{dir} is already queued")),
                None => Err(anyhow!("unknown request: {request}")),
            },
        };
        drop(state);
        let reply = reply.unwrap_or_else(|error| format!("error {error:#}\n"));
        (&stream).write_all(reply.as_bytes())?;
        Ok(())
    }

    fn start(&self, state: &mut State) -> Result<String> {
        if state.recording.is_some() {
            bail!("already recording");
        }
        let recording = Recording::start(&self.meetings)?;
        let dir = recording.dir.clone();
        state.recording = Some(recording);
        notify("Recording started", &self.name(&dir), BRIEFLY);
        Ok(format!("ok recording to {}\n", dir.display()))
    }

    /// Stops the recording and queues it, or discards it if it's too short.
    fn stop(&self, state: &mut State) -> Result<String> {
        let Some(recording) = state.recording.take() else {
            bail!("not recording");
        };
        let dir = recording.dir.clone();
        if recording.stop() < MIN_RECORDING {
            record::discard(&dir)?;
            notify(
                "Recording discarded",
                &format!("{} was under 30 seconds", self.name(&dir)),
                BRIEFLY,
            );
            return Ok(format!(
                "ok discarded {}: under 30 seconds\n",
                dir.display()
            ));
        }
        notify("Recording stopped", &self.name(&dir), BRIEFLY);
        self.enqueue(state, dir.clone());
        Ok(format!("ok queued {}\n", dir.display()))
    }

    /// A meeting folder's path under the meetings folder, such as
    /// `2026/11/12/1430`, for notifications.
    fn name(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.meetings)
            .unwrap_or(dir)
            .display()
            .to_string()
    }

    /// Finishes writing any recording, without processing it, and exits. The
    /// next startup queues it along with any meeting being processed now.
    fn shut_down(&self) -> ! {
        let mut state = self.state.lock().unwrap();
        if let Some(recording) = state.recording.take() {
            recording.stop();
        }
        process::exit(0);
    }

    /// Queues a folder for processing, unless it's already waiting or being
    /// processed. Returns whether it was queued.
    fn enqueue(&self, state: &mut State, dir: PathBuf) -> bool {
        if state.processing.as_ref() == Some(&dir) || state.queue.contains(&dir) {
            return false;
        }
        state.failed.retain(|(failed, _)| *failed != dir);
        state.queue.push_back(dir);
        self.queued.notify_one();
        true
    }

    /// Processes queued folders one at a time, forever.
    fn work(&self) {
        loop {
            let dir = {
                let state = self.state.lock().unwrap();
                let mut state = self
                    .queued
                    .wait_while(state, |s| s.queue.is_empty())
                    .unwrap();
                let dir = state.queue.pop_front().unwrap();
                state.processing = Some(dir.clone());
                dir
            };
            let result = fipple::process(&dir, &self.settings);
            let mut state = self.state.lock().unwrap();
            state.processing = None;
            match result {
                Ok(finished) => {
                    drop(state);
                    notify("Meeting processed", &self.name(&finished), Timeout::Default);
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    eprintln!("processing {}: {error}", dir.display());
                    let body = format!("{}: {error}", self.name(&dir));
                    state.failed.push((dir, error));
                    drop(state);
                    notify("Processing failed", &body, Timeout::Default);
                }
            }
        }
    }
}

fn status(state: &State) -> String {
    let mut reply = String::from("ok\n");
    if let Some(recording) = &state.recording {
        let seconds = recording.started.elapsed().as_secs();
        writeln!(
            reply,
            "recording: {} for {:02}:{:02}:{:02}",
            recording.dir.display(),
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
        .unwrap();
    }
    if let Some(dir) = &state.processing {
        writeln!(reply, "processing: {}", dir.display()).unwrap();
    }
    for dir in &state.queue {
        writeln!(reply, "queued: {}", dir.display()).unwrap();
    }
    for (dir, error) in &state.failed {
        writeln!(reply, "failed: {}: {error}", dir.display()).unwrap();
    }
    if reply == "ok\n" {
        reply.push_str("idle\n");
    }
    reply
}

/// Sends a desktop notification. A failure is logged rather than returned,
/// since nothing else depends on it.
fn notify(summary: &str, body: &str, timeout: Timeout) {
    if let Err(error) = Notification::new()
        .appname("fipple")
        .summary(summary)
        .body(body)
        .timeout(timeout)
        .show()
    {
        eprintln!("sending a notification: {error}");
    }
}
