use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};
use webrtc_audio_processing::config::{Config, EchoCanceller};
use webrtc_audio_processing::{Processor, Stats};

use crate::process::timestamp;

const RATE: &str = "48000";

/// Ten minutes of the canceller's 10 ms frames.
const LOG_EVERY_FRAMES: usize = 60_000;

/// How far the mic is delayed going into the canceller. The canceller only
/// finds an echo that comes 5 to 500 ms after its source, and the tracks
/// usually start within a few tens of milliseconds of each other, in either
/// order. Delaying the mic puts the usual lag in the middle of that range.
const MIC_DELAY_MS: i64 = 200;
const MIC_DELAY_FRAMES: usize = MIC_DELAY_MS as usize / 10;

/// Writes `out` as Opus: the `mic` track with the echo of the `desktop` track
/// cancelled out of it.
///
/// The canceller finds the delay between the tracks itself. Its estimates of
/// that delay and of its echo return loss enhancement (ERLE) are logged every
/// ten minutes, and at the end.
pub fn cancel(mic: &Path, desktop: &Path, out: &Path) -> Result<()> {
    let processor = Processor::new(RATE.parse()?)?;
    processor.set_config(Config {
        echo_canceller: Some(EchoCanceller::default()),
        ..Default::default()
    });

    let mut mic_decoder = decode(mic)?;
    let mut desktop_decoder = decode(desktop)?;
    let mut encoder = Command::new("ffmpeg")
        .args([
            "-nostdin", "-v", "error", "-y", "-f", "f32le", "-ar", RATE, "-ac", "1", "-i",
            "pipe:0", "-c:a", "libopus", "-b:a", "32k", "-f", "ogg",
        ])
        .arg(out)
        .stdin(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;

    let mut near_in = BufReader::new(mic_decoder.stdout.take().unwrap());
    let mut far_in = BufReader::new(desktop_decoder.stdout.take().unwrap());
    let mut encoded = BufWriter::new(encoder.stdin.take().unwrap());
    let mut near = vec![0.0; processor.num_samples_per_frame()];
    let mut far = near.clone();
    let mut mic_frames = 0;
    let mut stats = processor.get_stats();
    for frame in 0.. {
        // While the mic is delayed, its frames are silent and nothing is
        // written, which trims the delay back off.
        let samples = if frame < MIC_DELAY_FRAMES {
            0
        } else {
            let samples = read_frame(&mut near_in, &mut near)?;
            if samples == 0 {
                break;
            }
            samples
        };
        // If the desktop track ends first, silence stands in for the rest.
        read_frame(&mut far_in, &mut far)?;
        processor.analyze_render_frame([&far[..]])?;
        processor.process_capture_frame([&mut near[..]])?;
        // WebRTC hands back the first statistics since the last read, so read
        // them every frame to keep them current.
        stats = processor.get_stats();
        for sample in &near[..samples] {
            encoded.write_all(&sample.to_le_bytes())?;
        }
        if samples > 0 {
            mic_frames += 1;
            if mic_frames % LOG_EVERY_FRAMES == 0 {
                log(&stats, mic, mic_frames);
            }
        }
    }
    log(&stats, mic, mic_frames);
    // Closing stdin tells the encoder the track is over.
    encoded.into_inner().map_err(|e| e.into_error())?;
    // Read the desktop track to the end, so its decoder doesn't fail on a
    // closed pipe and a real decoding error isn't hidden.
    io::copy(&mut far_in, &mut io::sink())?;

    for (child, file) in [
        (mic_decoder, mic),
        (desktop_decoder, desktop),
        (encoder, out),
    ] {
        wait(child, file)?;
    }
    Ok(())
}

/// Logs the canceller's estimates, `frames` into the mic track.
fn log(stats: &Stats, mic: &Path, frames: usize) {
    let lag = stats.delay_ms.map_or("unknown".into(), |ms| {
        format!("{} ms", i64::from(ms) - MIC_DELAY_MS)
    });
    let erle = stats
        .echo_return_loss_enhancement
        .map_or("unknown".into(), |db| format!("{db:.1} dB"));
    eprintln!(
        "cancelling echo in {} at {}: echo lag {lag}, ERLE {erle}",
        mic.display(),
        timestamp(frames as f64 / 100.0)
    );
}

/// Starts an ffmpeg that writes `path` to stdout as raw mono samples.
fn decode(path: &Path) -> Result<Child> {
    Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-ar", RATE, "-ac", "1", "pipe:1"])
        .stdout(Stdio::piped())
        .spawn()
        .context("running ffmpeg")
}

fn wait(mut child: Child, file: &Path) -> Result<()> {
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg failed with {status} on {}", file.display());
    }
    Ok(())
}

/// Fills `frame` with the next samples, padding with silence past the end.
/// Returns how many samples there were.
fn read_frame(input: &mut impl Read, frame: &mut [f32]) -> io::Result<usize> {
    let mut bytes = Vec::with_capacity(frame.len() * 4);
    input
        .by_ref()
        .take(bytes.capacity() as u64)
        .read_to_end(&mut bytes)?;
    let mut chunks = bytes.as_chunks().0.iter();
    for sample in frame.iter_mut() {
        *sample = chunks
            .next()
            .map_or(0.0, |&chunk| f32::from_le_bytes(chunk));
    }
    Ok(bytes.len() / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg(args: &[&str]) {
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn samples(path: &Path) -> Vec<f32> {
        let output = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(path)
            .args(["-f", "f32le", "-ac", "1", "pipe:1"])
            .output()
            .unwrap();
        assert!(output.status.success());
        output
            .stdout
            .as_chunks()
            .0
            .iter()
            .map(|&chunk| f32::from_le_bytes(chunk))
            .collect()
    }

    fn level(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn cancels_a_late_echo() {
        assert_cancels("adelay=80");
    }

    #[test]
    fn cancels_an_early_echo() {
        assert_cancels("atrim=start=0.05,asetpts=PTS-STARTPTS");
    }

    /// Records the desktop and a mic that hears it through `echo`, and checks
    /// that cancelling removes the echo.
    fn assert_cancels(echo: &str) {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.ogg");
        let mic = dir.path().join("mic.ogg");
        let out = dir.path().join("mic-clean.ogg");
        let opus = ["-c:a", "libopus", "-b:a", "32k"];
        // Bursts of noise, like speech. The canceller removes much less of a
        // steady noise.
        ffmpeg(
            &[
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "anoisesrc=color=pink:duration=10:amplitude=0.3",
                ][..],
                &["-af", "volume=if(lt(mod(t\\,1)\\,0.4)\\,1\\,0):eval=frame"],
                &opus,
                &[desktop.to_str().unwrap()],
            ]
            .concat(),
        );
        // A quieter copy, as if the mic heard the speakers.
        ffmpeg(
            &[
                &[
                    "-i",
                    desktop.to_str().unwrap(),
                    "-af",
                    &format!("{echo},volume=0.4"),
                ][..],
                &opus,
                &[mic.to_str().unwrap()],
            ]
            .concat(),
        );

        cancel(&mic, &desktop, &out).unwrap();

        let (before, after) = (samples(&mic), samples(&out));
        assert_eq!(before.len(), after.len());
        // Skip the first two seconds, while the canceller converges.
        let (before, after) = (level(&before[96_000..]), level(&after[96_000..]));
        assert!(
            after < before / 100.0,
            "the echo only went from {before} to {after}"
        );
    }
}
