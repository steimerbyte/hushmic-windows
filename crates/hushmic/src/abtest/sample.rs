//! Portable take arithmetic for the A/B window: the declared chain latency,
//! the leg alignment, the level report and the WAV round trip. No device
//! types live here, so the Windows backend can reuse exactly what the Linux
//! one proves and both are unit-tested on every host.

use std::io;
use std::path::Path;

/// The chain's algorithmic latency in samples at 48 kHz: 2400 (STFT
/// framing + the models' group delay, `hushmic_denoiser::LATENCY_SAMPLES`)
/// + 1440 async output lead = 3840 = `dpdfnet_ladspa::align::
/// PLUGIN_LATENCY_SAMPLES` and `win::pump::LATENCY_MS`.
///
/// A processed take is cut by this much before it is compared with or
/// played next to the raw take: an offset of 80 ms is heard as "worse", so
/// the two legs must start at the same instant of the same input.
/// `win::abtest` const-asserts this value against the plugin's constant.
pub const DECLARED_LATENCY_SAMPLES: usize = 3_840;

/// Pair two takes of the same input for comparison: `direct` (the raw
/// capture) loses the tail the delayed leg never produced, `delayed` (the
/// processed take) loses its latency lead-in. Sample `n` of both returned
/// slices is then the same instant of the same input — nothing is copied,
/// so this is free on multi-second takes.
pub fn align_legs<'a>(direct: &'a [f32], delayed: &'a [f32]) -> (&'a [f32], &'a [f32]) {
    let lead = DECLARED_LATENCY_SAMPLES.min(delayed.len());
    let tail = direct.len().min(delayed.len() - lead);
    (&direct[..tail], &delayed[lead..lead + tail])
}

/// Peak and RMS (both dBFS) of one take.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Levels {
    pub peak_db: f32,
    pub rms_db: f32,
}

/// Level floor for the report. `types::DB_FLOOR` (-80 dBFS) is a display
/// limit for the A/B panes and would report a genuinely quiet microphone
/// as exactly "-80"; a level test must tell those apart.
///
/// shortcut: digital silence reads as `LEVEL_FLOOR_DB` instead of -inf so
/// the report stays a plain number; upgrade if a caller needs to show
/// "no signal at all" as its own state.
pub const LEVEL_FLOOR_DB: f32 = -120.0;

/// Peak and RMS of a take in dBFS, floored at [`LEVEL_FLOOR_DB`]. Empty
/// input is the floor on both.
pub fn levels(samples: &[f32]) -> Levels {
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &s in samples {
        peak = peak.max(s.abs());
        sum_sq += f64::from(s) * f64::from(s);
    }
    let rms = if samples.is_empty() {
        0.0
    } else {
        (sum_sq / samples.len() as f64).sqrt() as f32
    };
    let db = |amp: f32| (20.0 * amp.log10()).max(LEVEL_FLOOR_DB);
    Levels {
        peak_db: db(peak),
        rms_db: db(rms),
    }
}

/// Write samples as mono 48 kHz 16-bit PCM — the one WAV form every Windows
/// playback path renders (16-bit is also what Windows' own wave mapper
/// plays without a resampler complaint).
pub fn write_wav(path: &Path, samples: &[f32]) -> io::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: crate::abtest::types::SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .map_err(|e| io::Error::other(format!("cannot write {}: {e}", path.display())))?;
    for &s in samples {
        // Round, don't truncate: -6 dBFS stays symmetric around zero.
        let q = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        w.write_sample(q).map_err(io::Error::other)?;
    }
    w.finalize().map_err(io::Error::other)
}

/// Read a WAV back as mono f32 (16-bit PCM). Test-only: the window plays
/// its files through Windows, it never re-reads them.
#[cfg(test)]
pub fn read_wav(path: &Path) -> io::Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path).map_err(io::Error::other)?;
    let spec = r.spec();
    assert_eq!(spec.channels, 1, "mono take");
    match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| {
                s.map(|v| v as f32 / i16::MAX as f32)
                    .map_err(io::Error::other)
            })
            .collect(),
        hound::SampleFormat::Float => r
            .samples::<f32>()
            .map(|s| s.map_err(io::Error::other))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, len: usize) -> Vec<f32> {
        let rate = crate::abtest::types::SAMPLE_RATE as f32;
        (0..len)
            .map(|n| amp * (std::f32::consts::TAU * 440.0 * n as f32 / rate).sin())
            .collect()
    }

    #[test]
    fn wav_round_trip_keeps_samples_within_one_quantum() {
        let dir = std::env::temp_dir().join(format!("hushmic-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("round-trip.wav");
        let take = sine(0.5, 4_800);
        write_wav(&path, &take).unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(back.len(), take.len());
        for (a, b) in take.iter().zip(&back) {
            assert!((a - b).abs() <= 1.0 / i16::MAX as f32, "{a} != {b}");
        }
        // The header must describe what the Windows playback path needs.
        let spec = hound::WavReader::open(&path).unwrap().spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (1, 48_000, 16)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_writer_clips_instead_of_wrapping() {
        let dir = std::env::temp_dir().join(format!("hushmic-wav-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.wav");
        write_wav(&path, &[2.0, -2.0]).unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(back, vec![1.0, -1.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn align_legs_pairs_each_sample_with_its_delayed_original() {
        let raw: Vec<f32> = (0..10_000).map(|n| n as f32).collect();
        // The processed take starts D samples late: out[n] = raw[n - D], so
        // out[n] stands for the same instant as raw[n - D].
        let lead = DECLARED_LATENCY_SAMPLES;
        let processed: Vec<f32> = (0..raw.len())
            .map(|n| if n >= lead { raw[n - lead] } else { 0.0 })
            .collect();
        let (a, b) = align_legs(&raw, &processed);
        assert_eq!(a.len(), b.len());
        assert_eq!(a.len(), raw.len() - lead);
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert_eq!(*x, raw[i], "sample {i} of the direct leg moved");
            assert_eq!(x, y, "sample {i} is not the same instant");
        }
    }

    #[test]
    fn align_legs_handles_shorter_and_empty_legs() {
        // A take shorter than the declared latency has nothing left to
        // compare — but must not panic or wrap.
        let (a, b) = align_legs(&[1.0; 100], &[1.0; 100]);
        assert!(a.is_empty() && b.is_empty());
        let (a, b) = align_legs(&[], &[1.0; 4_000]);
        assert!(a.is_empty() && b.is_empty());
        // The direct leg may be shorter than the delayed one (a dropped
        // tail): the whole direct leg is the window then, and the delayed
        // leg starts after its latency.
        let (a, b) = align_legs(&[1.0; 4_000], &[0.0; 8_000]);
        assert_eq!((a.len(), b.len()), (4_000, 4_000));
    }

    #[test]
    fn levels_report_peak_and_rms_of_a_full_scale_sine() {
        let lv = levels(&sine(1.0, 48_000));
        // Full scale: peak 0 dBFS, RMS 1/sqrt(2) = -3.01 dBFS.
        assert!(lv.peak_db.abs() < 0.01, "{lv:?}");
        assert!((lv.rms_db + 3.01).abs() < 0.02, "{lv:?}");
    }

    #[test]
    fn levels_floor_silence_and_report_empty_input() {
        assert_eq!(levels(&[0.0; 4_800]).peak_db, LEVEL_FLOOR_DB);
        assert_eq!(levels(&[]).rms_db, LEVEL_FLOOR_DB);
        // -60 dBFS is a quiet but real mic: well above the floor.
        let quiet = sine(0.001, 48_000);
        let lv = levels(&quiet);
        assert!((lv.peak_db + 60.0).abs() < 0.05, "{lv:?}");
    }
}
