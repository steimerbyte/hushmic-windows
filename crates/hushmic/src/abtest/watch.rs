//! Host-neutral guards around the capture stream: "the microphone delivers
//! nothing" and "the capture leg died". Both backends (PipeWire child
//! processes on Linux, WASAPI in the process on Windows) can only observe
//! these as sample data, so they live here instead of in either backend.

use crate::abtest::types::SAMPLE_RATE;

/// The "your mic is delivering nothing" hint, shared by the two guards
/// that can conclude it (leg death, sustained exact-zero stream).
pub fn silent_mic_hint() -> String {
    crate::tr!("ab-toast-silent-mic")
}

/// Consecutive-exact-zero tracker for the RAW leg. A switched-off (or
/// hardware-muted) device that stays in the node list — measured live
/// with a Jabra Link 380 dongle, headset off — delivers full-rate
/// bit-exact-zero frames: no death, no starvation, healthy meters at the
/// floor, nothing for the other guards to see. Real ADCs always dither
/// (the same machine's quiet built-in mic: ~-51 dBFS, 79% nonzero
/// samples), so a sustained run of exact zeros is a safe "muted or
/// switched off" signal. Fires once per silence run; any nonzero sample
/// resets and re-arms. Pure.
pub struct ZeroWatch {
    zeros: u64,
    fired: bool,
}

/// 3 s of exact zeros before the hint — long enough that stream-start
/// padding can't trip it.
pub const ZERO_WATCH_SAMPLES: u64 = 3 * SAMPLE_RATE as u64;

impl ZeroWatch {
    pub fn new() -> Self {
        ZeroWatch {
            zeros: 0,
            fired: false,
        }
    }

    /// Feed one chunk; true => emit the hint now.
    pub fn feed(&mut self, samples: &[f32]) -> bool {
        if samples.iter().any(|&s| s != 0.0) {
            self.zeros = 0;
            self.fired = false;
            return false;
        }
        self.zeros += samples.len() as u64;
        if !self.fired && self.zeros >= ZERO_WATCH_SAMPLES {
            self.fired = true;
            return true;
        }
        false
    }
}

impl Default for ZeroWatch {
    fn default() -> Self {
        Self::new()
    }
}

/// The toast for an UNEXPECTED capture death while live-monitoring (a
/// device that vanishes mid-capture, the capture stream dying). While
/// recording, the existing recording-abort warning covers it — no second
/// toast. Pure.
pub fn capture_death_warning(was_recording: bool, raw_leg_died: bool) -> Option<String> {
    if was_recording {
        return None;
    }
    Some(if raw_leg_died {
        silent_mic_hint()
    } else {
        crate::tr!("ab-toast-processed-died")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_watch_fires_once_after_three_seconds_of_exact_zeros() {
        let mut w = ZeroWatch::new();
        let chunk = vec![0.0f32; SAMPLE_RATE as usize]; // 1 s per chunk
        assert!(!w.feed(&chunk));
        assert!(!w.feed(&chunk));
        assert!(w.feed(&chunk)); // 3 s reached
        assert!(!w.feed(&chunk)); // once per run
        assert!(!w.feed(&chunk));
    }

    #[test]
    fn zero_watch_resets_and_rearms_on_any_nonzero_sample() {
        let mut w = ZeroWatch::new();
        let zeros = vec![0.0f32; SAMPLE_RATE as usize];
        let mut speck = vec![0.0f32; SAMPLE_RATE as usize];
        speck[123] = 1.0e-6; // one dither-level sample is real signal
        assert!(!w.feed(&zeros));
        assert!(!w.feed(&zeros));
        assert!(!w.feed(&speck)); // reset — never fires on this chunk
        assert!(!w.feed(&zeros));
        assert!(!w.feed(&zeros));
        assert!(w.feed(&zeros)); // re-armed: fires again after a new run
    }

    #[test]
    fn zero_watch_ignores_live_audio_entirely() {
        let mut w = ZeroWatch::new();
        let noise: Vec<f32> = (0..SAMPLE_RATE as usize)
            .map(|i| ((i % 7) as f32 - 3.0) * 1e-4)
            .collect();
        for _ in 0..10 {
            assert!(!w.feed(&noise));
        }
    }

    #[test]
    fn live_capture_death_names_the_silent_mic() {
        crate::i18n::pin_english();
        // The raw leg produced nothing (off-but-listed device): say so.
        let msg = capture_death_warning(false, true).unwrap();
        assert!(msg.contains("microphone"), "{msg}");
        assert!(msg.contains("switched on"), "{msg}");
    }

    #[test]
    fn live_capture_death_of_the_filtered_leg_says_so() {
        crate::i18n::pin_english();
        let msg = capture_death_warning(false, false).unwrap();
        assert!(msg.contains("processed"), "{msg}");
        assert!(!msg.contains("switched on"), "{msg}");
    }

    #[test]
    fn recording_death_defers_to_the_recording_abort_warning() {
        assert_eq!(capture_death_warning(true, true), None);
        assert_eq!(capture_death_warning(true, false), None);
    }
}
