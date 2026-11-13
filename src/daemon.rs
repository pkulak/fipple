use std::collections::VecDeque;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use anyhow::{Context, Result};
use fipple::Settings;
use notify_rust::Notification;

/// Listens for client requests and processes meetings one at a time.
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
        state: Mutex::default(),
        queued: Condvar::new(),
    });
    for dir in unfinished(&meetings)? {
        daemon.enqueue(dir);
    }
    thread::spawn({
        let daemon = daemon.clone();
        move || daemon.work()
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

/// The folders in `meetings` that haven't been titled yet, oldest first.
fn unfinished(meetings: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in
        fs::read_dir(meetings).with_context(|| format!("reading {}", meetings.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(fipple::is_unfinished)
        {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

struct Daemon {
    settings: Settings,
    state: Mutex<State>,
    /// Signalled when a folder joins the queue.
    queued: Condvar,
}

#[derive(Default)]
struct State {
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
        let reply = match request {
            "status" => self.status(),
            _ => match request.strip_prefix("process ") {
                Some(dir) if self.enqueue(PathBuf::from(dir)) => format!("ok queued {dir}\n"),
                Some(dir) => format!("error {dir} is already queued\n"),
                None => format!("error unknown request: {request}\n"),
            },
        };
        (&stream).write_all(reply.as_bytes())?;
        Ok(())
    }

    /// Queues a folder for processing, unless it's already waiting or being
    /// processed. Returns whether it was queued.
    fn enqueue(&self, dir: PathBuf) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.processing.as_ref() == Some(&dir) || state.queue.contains(&dir) {
            return false;
        }
        state.failed.retain(|(failed, _)| *failed != dir);
        state.queue.push_back(dir);
        self.queued.notify_one();
        true
    }

    fn status(&self) -> String {
        let state = self.state.lock().unwrap();
        let mut reply = String::from("ok\n");
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
                    notify("Meeting processed", &folder_name(&finished));
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    eprintln!("processing {}: {error}", dir.display());
                    let body = format!("{}: {error}", folder_name(&dir));
                    state.failed.push((dir, error));
                    drop(state);
                    notify("Processing failed", &body);
                }
            }
        }
    }
}

fn folder_name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or(dir.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Sends a desktop notification. A failure is logged rather than returned,
/// since nothing else depends on it.
fn notify(summary: &str, body: &str) {
    if let Err(error) = Notification::new()
        .appname("fipple")
        .summary(summary)
        .body(body)
        .show()
    {
        eprintln!("sending a notification: {error}");
    }
}
