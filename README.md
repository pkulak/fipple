# fipple

fipple records meetings on Linux and turns them into a transcript and a summary. Bind `fipple toggle` to a hotkey, press it when the meeting starts and again when it ends, and a few minutes later you'll have a folder like this:

```
~/Meetings/2026/11/12/1430 Q3 planning/
├── transcript.md
├── summary.md
└── assets/
    ├── mic.ogg
    ├── desktop.ogg
    ├── mic.json
    └── desktop.json
```

It records your default microphone and whatever your speakers are playing as two separate tracks, so the transcript can tell your words from everyone else's. Transcription, summarizing and titling all go through [OpenRouter](https://openrouter.ai), so there are no local models to download, but you will need an OpenRouter account. It only works with PipeWire.

## Installing with Home Manager

The flake provides a Home Manager module that installs `fipple` and runs the daemon as a systemd user service:

```nix
{
  inputs.fipple.url = "github:pkulak/fipple";
}
```

```nix
{
  imports = [ inputs.fipple.homeManagerModules.default ];
  services.fipple = {
    enable = true;
    settings.api_key_command = "cat /run/agenix/openrouter";
  };
}
```

The packaged binary brings its own `pw-record`, `ffmpeg` and `ffprobe`. `settings` becomes the [config file](#configuration), and the daemon won't start until it has a way to get your OpenRouter key; see [the OpenRouter Key](#the-openrouter-key).

## Installing Without Nix

Install the binary with Cargo:

```sh
cargo install --git https://github.com/pkulak/fipple
```

It links against version 2 of PulseAudio's [webrtc-audio-processing](https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing) so building it needs that library's development package, plus Clang. Many distributions still ship only version 1. If yours does, build the library from source along with fipple, which also needs Meson and Ninja:

```sh
cargo install --git https://github.com/pkulak/fipple --features bundled
```

fipple runs `pw-record`, `ffmpeg` and `ffprobe`, so make sure they're installed (`pw-record` usually comes with PipeWire's tools package). Write a [config file](#configuration) with at least [your OpenRouter key](#the-openrouter-key), then copy [`fipple.service`](fipple.service) into `~/.config/systemd/user/` and enable it:

```sh
systemctl --user enable --now fipple
```

The unit runs `~/.cargo/bin/fipple daemon`. If you put the binary somewhere else, edit `ExecStart`.

## The OpenRouter Key

The config file's one required setting is `api_key_command`, a shell command that prints your OpenRouter key. The daemon runs it with `sh` when it starts, and won't start if the command fails or prints nothing. Keeping the key in its own file works well:

```toml
api_key_command = "cat ~/.config/fipple/openrouter-key"
```

If you don't mind the key sitting in the config file, `echo sk-or-v1-...` works too. With Home Manager, though, the config file ends up in the world-readable Nix store, so point the command at a secret managed by something like agenix instead.

## Commands

The client commands talk to the daemon, which must be running.

| Command | What it does |
| --- | --- |
| `fipple toggle` | Starts recording, or stops if it's already recording. This is the one to bind to a hotkey. |
| `fipple start` | Starts recording. It's an error if a recording is already running. |
| `fipple stop` | Stops recording and queues the meeting for processing. It's an error if nothing is recording. |
| `fipple status` | Shows the recording, the meeting being processed, the queue, and any meetings that failed. |
| `fipple process <dir>` | Queues a meeting folder to be processed again. |

`status` looks something like this:

```
$ fipple status
ok
recording: /home/you/Meetings/2026/11/12/1430 for 00:12:05
failed: /home/you/Meetings/2026/11/11/0900: track too long for one request
```

Desktop notifications tell you when a recording starts, stops or is discarded, and when processing finishes or fails. While you're recording, a brief reminder with the elapsed time pops up after 10 minutes and every 10 minutes after that. You can change both times, or turn reminders off, in the [config file](#configuration).

A recording shorter than 30 seconds is taken to be a double-tapped hotkey, and its folder is deleted.

### When Processing Fails

A folder still named only with its start time, such as `1430`, hasn't finished processing. The audio stays put, and the daemon picks the meeting up again the next time it starts. To retry sooner, run `fipple process` on the folder. Steps that already finished are skipped, so a retry doesn't pay for the same transcription twice.

Each track is sent to OpenRouter in one request, which works out to a limit of about two hours and 15 minutes per meeting. Longer meetings fail with "track too long for one request".

## Configuration

The daemon reads `~/.config/fipple/config.toml` (or `$XDG_CONFIG_HOME/fipple/config.toml`) when it starts, and won't start without it:

```toml
api_key_command = "cat ~/.config/fipple/openrouter-key"
name = "Alice"
echo_cancel = true

[reminders]
delay_minutes = 25
```

| Key | Default | Meaning |
| --- | --- | --- |
| `api_key_command` | required | Shell command that prints your OpenRouter key; see [the OpenRouter Key](#the-openrouter-key) |
| `dir` | `~/Meetings` | Where meeting folders go |
| `name` | `Me` | Your label in the transcript and summary |
| `transcribe_model` | `microsoft/mai-transcribe-2` | Speech-to-text model |
| `transcribe_options` | `{ azure = { diarization = { enabled = true } } }` | Table sent as `provider.options` when transcribing each track |
| `title_model` | `anthropic/claude-haiku-5.5` | Chat model for the title |
| `summary_model` | `openai/gpt-6.1-sol` | Chat model for the summary |
| `echo_cancel` | `false` | `true` to cancel speaker echo out of your track; see [Using Speakers](#using-speakers) |
| `on_finished` | none | Script to run on each processed meeting; see [When a Meeting Finishes](#when-a-meeting-finishes) |
| `summary_instructions` | none | File of your own instructions for the summary model; see [Your Own Instructions](#your-own-instructions) |
| `title_instructions` | none | File of your own instructions for the title model |
| `reminders.enabled` | `true` | `false` to turn off the reminders while recording |
| `reminders.delay_minutes` | `10` | Minutes before the first reminder |
| `reminders.interval_minutes` | `10` | Minutes between reminders after that |

Paths can start with `~`. An unknown key is an error, so a typo stops the daemon from starting rather than being ignored. After changing the file, restart the daemon:

```sh
systemctl --user restart fipple
```

With Home Manager, `services.fipple.settings` becomes the file, and switching restarts the daemon for you:

```nix
services.fipple.settings = {
  api_key_command = "cat /run/agenix/openrouter";
  name = "Alice";
  reminders.delay_minutes = 25;
};
```

`transcribe_options` is what turns on speaker labels, and every provider asks for them differently. If you change `transcribe_model`, you MUST set `transcribe_options` to match the new model's provider. Otherwise the request will probably fail, or everyone else in the meeting will be lumped together as `Them`.

## Using Speakers

If you listen through speakers rather than headphones, your microphone picks up everyone else too, and their words show up in the transcript twice: once as themselves and once as you. Set `echo_cancel = true` to fix that, or with Home Manager:

```nix
services.fipple.settings.echo_cancel = true;
```

Before transcribing, fipple uses WebRTC's echo canceller to remove the desktop track from your mic track, and saves the result as `assets/mic-clean.ogg`. `mic.ogg` stays as it was recorded. Recording doesn't change, so your call app's own echo cancellation works as usual.

The canceller can only remove what fipple recorded on the desktop track, so your call app has to play through your default output.

## Your Own Instructions

To tell the models something about how you work, such as who's on your team or what language to write in, put it in a file and point `summary_instructions` or `title_instructions` at it. With Home Manager:

```nix
services.fipple.settings.summary_instructions = pkgs.writeText "summary.md" ''
  Write the minutes in English. My team is Alice, Bob and Carol.
'';
```

## When a Meeting Finishes

To do something with each meeting once it's processed, such as copy the summary into your notes, point `on_finished` at an executable. With Home Manager:

```nix
services.fipple.settings.on_finished = pkgs.writeShellScript "fipple-finished" ''
  cp summary.md ~/Notes/Meetings/"$(basename "$1")".md
'';
```

fipple runs it inside the finished meeting folder, such as `~/Meetings/2026/11/12/1430 Q3 planning`, and passes that folder's path as its only argument. Its output goes to the daemon's log. The next meeting isn't processed until it exits.
