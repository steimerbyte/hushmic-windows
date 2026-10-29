//! The host loop PipeWire used to run for us.
//!
//! On Linux the graph does the work: the server pulls 480-sample quanta from
//! the mic, calls the filter chain in a realtime thread and renders the result
//! into the virtual source. On Windows nobody does that on our behalf, so this
//! module *is* the graph: one hop in, one hop out, forever, on the single
//! thread that owns both devices.
//!
//! It knows devices only through [`Duplex`] and the engine only through
//! [`HopProcessor`], which is what makes the cadence testable off Windows:
//! partial-read reassembly, the hop deadline, the close and error paths and
//! the health counters all run against fakes on every host.
//!
//! Health is written as plain atomics, so the tray and the doctor can read
//! [`Stats`] from their own thread at any moment without a lock.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{Duplex, HopProcessor, HOP, RATE};

/// Algorithmic latency of the chain, reported only when
/// [`PumpConfig::latency_report`] asks for it.
///
/// 3840 samples at [`RATE`] = `dpdfnet_ladspa::align::PLUGIN_LATENCY_SAMPLES`:
/// 2400 (`hushmic_denoiser::LATENCY_SAMPLES` — STFT framing plus the models'
/// group delay) + 1440 async output lead. `hushmic::controller::LATENCY_SAMPLES`
/// pins the same figure for the Linux delay node, so both hosts declare one
/// number to consumers.
pub const LATENCY_MS: f32 = 80.0;

/// Amplitude whose level is the -120 dBFS floor (`20 * log10(1e-6)`).
const FLOOR_AMP: f32 = 1e-6;

/// Wall time one hop must fit in: [`HOP`] samples at [`RATE`], 10 ms.
const HOP_PERIOD: Duration = Duration::from_nanos(HOP as u64 * 1_000_000_000 / RATE as u64);

/// Engine settings for one run.
///
/// The pump itself only supplies the cadence: the processor already holds
/// `attn_db` and `mode_control` ([`HopProcessor`] has no setters), and
/// `latency_report` only decides whether [`LATENCY_MS`] is published to
/// consumers. They ride along so one config feeds the engine, the log line and
/// the doctor.
pub struct PumpConfig {
    /// Attenuation limit in dB (the LADSPA port's 0..=100 range; 100 is
    /// effectively unlimited).
    pub attn_db: f32,
    /// Raw mode control: 0 = process, 1 = bypass, 2 = mute (rounded and
    /// clamped by the engine).
    pub mode_control: f32,
    /// Declare the chain's [`LATENCY_MS`] to consumers (the Linux conf's
    /// report-only delay node).
    pub latency_report: bool,
}

impl Default for PumpConfig {
    fn default() -> Self {
        PumpConfig {
            attn_db: 100.0,
            mode_control: 0.0,
            latency_report: true,
        }
    }
}

/// Live health of the running pump: atomics only, no locks, cheap to poll
/// from a UI thread.
///
/// Counters accumulate for the life of the `Stats` value; the caller owns one
/// per stream and drops it with the stream.
pub struct Stats {
    engine_label: OnceLock<String>,
    hops: AtomicU64,
    dropped: AtomicU64,
    underruns: AtomicU64,
    silence: AtomicU64,
    peak_bits: AtomicU32,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            engine_label: OnceLock::new(),
            hops: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            silence: AtomicU64::new(0),
            peak_bits: AtomicU32::new(FLOOR_DBFS.to_bits()),
        }
    }
}

/// The floor [`Stats::peak_dbfs`] starts at and never reports below.
const FLOOR_DBFS: f32 = -120.0;

impl Stats {
    pub fn new() -> Self {
        Stats::default()
    }

    /// The processor's own description; empty until a run has started.
    pub fn engine(&self) -> String {
        self.engine_label.get().cloned().unwrap_or_default()
    }

    /// Hops fully processed.
    pub fn hops(&self) -> u64 {
        self.hops.load(Ordering::Relaxed)
    }

    /// Hops whose processing overran its own 10 ms period — the overload
    /// signal the tier ladder reacts to.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Hops the device could not fill (it closed mid-hop).
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    /// Hops that came out exactly silent: nothing reached the render side.
    pub fn silence_hops(&self) -> u64 {
        self.silence.load(Ordering::Relaxed)
    }

    /// Peak of the last non-silent output hop in dBFS, floored at -120.0.
    pub fn peak_dbfs(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }
}

/// The loop's own state: the two hop buffers, allocated once and reused for
/// every iteration. Nothing in [`Pump::run`] allocates after construction.
pub struct Pump {
    input: [f32; HOP],
    output: [f32; HOP],
}

impl Pump {
    /// Drives `dev` until `stop` is set or the device fails.
    ///
    /// Returns a one-line reason when it stopped for a failure, `None` on a
    /// clean stop. `_cfg` is carried for the caller's log and doctor line —
    /// see [`PumpConfig`].
    pub fn run(
        dev: &mut dyn Duplex,
        mut proc: Box<dyn HopProcessor>,
        _cfg: PumpConfig,
        stats: &Stats,
        stop: &AtomicBool,
    ) -> Option<String> {
        let mut pump = Pump {
            input: [0.0; HOP],
            output: [0.0; HOP],
        };
        pump.drive(dev, proc.as_mut(), stats, stop)
    }

    fn drive(
        &mut self,
        dev: &mut dyn Duplex,
        proc: &mut dyn HopProcessor,
        stats: &Stats,
        stop: &AtomicBool,
    ) -> Option<String> {
        // Written once for the UI; a restart keeps the first label (the
        // counters belong to one stream).
        let _ = stats.engine_label.get_or_init(|| proc.describe());

        let mut filled = 0;
        loop {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            // A device may hand out any granularity, so top the hop up from
            // further reads: only a full hop is ever processed.
            while filled < HOP {
                match dev.read(&mut self.input[filled..]) {
                    Ok(0) => {
                        // Closed device: the partial hop is dropped, never
                        // zero-padded onto the render side.
                        stats.underruns.fetch_add(1, Ordering::Relaxed);
                        return Some(format!(
                            "{}: device closed after {filled} of {HOP} samples",
                            dev.describe()
                        ));
                    }
                    // Over-reports are clamped to the slice we handed out.
                    Ok(n) => filled = (filled + n).min(HOP),
                    Err(e) => return Some(format!("{}: capture read failed: {e}", dev.describe())),
                }
                if stop.load(Ordering::Acquire) {
                    return None;
                }
            }
            filled = 0;

            let started = Instant::now();
            proc.process(&self.input, &mut self.output);
            if started.elapsed() > HOP_PERIOD {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            stats.hops.fetch_add(1, Ordering::Relaxed);
            self.note_output_peak(stats);

            if let Err(e) = dev.write_hop(&self.output) {
                return Some(format!("{}: render write failed: {e}", dev.describe()));
            }
        }
    }

    /// Record the hop's peak, skipping digital silence: a muted hop must not
    /// overwrite the last real level the doctor shows.
    fn note_output_peak(&self, stats: &Stats) {
        let peak = self.output.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak > 0.0 {
            stats
                .peak_bits
                .store(dbfs(peak).to_bits(), Ordering::Relaxed);
        } else {
            stats.silence.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Amplitude to dBFS, floored at [`FLOOR_DBFS`] by clamping the amplitude.
fn dbfs(peak: f32) -> f32 {
    20.0 * peak.max(FLOOR_AMP).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Arc;
    use std::thread::sleep;

    /// A device that hands out pre-baked samples at a configurable
    /// granularity and records everything rendered into it.
    struct FakeDuplex {
        input: Vec<f32>,
        chunk: usize,
        pos: usize,
        written: Vec<f32>,
        fail_write: bool,
        fail_read: bool,
    }

    impl FakeDuplex {
        fn new(input: Vec<f32>) -> Self {
            FakeDuplex {
                input,
                chunk: usize::MAX,
                pos: 0,
                written: Vec::new(),
                fail_write: false,
                fail_read: false,
            }
        }

        fn chunked(mut self, chunk: usize) -> Self {
            self.chunk = chunk;
            self
        }
    }

    impl Duplex for FakeDuplex {
        fn read(&mut self, out: &mut [f32]) -> io::Result<usize> {
            if self.fail_read {
                return Err(io::Error::from_raw_os_error(5));
            }
            let avail = self.input.len() - self.pos;
            if avail == 0 {
                return Ok(0); // closed
            }
            let n = out.len().min(self.chunk).min(avail);
            out[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }

        fn write_hop(&mut self, hop: &[f32]) -> io::Result<()> {
            if self.fail_write {
                return Err(io::Error::from_raw_os_error(5));
            }
            self.written.extend_from_slice(hop);
            Ok(())
        }

        fn describe(&self) -> String {
            "fake mic -> fake cable".to_string()
        }
    }

    /// Identity times `gain`, with an optional stall to overrun the deadline.
    struct Gain {
        gain: f32,
        stall: Duration,
    }

    impl Gain {
        fn new(gain: f32) -> Self {
            Gain {
                gain,
                stall: Duration::ZERO,
            }
        }

        fn stalling(gain: f32, stall: Duration) -> Self {
            Gain { gain, stall }
        }
    }

    impl HopProcessor for Gain {
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            if !self.stall.is_zero() {
                sleep(self.stall);
            }
            for (o, i) in output.iter_mut().zip(input) {
                *o = i * self.gain;
            }
        }

        fn describe(&self) -> String {
            format!("fake gain x{}", self.gain)
        }
    }

    /// Stops the pump from inside the loop, the way a UI action would.
    struct Halt(Arc<AtomicBool>);

    impl HopProcessor for Halt {
        fn process(&mut self, _input: &[f32], _output: &mut [f32]) {
            self.0.store(true, Ordering::Release);
        }

        fn describe(&self) -> String {
            "halt".to_string()
        }
    }

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 * 0.125).collect()
    }

    #[test]
    fn default_config_is_full_attenuation_in_process_mode() {
        let cfg = PumpConfig::default();
        assert_eq!(cfg.attn_db, 100.0);
        assert_eq!(cfg.mode_control, 0.0);
        assert!(cfg.latency_report);
    }

    #[test]
    fn hop_period_is_one_hop_of_wall_time() {
        assert_eq!(HOP, 480);
        assert_eq!(HOP_PERIOD, Duration::from_millis(10));
    }

    #[test]
    fn a_preset_stop_flag_exits_before_reading() {
        let mut dev = FakeDuplex::new(vec![1.0; HOP * 2]);
        let stats = Stats::new();
        let stop = AtomicBool::new(true);

        let reason = Pump::run(
            &mut dev,
            Box::new(Gain::new(1.0)),
            PumpConfig::default(),
            &stats,
            &stop,
        );

        assert!(reason.is_none(), "clean stop has no reason");
        assert!(dev.written.is_empty());
        assert_eq!(stats.hops(), 0);
        assert_eq!(stats.engine(), "fake gain x1");
        assert_eq!(stats.peak_dbfs(), FLOOR_DBFS);
    }

    #[test]
    fn a_stop_flag_set_mid_stream_ends_the_loop_cleanly() {
        let mut dev = FakeDuplex::new(vec![1.0; HOP * 4]);
        let stats = Stats::new();
        let stop = Arc::new(AtomicBool::new(false));

        let reason = Pump::run(
            &mut dev,
            Box::new(Halt(stop.clone())),
            PumpConfig::default(),
            &stats,
            &stop,
        );

        assert!(reason.is_none());
        assert_eq!(stats.hops(), 1, "the hop in flight was finished");
        assert_eq!(dev.written.len(), HOP);
    }

    #[test]
    fn a_closed_device_is_reported_as_a_reason() {
        let mut dev = FakeDuplex::new(vec![1.0; 300]);
        let stats = Stats::new();

        let reason = Pump::run(
            &mut dev,
            Box::new(Gain::new(1.0)),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        )
        .expect("a closed device is a failure");

        assert!(reason.contains("closed"), "unexpected reason: {reason}");
        assert!(reason.contains("300 of 480"), "unexpected reason: {reason}");
        assert_eq!(stats.underruns(), 1);
        assert_eq!(stats.hops(), 0, "a partial hop is never processed");
        assert!(dev.written.is_empty());
    }

    #[test]
    fn partial_reads_reassemble_full_hops() {
        let input = ramp(HOP * 2);
        // 100 does not divide 480: hop boundaries fall inside a read.
        let mut dev = FakeDuplex::new(input.clone()).chunked(100);
        let stats = Stats::new();

        let reason = Pump::run(
            &mut dev,
            Box::new(Gain::new(1.0)),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        );

        assert!(reason.is_some(), "the fake runs dry and closes");
        assert_eq!(stats.hops(), 2);
        assert_eq!(stats.underruns(), 1);
        assert_eq!(dev.written, input, "sample order must survive the reads");
    }

    #[test]
    fn processor_output_reaches_the_device() {
        let mut dev = FakeDuplex::new(vec![1.0; HOP]);
        let stats = Stats::new();

        Pump::run(
            &mut dev,
            Box::new(Gain::new(0.5)),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        );

        assert_eq!(dev.written, vec![0.5; HOP]);
        // 20 * log10(0.5)
        assert!(
            (stats.peak_dbfs() + 6.0206).abs() < 1e-3,
            "peak was {}",
            stats.peak_dbfs()
        );
    }

    #[test]
    fn an_overrunning_hop_counts_as_dropped() {
        let mut dev = FakeDuplex::new(vec![0.25; HOP]);
        let stats = Stats::new();

        Pump::run(
            &mut dev,
            Box::new(Gain::stalling(1.0, Duration::from_millis(11))),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        );

        assert_eq!(stats.hops(), 1);
        assert_eq!(stats.dropped(), 1, "11 ms does not fit a 10 ms hop");
    }

    #[test]
    fn a_read_error_is_reported_as_a_reason() {
        let mut dev = FakeDuplex::new(vec![1.0; HOP]);
        dev.fail_read = true;
        let stats = Stats::new();

        let reason = Pump::run(
            &mut dev,
            Box::new(Gain::new(1.0)),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        )
        .expect("a failed capture is a failure");

        assert!(
            reason.contains("capture read failed"),
            "unexpected reason: {reason}"
        );
        assert_eq!(stats.hops(), 0);
        assert_eq!(stats.underruns(), 0, "an error is not a short read");
    }

    #[test]
    fn a_write_failure_is_reported_as_a_reason() {
        let mut dev = FakeDuplex::new(vec![1.0; HOP]);
        dev.fail_write = true;
        let stats = Stats::new();

        let reason = Pump::run(
            &mut dev,
            Box::new(Gain::new(1.0)),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        )
        .expect("a failed render is a failure");

        assert!(
            reason.contains("render write failed"),
            "unexpected reason: {reason}"
        );
        assert_eq!(stats.hops(), 1);
        assert!(dev.written.is_empty());
    }

    #[test]
    fn stats_track_hops_silence_and_peak() {
        let mut input = vec![1.0; HOP]; // 0 dBFS
        input.extend(vec![0.0; HOP]); // exact silence
        input.extend(vec![1e-9; HOP]); // below the floor
        let mut dev = FakeDuplex::new(input);
        let stats = Stats::new();

        Pump::run(
            &mut dev,
            Box::new(Gain::new(1.0)),
            PumpConfig::default(),
            &stats,
            &AtomicBool::new(false),
        );

        assert_eq!(stats.hops(), 3);
        assert_eq!(stats.silence_hops(), 1);
        assert_eq!(stats.underruns(), 1);
        assert_eq!(stats.dropped(), 0);
        // The silent hop was skipped, then the third clipped to the floor.
        assert!(
            (stats.peak_dbfs() - FLOOR_DBFS).abs() < 1e-3,
            "peak was {}",
            stats.peak_dbfs()
        );
        assert_eq!(dev.written.len(), HOP * 3);
    }
}
