//! Drives meeting folders through processing against a stub OpenRouter.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fipple::Settings;
use serde_json::{Value, json};
use tempfile::TempDir;

const MIC_JSON: &str = r#"{"text": "Morning, everyone. I'll send the notes.", "segments": [
    {"id": 0, "start": 0.0, "end": 2.0, "text": " Morning, everyone."},
    {"id": 1, "start": 12.5, "end": 13.0, "text": "   "},
    {"id": 2, "start": 3725.9, "end": 3727.0, "text": " I'll send the notes."}
]}"#;

const DESKTOP_JSON: &str = r#"{"text": "Hi. Let's look at Q3. Sounds good.", "segments": [
    {"id": 0, "start": 0.0, "end": 1.5, "text": "Hi.", "speaker": 0},
    {"id": 1, "start": 4.2, "end": 6.0, "text": "Let's look at Q3.", "speaker": 1},
    {"id": 2, "start": 61.0, "end": 62.0, "text": "Sounds good."},
    {"id": 3, "start": 70.0, "end": 71.0, "text": "", "speaker": 0}
]}"#;

const TRANSCRIPT: &str = "\
[00:00:00] **Phil:** Morning, everyone.
[00:00:00] **Speaker 1:** Hi.
[00:00:04] **Speaker 2:** Let's look at Q3.
[00:01:01] **Them:** Sounds good.
[01:02:05] **Phil:** I'll send the notes.
";

const SUMMARY: &str = "## Overview\nThe team looked at Q3.";

struct Request {
    path: String,
    authorization: String,
    body: Value,
}

/// A local OpenRouter that records each request and answers with `respond`.
struct Stub {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Stub {
    fn start(respond: impl Fn(&str, &Value) -> (u16, String) + Send + 'static) -> Stub {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/v1", server.server_addr().to_ip().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                let path = request.url().to_string();
                let authorization = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.to_string())
                    .unwrap_or_default();
                let (status, reply) = respond(&path, &body);
                recorded.lock().unwrap().push(Request {
                    path,
                    authorization,
                    body,
                });
                let response = tiny_http::Response::from_string(reply)
                    .with_status_code(status)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                request.respond(response).unwrap();
            }
        });
        Stub { url, requests }
    }

    /// Answers like OpenRouter would, with `title` as the title model's reply.
    fn happy(title: &'static str) -> Stub {
        let transcriptions = AtomicUsize::new(0);
        Stub::start(move |path, body| match path {
            "/api/v1/audio/transcriptions" => transcription(&transcriptions),
            "/api/v1/chat/completions" if body["model"] == "test/summary" => (200, chat(SUMMARY)),
            "/api/v1/chat/completions" if body["model"] == "test/title" => (200, chat(title)),
            _ => (404, format!("unexpected request to {path}")),
        })
    }

    fn settings(&self) -> Settings {
        Settings {
            base_url: self.url.clone(),
            name: "Phil".into(),
            transcribe_model: "test/transcribe".into(),
            transcribe_options: json!({"test": {"diarize": true}}),
            title_model: "test/title".into(),
            summary_model: "test/summary".into(),
            ..Settings::new("test-key")
        }
    }

    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }

    /// The text of every message in a chat request, joined.
    fn chat_text(&self, index: usize) -> String {
        let requests = self.requests.lock().unwrap();
        requests[index].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Answers transcription requests in the order `process` makes them: mic, then
/// desktop.
fn transcription(count: &AtomicUsize) -> (u16, String) {
    let json = match count.fetch_add(1, Ordering::SeqCst) {
        0 => MIC_JSON,
        _ => DESKTOP_JSON,
    };
    (200, json.to_string())
}

fn chat(content: &str) -> String {
    json!({"choices": [{"message": {"role": "assistant", "content": content}}]}).to_string()
}

/// A fresh meeting folder holding `files`, given relative to it.
fn meeting(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("2026/11/12/1430");
    fs::create_dir_all(dir.join("assets")).unwrap();
    for (name, content) in files {
        fs::write(dir.join(name), content).unwrap();
    }
    (root, dir)
}

/// Writes a two-second Opus clip to `assets/<track>.ogg`.
fn record(dir: &Path, track: &str) {
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=2")
        .args(["-c:a", "libopus", "-b:a", "32k"])
        .arg(dir.join(format!("assets/{track}.ogg")))
        .status()
        .unwrap();
    assert!(status.success());
}

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap()
}

fn names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

#[test]
fn processes_a_fresh_meeting() {
    let stub = Stub::happy("\"Q3 / planning\"\n");
    let (root, dir) = meeting(&[]);
    record(&dir, "mic");
    record(&dir, "desktop");

    let finished = fipple::process(&dir, &stub.settings()).unwrap();

    assert_eq!(finished, root.path().join("2026/11/12/1430 Q3 planning"));
    assert!(!dir.exists());
    assert_eq!(
        names(&finished),
        BTreeSet::from(["assets", "transcript.md", "summary.md"].map(String::from))
    );
    assert_eq!(
        names(&finished.join("assets")),
        BTreeSet::from(["mic.ogg", "desktop.ogg", "mic.json", "desktop.json"].map(String::from))
    );
    assert_eq!(read(&finished, "assets/mic.json"), MIC_JSON);
    assert_eq!(read(&finished, "assets/desktop.json"), DESKTOP_JSON);
    assert_eq!(read(&finished, "transcript.md"), TRANSCRIPT);
    assert_eq!(read(&finished, "summary.md"), format!("{SUMMARY}\n"));

    assert_eq!(
        stub.paths(),
        [
            "/api/v1/audio/transcriptions",
            "/api/v1/audio/transcriptions",
            "/api/v1/chat/completions",
            "/api/v1/chat/completions",
        ]
    );
    let requests = stub.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.authorization == "Bearer test-key")
    );

    for request in &requests[..2] {
        let body = &request.body;
        assert_eq!(
            body["provider"],
            json!({"options": {"test": {"diarize": true}}})
        );
        assert_eq!(body["model"], "test/transcribe");
        assert_eq!(body["response_format"], "verbose_json");
        assert_eq!(body["timestamp_granularities"], json!(["segment", "word"]));
        assert_eq!(body["input_audio"]["format"], "mp3");
        let audio = STANDARD
            .decode(body["input_audio"]["data"].as_str().unwrap())
            .unwrap();
        assert!(audio.starts_with(b"ID3"), "the upload isn't an MP3");
    }

    assert_eq!(requests[2].body["model"], "test/summary");
    drop(requests);
    let summary_prompt = stub.chat_text(2);
    for expected in ["2026-11-12", "14:30", "00:00:02", TRANSCRIPT, "Phil"] {
        assert!(
            summary_prompt.contains(expected),
            "the summary prompt lacks {expected:?}"
        );
    }

    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[3].body["model"], "test/title");
    assert_eq!(
        requests[3].body["messages"][1]["content"],
        format!("{SUMMARY}\n")
    );
}

#[test]
fn echo_cancelling_keeps_the_cleaned_mic_track() {
    let stub = Stub::happy("Q3 planning");
    let (root, dir) = meeting(&[]);
    record(&dir, "mic");
    record(&dir, "desktop");
    let settings = Settings {
        echo_cancel: true,
        ..stub.settings()
    };

    let finished = fipple::process(&dir, &settings).unwrap();

    assert_eq!(finished, root.path().join("2026/11/12/1430 Q3 planning"));
    assert_eq!(
        names(&finished.join("assets")),
        BTreeSet::from(
            [
                "mic.ogg",
                "mic-clean.ogg",
                "desktop.ogg",
                "mic.json",
                "desktop.json"
            ]
            .map(String::from)
        )
    );
    assert_eq!(read(&finished, "transcript.md"), TRANSCRIPT);
}

#[test]
fn existing_transcriptions_make_no_transcription_requests() {
    let stub = Stub::happy("Q3 planning");
    let (root, dir) = meeting(&[
        ("assets/mic.json", MIC_JSON),
        ("assets/desktop.json", DESKTOP_JSON),
    ]);
    // ffprobe reads the duration from mic.ogg; desktop.ogg isn't needed.
    record(&dir, "mic");

    let finished = fipple::process(&dir, &stub.settings()).unwrap();

    assert_eq!(finished, root.path().join("2026/11/12/1430 Q3 planning"));
    assert_eq!(
        stub.paths(),
        ["/api/v1/chat/completions", "/api/v1/chat/completions"]
    );
    assert_eq!(read(&finished, "transcript.md"), TRANSCRIPT);
    assert_eq!(read(&finished, "summary.md"), format!("{SUMMARY}\n"));
}

#[test]
fn words_make_one_line_per_sentence() {
    let mic = r#"{"text": "Hello there.", "segments": [
        {"start": 2.0, "end": 2.6, "text": "Hello there."}
    ], "words": [
        {"word": "Hello", "start": 2.0, "end": 2.3},
        {"word": " there.", "start": 2.3, "end": 2.6}
    ]}"#;
    // One segment for all of it, the way MAI-Transcribe 2 merges a speaker's
    // turn.
    let desktop = r#"{"text": "", "segments": [
        {"start": 0.0, "end": 9.0, "text": "Everything.", "speaker": 0}
    ], "words": [
        {"word": "Morning.", "start": 0.0, "end": 0.5, "speaker": 0},
        {"word": "Ready", "start": 1.0, "end": 1.2, "speaker": 0},
        {"word": "to", "start": 1.2, "end": 1.3, "speaker": 0},
        {"word": "start?", "start": 1.3, "end": 1.6, "speaker": 0},
        {"word": "So", "start": 3.0, "end": 3.2, "speaker": 0},
        {"word": "yes!", "start": 3.4, "end": 3.6, "speaker": 1}
    ]}"#;
    let stub = Stub::happy("Q3 planning");
    let (root, dir) = meeting(&[("assets/mic.json", mic), ("assets/desktop.json", desktop)]);
    record(&dir, "mic");

    fipple::process(&dir, &stub.settings()).unwrap();

    assert_eq!(
        read(
            &root.path().join("2026/11/12/1430 Q3 planning"),
            "transcript.md"
        ),
        "\
[00:00:00] **Speaker 1:** Morning.
[00:00:01] **Speaker 1:** Ready to start?
[00:00:02] **Phil:** Hello there.
[00:00:03] **Speaker 1:** So
[00:00:03] **Speaker 2:** yes!
"
    );
}

#[test]
fn existing_outputs_skip_their_steps() {
    let stub = Stub::happy("Q3 planning");
    let (root, dir) = meeting(&[
        ("assets/mic.json", MIC_JSON),
        ("assets/desktop.json", DESKTOP_JSON),
        ("transcript.md", "edited transcript\n"),
        ("summary.md", "edited summary\n"),
    ]);

    let finished = fipple::process(&dir, &stub.settings()).unwrap();

    assert_eq!(finished, root.path().join("2026/11/12/1430 Q3 planning"));
    assert_eq!(stub.paths(), ["/api/v1/chat/completions"]);
    assert_eq!(stub.chat_text(0).lines().last(), Some("edited summary"));
    assert_eq!(read(&finished, "transcript.md"), "edited transcript\n");
    assert_eq!(read(&finished, "summary.md"), "edited summary\n");
}

#[test]
fn cleans_the_title() {
    // The 60th character is a space, which goes too.
    let long = "abcdefghi ".repeat(8);
    let cases = [
        ("  “Q3 planning”  ", "Q3 planning"),
        ("'Budget\t\n  review'", "Budget review"),
        ("`Q3/Q4 roadmap`", "Q3Q4 roadmap"),
        ("Bell\u{7} test\u{0}", "Bell test"),
        (long.as_str(), &long[..59]),
    ];
    for (reply, title) in cases {
        let reply: &'static str = reply.to_string().leak();
        let stub = Stub::happy(reply);
        let (root, dir) = meeting(&[
            ("assets/mic.json", MIC_JSON),
            ("assets/desktop.json", DESKTOP_JSON),
            ("summary.md", SUMMARY),
        ]);

        let finished = fipple::process(&dir, &stub.settings()).unwrap();

        assert_eq!(
            finished,
            root.path().join(format!("2026/11/12/1430 {title}")),
            "for the reply {reply:?}"
        );
    }
}

#[test]
fn an_empty_title_fails() {
    let stub = Stub::happy(" \"\" \n");
    let (_root, dir) = meeting(&[
        ("assets/mic.json", MIC_JSON),
        ("assets/desktop.json", DESKTOP_JSON),
        ("summary.md", SUMMARY),
    ]);

    let error = fipple::process(&dir, &stub.settings()).unwrap_err();

    assert!(error.to_string().contains("empty title"), "{error:#}");
    assert!(dir.exists());
}

#[test]
fn a_failing_response_leaves_the_bare_name() {
    let transcriptions = AtomicUsize::new(0);
    let stub = Stub::start(move |path, _| match path {
        "/api/v1/audio/transcriptions" => transcription(&transcriptions),
        _ => (500, "upstream error".to_string()),
    });
    let (_root, dir) = meeting(&[]);
    record(&dir, "mic");
    record(&dir, "desktop");

    let error = fipple::process(&dir, &stub.settings()).unwrap_err();

    assert!(format!("{error:#}").contains("500"), "{error:#}");
    assert_eq!(
        names(dir.parent().unwrap()),
        BTreeSet::from(["1430".into()])
    );
    assert!(!dir.join("summary.md").exists());
}
