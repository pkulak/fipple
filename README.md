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
  services.fipple.enable = true;
}
```

The packaged binary brings its own `pw-record`, `ffmpeg` and `ffprobe`. The daemon won't start until it has an OpenRouter key; see [the OpenRouter Key](#the-openrouter-key).

## Installing Without Nix

Install the binary with Cargo:

```sh
cargo install --git https://github.com/pkulak/fipple
```

It links against version 2 of PulseAudio's [webrtc-audio-processing](https://gitlab.freedesktop.org/pulseaudio/webrtc-audio-processing) so building it needs that library's development package, plus Clang. Many distributions still ship only version 1. If yours does, build the library from source along with fipple, which also needs Meson and Ninja:

```sh
cargo install --git https://github.com/pkulak/fipple --features bundled
```

fipple runs `pw-record`, `ffmpeg` and `ffprobe`, so make sure they're installed (`pw-record` usually comes with PipeWire's tools package). Then copy [`fipple.service`](fipple.service) into `~/.config/systemd/user/` and enable it:

```sh
systemctl --user enable --now fipple
```

The unit runs `~/.cargo/bin/fipple daemon`. If you put the binary somewhere else, edit `ExecStart`.

## The OpenRouter Key

The daemon reads `OPENROUTER_API_KEY` from its environment and refuses to start without it. As a systemd user service, it gets its environment from the systemd user manager, so that's where the key has to go. The easiest way is a file in `~/.config/environment.d/`:

```sh
mkdir -p ~/.config/environment.d
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > ~/.config/environment.d/openrouter.conf
chmod 600 ~/.config/environment.d/openrouter.conf
```

The user manager reads that directory when it starts, so it takes effect at your next login. To use it right away:

```sh
systemctl --user set-environment OPENROUTER_API_KEY=sk-or-v1-...
systemctl --user restart fipple
```

Every user service can see the key this way. On Nix, don't put the key in your Home Manager config, or it'll end up in the world-readable store.

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

Desktop notifications tell you when a recording starts, stops or is discarded, and when processing finishes or fails.

A recording shorter than 30 seconds is taken to be a double-tapped hotkey, and its folder is deleted.

### When Processing Fails

A folder still named only with its start time, such as `1430`, hasn't finished processing. The audio stays put, and the daemon picks the meeting up again the next time it starts. To retry sooner, run `fipple process` on the folder. Steps that already finished are skipped, so a retry doesn't pay for the same transcription twice.

Each track is sent to OpenRouter in one request, which works out to a limit of about two hours and 15 minutes per meeting. Longer meetings fail with "track too long for one request".

## Configuration

The daemon reads these environment variables when it starts:

| Variable | Default | Meaning |
| --- | --- | --- |
| `OPENROUTER_API_KEY` | required | OpenRouter key |
| `FIPPLE_DIR` | `~/Meetings` | Where meeting folders go |
| `FIPPLE_NAME` | `Me` | Your label in the transcript and summary |
| `FIPPLE_TRANSCRIBE_MODEL` | `microsoft/mai-transcribe-2` | Speech-to-text model |
| `FIPPLE_TRANSCRIBE_OPTIONS` | `{"azure":{"diarization":{"enabled":true}}}` | JSON sent as `provider.options` when transcribing each track |
| `FIPPLE_TITLE_MODEL` | `anthropic/claude-haiku-5.5` | Chat model for the title |
| `FIPPLE_SUMMARY_MODEL` | `openai/gpt-6.1-sol` | Chat model for the summary |
| `FIPPLE_ECHO_CANCEL` | `false` | `true` to cancel speaker echo out of your track; see [Using Speakers](#using-speakers) |
| `FIPPLE_ON_FINISHED` | none | Script to run on each processed meeting; see [When a Meeting Finishes](#when-a-meeting-finishes) |
| `FIPPLE_SUMMARY_INSTRUCTIONS` | none | File of your own instructions for the summary model; see [Your Own Instructions](#your-own-instructions) |
| `FIPPLE_TITLE_INSTRUCTIONS` | none | File of your own instructions for the title model |

Set them the same way as the key, then restart the service. With Home Manager, you can also set them on the service:

```nix
systemd.user.services.fipple.Service.Environment = [ "FIPPLE_NAME=Alice" ];
```

`FIPPLE_TRANSCRIBE_OPTIONS` is what turns on speaker labels, and every provider asks for them differently. If you change `FIPPLE_TRANSCRIBE_MODEL`, you MUST set `FIPPLE_TRANSCRIBE_OPTIONS` to match the new model's provider. Otherwise the request will probably fail, or everyone else in the meeting will be lumped together as `Them`.

## Using Speakers

If you listen through speakers rather than headphones, your microphone picks up everyone else too, and their words show up in the transcript twice: once as themselves and once as you. Set `FIPPLE_ECHO_CANCEL=true` to fix that, or with Home Manager:

```nix
services.fipple.echoCancel = true;
```

Before transcribing, fipple uses WebRTC's echo canceller to remove the desktop track from your mic track, and saves the result as `assets/mic-clean.ogg`. `mic.ogg` stays as it was recorded. Recording doesn't change, so your call app's own echo cancellation works as usual.

The canceller can only remove what fipple recorded on the desktop track, so your call app has to play through your default output.

## Your Own Instructions

To tell the models something about how you work, such as who's on your team or what language to write in, put it in a file and point `FIPPLE_SUMMARY_INSTRUCTIONS` or `FIPPLE_TITLE_INSTRUCTIONS` at it. With Home Manager:

```nix
services.fipple.summaryInstructions = pkgs.writeText "summary.md" ''
  Write the minutes in English. My team is Alice, Bob and Carol.
'';
```

## When a Meeting Finishes

To do something with each meeting once it's processed, such as copy the summary into your notes, point `FIPPLE_ON_FINISHED` at an executable. With Home Manager:

```nix
services.fipple.onFinished = pkgs.writeShellScript "fipple-finished" ''
  cp summary.md ~/Notes/Meetings/"$(basename "$1")".md
'';
```

fipple runs it inside the finished meeting folder, such as `~/Meetings/2026/11/12/1430 Q3 planning`, and passes that folder's path as its only argument. Its output goes to the daemon's log. The next meeting isn't processed until it exits.
