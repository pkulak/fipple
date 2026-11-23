use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use fipple::Settings;
use notify_rust::{Hint, Notification, Timeout};
use signal_hook::consts::SIGTERM;
use signal_hook::iterator::Signals;

use crate::config::{self, AutoRecord, Reminders};
use crate::record::{self, Recording};

/// Recordings shorter than this are taken to be accidental and discarded.
const MIN_RECORDING: Duration = Duration::from_secs(30);

/// How long notifications about something the user just did stay up.
const BRIEFLY: Timeout = Timeout::Milliseconds(5000);

/// How long a reminder stays up. Another one follows, so it can be short.
const REMINDER_TIMEOUT: Timeout = Timeout::Milliseconds(3000);

/// Listens for client requests, records meetings and processes them one at a
/// time.
pub fn run() -> Result<()> {
    let config = config::load()?;
    let meetings = config.meetings;
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
        settings: config.settings,
        meetings,
        on_finished: config.on_finished,
        reminders: config.reminders,
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
    if let Some(auto_record) = config.auto_record {
        thread::spawn({
            let daemon = daemon.clone();
            move || daemon.watch(auto_record)
        });
    }
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
    /// Run in each meeting folder after it's processed, with the folder as
    /// its argument.
    on_finished: Option<PathBuf>,
    reminders: Reminders,
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
    fn handle(self: &Arc<Self>, stream: UnixStream) -> Result<()> {
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

    fn start(self: &Arc<Self>, state: &mut State) -> Result<String> {
        if state.recording.is_some() {
            bail!("already recording");
        }
        let recording = Recording::start(&self.meetings)?;
        let dir = recording.dir.clone();
        let started = recording.started;
        state.recording = Some(recording);
        if self.reminders.enabled {
            thread::spawn({
                let daemon = self.clone();
                move || daemon.remind(started)
            });
        }
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

    /// Pops up reminders on the configured schedule until the recording that
    /// began at `started` stops. Each one replaces the last and stays out of
    /// the notification history.
    fn remind(&self, started: Instant) {
        let mut id = None;
        let mut minutes = self.reminders.delay_minutes;
        loop {
            thread::sleep(Duration::from_secs(minutes * 60));
            minutes = self.reminders.interval_minutes.get();
            let state = self.state.lock().unwrap();
            if state.recording.as_ref().map(|r| r.started) != Some(started) {
                return;
            }
            drop(state);
            let summary = format!("Recording · {} min", started.elapsed().as_secs() / 60);
            let mut notification = Notification::new();
            notification
                .appname("fipple")
                .summary(&summary)
                .hint(Hint::Transient(true))
                .timeout(REMINDER_TIMEOUT);
            if let Some(id) = id {
                notification.id(id);
            }
            match notification.show() {
                Ok(handle) => id = Some(handle.id()),
                Err(error) => eprintln!("sending a notification: {error}"),
            }
        }
    }

    /// Polls the configured script and starts or stops recordings on changes.
    fn watch(self: &Arc<Self>, auto_record: AutoRecord) {
        let mut tracker = Tracker::new(auto_record.stop_delay, auto_record.auto_start);
        let mut last_error: Option<String> = None;
        loop {
            match poll_script(&auto_record.script) {
                Ok(active) => {
                    last_error = None;
                    match tracker.update(active, Instant::now()) {
                        Some(Action::Start) => {
                            let mut state = self.state.lock().unwrap();
                            if state.recording.is_none()
                                && let Err(error) = self.start(&mut state)
                            {
                                eprintln!("auto-record: starting recording: {error:#}");
                            }
                        }
                        Some(Action::Stop) => {
                            let mut state = self.state.lock().unwrap();
                            if state.recording.is_some()
                                && let Err(error) = self.stop(&mut state)
                            {
                                eprintln!("auto-record: stopping recording: {error:#}");
                            }
                        }
                        None => {}
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!("auto-record: {message}");
                        last_error = Some(message);
                    }
                }
            }
            thread::sleep(auto_record.interval);
        }
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
        std::process::exit(0);
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
                    if let Some(hook) = &self.on_finished
                        && let Err(error) = run_hook(hook, &finished)
                    {
                        let error = format!("{error:#}");
                        eprintln!("on-finished script for {}: {error}", finished.display());
                        let body = format!("{}: {error}", self.name(&finished));
                        notify("On-finished script failed", &body, Timeout::Default);
                    }
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

/// Runs the on-finished script in a processed meeting folder. Its output goes
/// to the daemon's.
fn run_hook(hook: &Path, dir: &Path) -> Result<()> {
    let status = Command::new(hook)
        .arg(dir)
        .current_dir(dir)
        .status()
        .with_context(|| format!("running {}", hook.display()))?;
    if !status.success() {
        bail!("{} failed with {status}", hook.display());
    }
    Ok(())
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

/// Runs the meeting-detection script. Exit 0 means active, exit 1 means
/// inactive, and any other status is an error.
fn poll_script(script: &Path) -> Result<bool> {
    let status = Command::new(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("running {}", script.display()))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("{} exited with {status}", script.display()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
}

/// Edge-triggered meeting state with a grace period before stopping.
struct Tracker {
    stop_delay: Duration,
    auto_start: bool,
    active: Option<bool>,
    inactive_since: Option<Instant>,
}

impl Tracker {
    fn new(stop_delay: Duration, auto_start: bool) -> Self {
        Tracker {
            stop_delay,
            auto_start,
            active: None,
            inactive_since: None,
        }
    }

    fn update(&mut self, is_active: bool, now: Instant) -> Option<Action> {
        let Some(was_active) = self.active else {
            self.active = Some(is_active);
            return None;
        };
        match (was_active, is_active) {
            (false, true) => {
                self.active = Some(true);
                self.auto_start.then_some(Action::Start)
            }
            (true, true) => {
                self.inactive_since = None;
                None
            }
            (true, false) => {
                let since = *self.inactive_since.get_or_insert(now);
                if now.duration_since(since) >= self.stop_delay {
                    self.active = Some(false);
                    self.inactive_since = None;
                    Some(Action::Stop)
                } else {
                    None
                }
            }
            (false, false) => None,
        }
    }
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn seeds_initial_state_without_acting() {
        let t = Instant::now();
        let mut tracker = Tracker::new(Duration::from_secs(15), true);
        assert_eq!(tracker.update(true, at(t, 0)), None);
        assert_eq!(tracker.update(true, at(t, 5)), None);

        let mut tracker = Tracker::new(Duration::from_secs(15), true);
        assert_eq!(tracker.update(false, at(t, 0)), None);
        assert_eq!(tracker.update(false, at(t, 5)), None);
    }

    #[test]
    fn starts_immediately_and_stops_after_grace_period() {
        let t = Instant::now();
        let mut tracker = Tracker::new(Duration::from_secs(15), true);
        assert_eq!(tracker.update(false, at(t, 0)), None);
        assert_eq!(tracker.update(true, at(t, 5)), Some(Action::Start));
        // Still active does not emit another Start (respects manual stop).
        assert_eq!(tracker.update(true, at(t, 10)), None);

        // First inactive starts the grace period.
        assert_eq!(tracker.update(false, at(t, 15)), None);
        assert_eq!(tracker.update(false, at(t, 20)), None);
        assert_eq!(tracker.update(false, at(t, 29)), None);
        assert_eq!(tracker.update(false, at(t, 30)), Some(Action::Stop));
        // Staying inactive does not emit another Stop (respects manual start).
        assert_eq!(tracker.update(false, at(t, 35)), None);

        // Next meeting triggers a fresh Start.
        assert_eq!(tracker.update(true, at(t, 40)), Some(Action::Start));
    }

    #[test]
    fn brief_gap_cancels_pending_stop() {
        let t = Instant::now();
        let mut tracker = Tracker::new(Duration::from_secs(15), true);
        assert_eq!(tracker.update(false, at(t, 0)), None);
        assert_eq!(tracker.update(true, at(t, 5)), Some(Action::Start));

        assert_eq!(tracker.update(false, at(t, 10)), None);
        assert_eq!(tracker.update(false, at(t, 20)), None);
        // Window comes back before the 15s grace period finishes.
        assert_eq!(tracker.update(true, at(t, 24)), None);

        // A later drop must wait a full 15s again.
        assert_eq!(tracker.update(false, at(t, 30)), None);
        assert_eq!(tracker.update(false, at(t, 40)), None);
        assert_eq!(tracker.update(false, at(t, 45)), Some(Action::Stop));
    }

    #[test]
    fn auto_start_false_only_stops() {
        let t = Instant::now();
        let mut tracker = Tracker::new(Duration::from_secs(15), false);
        assert_eq!(tracker.update(false, at(t, 0)), None);
        assert_eq!(tracker.update(true, at(t, 5)), None);
        assert_eq!(tracker.update(false, at(t, 10)), None);
        assert_eq!(tracker.update(false, at(t, 25)), Some(Action::Stop));
    }

    #[test]
    fn zero_stop_delay_stops_immediately() {
        let t = Instant::now();
        let mut tracker = Tracker::new(Duration::ZERO, true);
        assert_eq!(tracker.update(true, at(t, 0)), None);
        assert_eq!(tracker.update(false, at(t, 5)), Some(Action::Stop));
    }

    #[test]
    fn poll_script_maps_exit_codes() {
        let dir = tempdir().unwrap();
        let write_script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };

        assert!(poll_script(&write_script("active", "exit 0")).unwrap());
        assert!(!poll_script(&write_script("inactive", "exit 1")).unwrap());
        assert!(poll_script(&write_script("error", "exit 2")).is_err());
        assert!(poll_script(&dir.path().join("missing")).is_err());
    }
}
