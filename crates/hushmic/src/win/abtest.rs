//! Windows backend of the A/B window: the counterpart of the Linux
//! [`crate::abtest::audio`] module (`abtest::mod` re-exports this one as
//! `abtest::audio` on Windows, so `ui`/`state` are written once).
//!
//! Linux spawns two `pw-record` children — raw mic and `hushmic_source` — and
//! plays back through `pw-play`. Windows has neither: this module opens the
//! real microphone through the WASAPI duplex the chain uses and denoises each
//! hop in-process for the live panes. For the recorded take there is no node
//! to tap for a second, later-aligned leg, so the suppressed leg is DERIVED
//! offline by replaying the raw take through a fresh
//! [`dpdfnet_ladspa::Processor`]: same input, same engine, no tap inside the
//! chain, and the two legs can be aligned exactly (see
//! [`crate::abtest::sample`]).
//!
//! Audio state crosses to the UI only as [`Frame`]s, exactly as on Linux.
//!
//! Known cost, not a stub: [`WasapiDuplex::open`] always opens a render
//! endpoint too, so the window keeps a silent output stream alive while it
//! captures. A capture-only opener would have to live in `win/audio.rs`,
//! which this workstream does not own.

use crate::abtest::dsp::{LevelMeter, SpectrumAnalyzer};
use crate::abtest::metrics;
use crate::abtest::sample::{align_legs, levels, write_wav, DECLARED_LATENCY_SAMPLES};
use crate::abtest::types::{
    Channel, Command, Frame, LaunchGate, RECORD_SECS, SAMPLE_RATE,
};
use crate::abtest::watch::{capture_death_warning, silent_mic_hint, ZeroWatch};
use crate::config::{Config, Inference};
use crate::win::audio::{list_devices, WasapiDuplex};
use crate::win::chain::Assets;
use crate::win::{Duplex, HOP};
use dpdfnet_ladspa::Processor;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Media::Audio::{
    PlaySoundW, SND_FILENAME, SND_NODEFAULT, SND_PURGE, SND_SYNC,
};

/// The alignment this backend cuts between the legs must be the controller's
/// figure too, or the window would charge the comparison with a time offset.
const _: () = assert!(
    DECLARED_LATENCY_SAMPLES == dpdfnet_ladspa::align::PLUGIN_LATENCY_SAMPLES
        && DECLARED_LATENCY_SAMPLES == crate::win::control::LATENCY_SAMPLES as usize
);

/// One hop in wall time: the cadence the offline replay is paced at.
const HOP_PERIOD: Duration = Duration::from_micros(HOP as u64 * 1_000_000 / SAMPLE_RATE as u64);

/// How often the capture endpoint is re-probed.
const DEVICE_POLL: Duration = Duration::from_millis(1500);

/// How long a stopped capture thread is given to notice before it is left
/// detached.
const CAPTURE_JOIN: Duration = Duration::from_millis(1500);

/// Playback is declared done past this point even if the wave mapper never
/// reports completion: a sample is at most [`RECORD_SECS`] long.
const PLAYBACK_GRACE: f32 = 5.0;

/// The same surface as the Linux backend.
pub struct Backend {
    /// The configured microphone (a substring of the endpoint's friendly
    /// name); empty = the system default.
    capture: String,
    cmd_rx: Receiver<Command>,
    frame_tx: Sender<Frame>,
}

impl Backend {
    /// `_filtered_node`: Linux names the node carrying the processed stream;
    /// Windows derives the suppressed take from the capture, so there is no
    /// such node. `_gate`: Linux's launch blockers are "suppression is off"
    /// and "PipeWire is down" — neither applies to a window that runs its own
    /// engine, so a launch is never blocked here.
    pub fn new(
        raw_node: String,
        _filtered_node: String,
        _gate: LaunchGate,
        cmd_rx: Receiver<Command>,
        frame_tx: Sender<Frame>,
    ) -> Self {
        Backend {
            capture: raw_node,
            cmd_rx,
            frame_tx,
        }
    }

    /// Spawn the backend thread. `repaint` wakes the UI after every frame.
    pub fn start(self, repaint: Box<dyn Fn() + Send + Sync>) {
        std::thread::Builder::new()
            .name("abtest-backend".into())
            .spawn(move || run_backend(self, Arc::from(repaint)))
            .expect("spawn backend thread");
    }
}

/// The live capture thread: its stop flag and join handle.
struct Capture {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Shared {
    /// The take being recorded (raw capture only; the suppressed leg is
    /// derived once it is complete).
    rec: Mutex<Option<Take>>,
    /// The capture thread ended on its own (device gone, endpoint error).
    died: AtomicBool,
}

struct Take {
    raw: Vec<f32>,
    target: usize,
}

/// The last completed sample: aligned legs plus the WAVs playback uses.
struct Sample {
    raw: Vec<f32>,
    filtered: Vec<f32>,
    raw_wav: PathBuf,
    filtered_wav: PathBuf,
    /// The suppressed take is silence because the engine does not run: the
    /// window says so instead of presenting an all-zero comparison as a
    /// result.
    engine_silent: bool,
    /// The raw take is silence: the microphone delivered nothing, so no
    /// metric would mean anything (and blaming the engine would be a lie).
    mic_silent: bool,
}

struct Playback {
    done: Receiver<()>,
    started: Instant,
}

/// What the engine reads from the environment, resolved once per window: the
/// model tier and attenuation the chain would use for the configured mic.
///
/// Also the mic test's view of the engine ([`crate::win::mictest`] derives its
/// suppressed take the same way), so this is crate-visible.
pub(crate) struct EngineSpec {
    pub(crate) attn_db: f32,
    /// `None` = the assets resolved. `Some(reason)` = they did not; the
    /// engine then degrades to silence by design, and the reason is the file
    /// the doctor would also name.
    pub(crate) assets_error: Option<String>,
}

/// Open the window in its own process — what Linux's `spawn_child_window`
/// does for the daemon, here without the PipeWire preamble. Returns the child
/// pid so the caller can log it.
pub fn spawn_window() -> Result<u32, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("this executable's path is unknown: {e}"))?;
    let child = std::process::Command::new(exe)
        .arg("--test-window")
        .spawn()
        .map_err(|e| format!("the window process could not be started: {e}"))?;
    Ok(child.id())
}

fn run_backend(b: Backend, repaint: Arc<dyn Fn() + Send + Sync>) {
    let emit = |f: Frame| {
        let _ = b.frame_tx.send(f);
        repaint();
    };
    let shared = Arc::new(Shared {
        rec: Mutex::new(None),
        died: AtomicBool::new(false),
    });

    // The assets are published to the environment once, before any engine
    // exists (the window may run without a daemon).
    let spec = prepare_engine();
    if let Some(e) = &spec.assets_error {
        emit(Frame::Warn(format!(
            "{e} — the suppressed view stays silent (see `hushmic --doctor`)"
        )));
    }

    let mut monitors: Option<Capture> = None;
    let mut playback: Option<Playback> = None;
    let mut sample: Option<Sample> = None;
    let mut device_ok = device_present(&b.capture);
    let mut last_poll = Instant::now();
    emit(Frame::Device {
        ok: device_ok,
        name: b.capture.clone(),
    });

    loop {
        match b.cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Command::StartMonitor) => {
                if monitors.is_none() && device_ok {
                    monitors = spawn_capture(&b.capture, spec.attn_db, &shared, &b, &repaint);
                }
            }
            Ok(Command::Stop) => {
                if playback.take().is_some() {
                    stop_playback();
                    emit(Frame::PlaybackDone);
                } else if shared.rec.lock().unwrap().take().is_some() {
                    // Cancelled take: buffers only, the mic keeps running.
                    emit(Frame::RecordCancelled);
                } else {
                    stop_capture(&mut monitors);
                    emit(Frame::MonitorStopped);
                }
            }
            Ok(Command::Record) => {
                // The UI offers Record in the sample view, where the capture
                // is down: bring it back before arming.
                if monitors.is_none() && device_ok {
                    monitors = spawn_capture(&b.capture, spec.attn_db, &shared, &b, &repaint);
                }
                if monitors.is_some() && playback.is_none() {
                    let target = (RECORD_SECS * SAMPLE_RATE as f32) as usize;
                    *shared.rec.lock().unwrap() = Some(Take {
                        raw: Vec::with_capacity(target),
                        target,
                    });
                    emit(Frame::RecordStarted);
                }
            }
            Ok(Command::Play(ch)) => {
                // Refused while a take is armed (script-only race: the button
                // is disabled), because playing stops the capture and would
                // destroy the take while the UI still says Recording.
                if shared.rec.lock().unwrap().is_none() {
                    if let Some(s) = &sample {
                        // Playback is a sample-view action: the mic must not
                        // stay hot behind a sample review.
                        stop_capture(&mut monitors);
                        if playback.take().is_some() {
                            stop_playback();
                        }
                        if ch == Channel::Filtered {
                            if s.mic_silent {
                                emit(Frame::Warn(silent_mic_hint()));
                            } else if s.engine_silent {
                                emit(Frame::Warn(
                                    "the suppressed take is silent: the denoiser engine is not \
                                     running, so there is nothing to compare"
                                        .to_string(),
                                ));
                            }
                        }
                        let wav = match ch {
                            Channel::Raw => &s.raw_wav,
                            Channel::Filtered => &s.filtered_wav,
                        };
                        match spawn_play(wav) {
                            Ok(p) => playback = Some(p),
                            Err(e) => {
                                emit(Frame::Warn(format!("playback could not start: {e}")));
                                emit(Frame::PlaybackDone);
                            }
                        }
                    }
                }
            }
            Ok(Command::RetryDevice) => last_poll = Instant::now() - DEVICE_POLL,
            Err(RecvTimeoutError::Timeout) => {}
            // The window is gone.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // The capture thread ended on its own: tear down and let the device
        // poll re-probe. MonitorStopped is the UI's terminal frame — without
        // it the window would pulse "listening" forever.
        if shared.died.swap(false, Ordering::Relaxed) {
            let recording = shared.rec.lock().unwrap().is_some();
            stop_capture(&mut monitors);
            if recording {
                emit(Frame::Warn(crate::tr!("ab-toast-abort-stream")));
            }
            if let Some(msg) = capture_death_warning(recording, true) {
                emit(Frame::Warn(msg));
            }
            emit(Frame::MonitorStopped);
            last_poll = Instant::now() - DEVICE_POLL;
        }

        // Take completion: the capture filled the buffer.
        let finished = {
            let mut slot = shared.rec.lock().unwrap();
            match slot.as_ref() {
                Some(t) if t.raw.len() >= t.target => slot.take(),
                Some(t) => {
                    emit(Frame::RecordProgress {
                        secs_done: t.raw.len() as f32 / SAMPLE_RATE as f32,
                    });
                    None
                }
                None => None,
            }
        };
        if let Some(take) = finished {
            // The mic goes cold for the derivation: it costs about one take
            // length (see derive_processed) and the raw leg is complete. The
            // UI stays in Recording through it (progress at its end) — the
            // take is not finished and `MonitorStopped` here would show an
            // empty sample view for those seconds.
            stop_capture(&mut monitors);
            match store_take(take.raw, &spec) {
                Ok(s) => {
                    if s.mic_silent {
                        // The take is silence: no metric on zeros, and the
                        // honest cause is the microphone, not the engine.
                        emit(Frame::Warn(silent_mic_hint()));
                    } else if s.engine_silent {
                        // No metrics: an all-zero suppressed leg would report
                        // a record-setting background reduction.
                        emit(Frame::Warn(
                            "the suppressed take is silent: the denoiser engine is not running, \
                             so there is nothing to compare"
                                .to_string(),
                        ));
                    } else {
                        emit(Frame::Metrics(metrics::compute(&s.raw, &s.filtered)));
                    }
                    emit(Frame::RecordDone);
                    sample = Some(s);
                }
                Err(e) => {
                    emit(Frame::Warn(format!("the take could not be stored: {e}")));
                }
            }
            // The live capture is already down: this lands the UI in the
            // sample view now that the take (or its failure) is reported.
            emit(Frame::MonitorStopped);
        }

        // Playback progress/completion. The wave mapper plays the file on its
        // own thread; the wall clock is the playhead, as with pw-play.
        let mut playback_over = false;
        if let Some(p) = &playback {
            let secs = p.started.elapsed().as_secs_f32();
            let done = p.done.try_recv().is_ok();
            if done || secs > RECORD_SECS + PLAYBACK_GRACE {
                playback_over = true;
            } else {
                emit(Frame::PlaybackProgress { secs });
                if let Some(s) = &sample {
                    let end = ((secs * SAMPLE_RATE as f32) as usize).min(s.raw.len());
                    let start = end.saturating_sub(SAMPLE_RATE as usize / 10);
                    if end > start {
                        emit(Frame::Level {
                            raw_db: levels(&s.raw[start..end]).rms_db,
                            filtered_db: levels(&s.filtered[start..end]).rms_db,
                        });
                    }
                }
            }
        }
        if playback_over {
            stop_playback(); // a no-op when it already finished
            emit(Frame::PlaybackDone);
            playback = None;
        }

        // Device watch: a vanished mic raises the overlay, a returning one
        // clears it (state.rs restarts the live view itself).
        if last_poll.elapsed() >= DEVICE_POLL {
            last_poll = Instant::now();
            if let Some(fresh) = resolve_capture(&b.capture) {
                let ok = fresh.is_some();
                if ok != device_ok {
                    device_ok = ok;
                    emit(Frame::Device {
                        ok,
                        name: b.capture.clone(),
                    });
                }
            }
        }
    }

    stop_capture(&mut monitors);
    stop_playback();
    if let Some(s) = sample {
        remove_wav(&s.raw_wav);
        remove_wav(&s.filtered_wav);
    }
}

fn device_present(capture: &str) -> bool {
    resolve_capture(capture).map(|found| found.is_some()).unwrap_or(true)
}

/// `Some(Some(name))` = a live capture endpoint matches `needle` (a case
/// insensitive substring of its friendly name; empty = any, i.e. the default
/// endpoint the audio layer will pick); `Some(None)` = probed, nothing
/// matches; `None` = the probe itself failed (unknown, stay optimistic).
fn resolve_capture(needle: &str) -> Option<Option<String>> {
    let (captures, _) = list_devices().ok()?;
    let wanted = needle.trim().to_lowercase();
    if wanted.is_empty() {
        return Some(captures.into_iter().next());
    }
    Some(
        captures
            .into_iter()
            .find(|name| name.to_lowercase().contains(&wanted)),
    )
}

fn spawn_capture(
    capture: &str,
    attn_db: f32,
    shared: &Arc<Shared>,
    b: &Backend,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) -> Option<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let capture = capture.to_string();
    let shared = Arc::clone(shared);
    let frame_tx = b.frame_tx.clone();
    let repaint = Arc::clone(repaint);
    let for_thread = Arc::clone(&repaint);
    let stop_thread = Arc::clone(&stop);
    let handle = match std::thread::Builder::new()
        .name("abtest-win-capture".into())
        .spawn(move || capture_loop(capture, attn_db, shared, frame_tx, for_thread, stop_thread))
    {
        Ok(handle) => handle,
        Err(e) => {
            let _ = b
                .frame_tx
                .send(Frame::Warn(format!("the capture thread could not start: {e}")));
            repaint();
            return None;
        }
    };
    Some(Capture {
        stop,
        handle: Some(handle),
    })
}

fn capture_loop(
    capture: String,
    attn_db: f32,
    shared: Arc<Shared>,
    frame_tx: Sender<Frame>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    stop: Arc<AtomicBool>,
) {
    let emit = |f: Frame| {
        let _ = frame_tx.send(f);
        repaint();
    };
    let mut dev = match WasapiDuplex::open(&capture, "") {
        Ok(d) => d,
        Err(e) => {
            if !stop.load(Ordering::Relaxed) {
                emit(Frame::Warn(format!("the microphone could not be opened: {e}")));
                shared.died.store(true, Ordering::Relaxed);
            }
            return;
        }
    };
    // The same engine the chain runs, so the suppressed pane is what users
    // would hear.
    let mut core = new_engine();
    let mut spectrum = [SpectrumAnalyzer::new(), SpectrumAnalyzer::new()];
    let mut level = [LevelMeter::new(), LevelMeter::new()];
    let mut zero_raw = ZeroWatch::new();
    let mut zero_filtered = ZeroWatch::new();
    let mut hop = [0.0f32; HOP];
    let mut out = [0.0f32; HOP];
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // One engine hop, reassembled from whatever the device delivers.
        let mut filled = 0;
        while filled < HOP {
            match dev.read(&mut hop[filled..]) {
                Ok(0) => {
                    if !stop.load(Ordering::Relaxed) {
                        emit(Frame::Warn("the microphone stream ended".to_string()));
                        shared.died.store(true, Ordering::Relaxed);
                    }
                    return;
                }
                Ok(n) => filled += n,
                Err(e) => {
                    if !stop.load(Ordering::Relaxed) {
                        emit(Frame::Warn(format!("the microphone failed: {e}")));
                        shared.died.store(true, Ordering::Relaxed);
                    }
                    return;
                }
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
        }
        if zero_raw.feed(&hop) {
            emit(Frame::Warn(silent_mic_hint()));
        }
        core.process_hop(&hop, &mut out, attn_db, 0.0);
        // A dead engine is silent by design; say so only while the mic
        // delivers signal, or a switched-off mic would read as a broken
        // denoiser.
        if hop.iter().any(|s| *s != 0.0) && zero_filtered.feed(&out) {
            emit(Frame::Warn(
                "the denoiser produced only silence: the suppressed view stays silent"
                    .to_string(),
            ));
        }
        for (ch, samples) in [(Channel::Raw, &hop[..]), (Channel::Filtered, &out[..])] {
            for bins in spectrum[ch as usize].feed(samples) {
                emit(Frame::Spectrum {
                    ch,
                    bins: bins.to_vec(),
                });
            }
        }
        let raw_db = level[0].feed(&hop);
        let filtered_db = level[1].feed(&out);
        for (raw_db, filtered_db) in raw_db.into_iter().zip(filtered_db) {
            emit(Frame::Level { raw_db, filtered_db });
        }
        if let Some(take) = shared.rec.lock().unwrap().as_mut() {
            let room = take.target.saturating_sub(take.raw.len()).min(HOP);
            take.raw.extend_from_slice(&hop[..room]);
        }
    }
}

/// Stop the live capture. Only the owning thread can drop the duplex, which
/// is what releases a thread parked in a device read, so the join is bounded:
/// a wedged endpoint leaves a detached thread that dies with the process (the
/// same trade [`crate::win::chain`] makes).
fn stop_capture(monitors: &mut Option<Capture>) {
    if let Some(mut mon) = monitors.take() {
        mon.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = mon.handle.take() {
            let deadline = Instant::now() + CAPTURE_JOIN;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

/// A fresh engine with its activation handshake — silence when the model or
/// the runtime is missing, the established doctor-visible degradation.
fn new_engine() -> Processor {
    let mut core = Processor::new(SAMPLE_RATE as u64);
    core.activate();
    core
}

/// Resolve the model tier and attenuation for the configured microphone and
/// publish the engine's files to the environment it reads.
///
/// The profile key is the configured mic string: that is the key the tray and
/// the chain write, so the window compares with the same settings the user
/// hears.
pub(crate) fn prepare_engine() -> EngineSpec {
    let cfg = Config::load();
    let profile = cfg.profile_for(cfg.mic.as_deref());
    let assets_error = match Assets::resolve(&profile.model) {
        Ok(assets) => {
            publish_assets(&assets, cfg.inference);
            None
        }
        Err(e) => Some(e),
    };
    EngineSpec {
        attn_db: profile.attn_limit,
        assets_error,
    }
}

/// Publish the paths the engine reads from the environment.
///
/// shortcut: a second copy of `win::chain::install_env`, which is private to
/// a module this workstream does not own. The asset *rule* is shared —
/// [`Assets::resolve`], the same files the doctor names — and the variable
/// names are the engine's own (see `dpdfnet_ladspa`). One deliberate
/// difference: the config's inference mode is written unconditionally, so the
/// window's engine uses what the user configured instead of honouring a
/// pre-existing environment override. Upgrade when the chain exposes its
/// publisher.
fn publish_assets(assets: &Assets, inference: Inference) {
    std::env::set_var("HUSHMIC_MODEL_PATH", &assets.model);
    std::env::set_var("ORT_DYLIB_PATH", &assets.ort);
    match &assets.fallback {
        Some(path) => std::env::set_var("HUSHMIC_FALLBACK_MODEL_PATH", path),
        None => std::env::remove_var("HUSHMIC_FALLBACK_MODEL_PATH"),
    }
    std::env::set_var("HUSHMIC_INFERENCE", inference.as_str());
}

/// Turn a completed raw take into the playable sample: derive the suppressed
/// leg, align both, write their WAVs.
fn store_take(mut raw: Vec<f32>, spec: &EngineSpec) -> io::Result<Sample> {
    raw.truncate((RECORD_SECS * SAMPLE_RATE as f32) as usize);
    // A take with no assets gets zeros rather than an engine that silently
    // degrades: the window must never dress an all-zero leg up as a result.
    let processed = if spec.assets_error.is_some() {
        vec![0.0; raw.len()]
    } else {
        derive_processed(&raw, spec.attn_db)
    };
    let engine_silent = processed.iter().all(|s| *s == 0.0);
    let mic_silent = raw.iter().all(|s| *s == 0.0);
    // Cut the declared latency between the legs: the processed take starts
    // DECLARED_LATENCY_SAMPLES late, and a listener must not hear that offset
    // as a quality difference. Both legs are then the same instants of the
    // same input.
    let (raw_aligned, processed_aligned) = align_legs(&raw, &processed);
    let dir = work_dir()?;
    let raw_wav = dir.join(RAW_WAV);
    let filtered_wav = dir.join(CLEAN_WAV);
    write_wav(&raw_wav, raw_aligned)?;
    write_wav(&filtered_wav, processed_aligned)?;
    Ok(Sample {
        raw: raw_aligned.to_vec(),
        filtered: processed_aligned.to_vec(),
        raw_wav,
        filtered_wav,
        engine_silent,
        mic_silent,
    })
}

/// The suppressed leg of a take: replay it through a fresh engine, hop by
/// hop. A fresh [`Processor`] starts with empty rings, so the take gets the
/// same treatment a chain session would give it, and no other window's audio
/// can push samples through it.
///
/// shortcut: the engine has no drain/backpressure (a hop whose output the
/// worker has not produced yet is filled with zeros), so the replay is paced
/// at the chain's real-time hop cadence: a take costs about its own length.
/// Upgrade when the worker grows a bounded drain.
pub(crate) fn derive_processed(raw: &[f32], attn_db: f32) -> Vec<f32> {
    let mut core = new_engine();
    let mut out = vec![0.0f32; raw.len()];
    for (i, chunk) in raw.chunks(HOP).enumerate() {
        let mut input = [0.0f32; HOP];
        input[..chunk.len()].copy_from_slice(chunk);
        let mut hop_out = [0.0f32; HOP];
        // 0.0 = `RunMode::Suppress` on the plugin's mode port, the same value
        // the chain writes: the take is the user's configured treatment.
        core.process_hop(&input, &mut hop_out, attn_db, 0.0);
        let at = i * HOP;
        out[at..at + chunk.len()].copy_from_slice(&hop_out[..chunk.len()]);
        std::thread::sleep(HOP_PERIOD);
    }
    out
}

/// Play a WAV through the default output device (not a virtual cable: the
/// window is for the person sitting at the machine).
fn spawn_play(wav: &Path) -> io::Result<Playback> {
    let path = HSTRING::from(wav.to_string_lossy().to_string());
    let (tx, done) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("abtest-win-play".into())
        .spawn(move || {
            // SAFETY: a valid, NUL-terminated wide path; SND_FILENAME with no
            // module handle is the documented file form. SND_SYNC blocks this
            // thread until the file has played or was purged. The BOOL is
            // "started playing", which SND_SYNC has already answered.
            unsafe {
                let _ = PlaySoundW(&path, None, SND_FILENAME | SND_NODEFAULT | SND_SYNC);
            }
            let _ = tx.send(());
        })?;
    Ok(Playback {
        done,
        started: Instant::now(),
    })
}

/// Play a WAV and return once it has finished — the mic test's blocking form
/// of [`spawn_play`], with the same bounded wait the window uses.
pub(crate) fn play_blocking(wav: &Path) -> io::Result<()> {
    let p = spawn_play(wav)?;
    let wait = Duration::from_secs_f32(RECORD_SECS + PLAYBACK_GRACE);
    match p.done.recv_timeout(wait) {
        Ok(()) => Ok(()),
        Err(RecvTimeoutError::Timeout) => {
            stop_playback();
            Err(io::Error::other(
                "playback never finished — is an output device available?",
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("the playback thread died"))
        }
    }
}

/// Stop whatever this process is playing. `SND_PURGE` is process-wide and a
/// no-op with nothing playing.
fn stop_playback() {
    // SAFETY: the purge form touches no pointer and no file.
    unsafe {
        let _ = PlaySoundW(None::<&windows::core::PCWSTR>, None, SND_PURGE);
    }
}

/// WAVs live in a per-process temp directory: two windows on one machine must
/// not overwrite each other's take.
fn work_dir() -> io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("hushmic-abtest-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

const RAW_WAV: &str = "abtest-raw.wav";
const CLEAN_WAV: &str = "abtest-filtered.wav";

/// Remove this process's recordings. The backend thread does the same on its
/// way out, but that is detached and races process exit (it loses whenever a
/// sample is playing); the caller sweeps synchronously after the window
/// closes, exactly as the Linux `--test-window` branch does.
pub fn remove_recordings() {
    if let Ok(dir) = work_dir() {
        remove_wav(&dir.join(RAW_WAV));
        remove_wav(&dir.join(CLEAN_WAV));
        let _ = std::fs::remove_dir(&dir); // only succeeds when empty
    }
}

fn remove_wav(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir); // only succeeds when empty
    }
}
