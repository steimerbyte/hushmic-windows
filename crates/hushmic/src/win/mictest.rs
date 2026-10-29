//! Windows microphone test: the audio-only counterpart of Linux's
//! [`crate::mictest`] fallback, for when the A/B window cannot start
//! (no GL, `--test-window` refused) — the same role the file-based recording
//! test plays on Linux.
//!
//! Same user-visible flow as Linux: record [`RECORD_SECS`] seconds from the
//! configured microphone, then play the raw and the suppressed take, with a
//! notification before each step. The suppressed take is derived exactly as
//! the A/B window derives it (the raw take replayed through a fresh engine),
//! so this is the same comparison without the graphics.
//!
//! The returned summary is the one thing Linux's mic test does not print: the
//! take's peak/RMS and the device name. There is no catalog key for a level
//! report and the translation files are outside this workstream, so the
//! caller shows this line as the last notification (English) instead of
//! inventing a key.

use crate::abtest::sample::{align_legs, levels, write_wav, LEVEL_FLOOR_DB};
use crate::win::abtest::{derive_processed, play_blocking, prepare_engine};
use crate::win::audio::WasapiDuplex;
use crate::win::{Duplex, HOP};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The speak window, the same 5 s Linux's mic test uses.
pub const RECORD_SECS: u64 = 5;

const RAW_NAME: &str = "mictest-raw.wav";
const CLEAN_NAME: &str = "mictest-cleaned.wav";

/// Record, then play the raw and the suppressed take. Blocking: run it on a
/// worker thread. `capture` is the configured microphone (empty = the system
/// default), `say` shows the step notifications. The returned line reports
/// the level and the device name.
pub fn run(capture: &str, say: &dyn Fn(&str, &str)) -> Result<String, String> {
    let title = crate::tr!("notify-mictest-title");
    let spec = prepare_engine();
    say(
        &title,
        &crate::tr!("notify-mictest-recording-body", secs = RECORD_SECS),
    );

    let mut dev = WasapiDuplex::open(capture, "")
        .map_err(|e| format!("the microphone could not be opened: {e}"))?;
    let device = capture_name(&dev.describe()).to_string();
    let mut raw = Vec::with_capacity(RECORD_SECS as usize * 48_000);
    let deadline = Instant::now() + Duration::from_secs(RECORD_SECS);
    let mut hop = [0.0f32; HOP];
    while Instant::now() < deadline {
        match dev.read(&mut hop) {
            Ok(0) => {
                return Err(format!(
                    "the microphone stream ended {} s into the test",
                    deadline.saturating_duration_since(Instant::now()).as_secs()
                ))
            }
            Ok(n) => raw.extend_from_slice(&hop[..n]),
            Err(e) => return Err(format!("the microphone failed: {e}")),
        }
    }
    // The duplex must be closed before the legs are generated: its render
    // half would keep the endpoint busy while the takes play.
    drop(dev);

    let lv = levels(&raw);
    if lv.peak_db <= LEVEL_FLOOR_DB {
        return Err(
            "the microphone delivered no audio at all — if it's a wireless headset, make sure \
             it's switched on"
                .to_string(),
        );
    }
    // Same derivation as the A/B window's take: a fresh engine over the raw
    // capture, so both legs are the same input.
    let processed = if spec.assets_error.is_some() {
        vec![0.0; raw.len()]
    } else {
        derive_processed(&raw, spec.attn_db)
    };
    // Same declared latency between the legs as the window.
    let (raw_aligned, processed_aligned) = align_legs(&raw, &processed);
    let dir = work_dir().map_err(|e| format!("no private directory for the recording: {e}"))?;
    let raw_wav = dir.join(RAW_NAME);
    let clean_wav = dir.join(CLEAN_NAME);
    let stored = write_wav(&raw_wav, raw_aligned)
        .and_then(|()| write_wav(&clean_wav, processed_aligned));
    if let Err(e) = stored {
        remove(&raw_wav);
        remove(&clean_wav);
        return Err(format!("the recording could not be stored: {e}"));
    }

    say(&title, &crate::tr!("notify-mictest-playing-raw-body"));
    let played = play_blocking(&raw_wav).and_then(|()| {
        say(&title, &crate::tr!("notify-mictest-playing-clean-body"));
        play_blocking(&clean_wav)
    });
    // The recording is the user's voice: never leave it behind.
    remove(&raw_wav);
    remove(&clean_wav);
    let _ = std::fs::remove_dir(&dir);
    played.map_err(|e| format!("playback failed: {e}"))?;
    // Same closing message as Linux's mic test; the level report follows as
    // the caller's last notification.
    say(&title, &crate::tr!("notify-mictest-finished-body"));

    let note = match &spec.assets_error {
        Some(e) => format!(" (the suppressed take was silent: {e})"),
        None => String::new(),
    };
    Ok(format!(
        "{device}: peak {:.1} dBFS, RMS {:.1} dBFS over {} s{note}",
        lv.peak_db, lv.rms_db, RECORD_SECS
    ))
}

/// The capture endpoint's friendly name out of
/// [`Duplex::describe`]'s "capture [format] -> render [format]".
fn capture_name(describe: &str) -> &str {
    describe.split(" [").next().unwrap_or(describe)
}

/// Recordings live in a per-process temp directory (a running A/B window uses
/// its own), so no two tests can overwrite each other's files.
fn work_dir() -> io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("hushmic-mictest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_name_is_the_first_half_of_describe() {
        assert_eq!(
            capture_name("Microphone (Realtek(R) Audio) [2 ch 48000] -> Speakers [2 ch 48000]"),
            "Microphone (Realtek(R) Audio)"
        );
        assert_eq!(capture_name("no brackets"), "no brackets");
    }
}
