//! The Windows processing chain: the counterpart of [`crate::controller`].
//!
//! Linux renders a PipeWire filter-chain config and spawns `pipewire -c` to
//! host it; the server owns the audio. Windows has no such server, so this
//! module owns the whole chain in-process instead: resolve the assets the
//! engine needs and the two endpoints it connects, then hand them to
//! [`Pump`](crate::win::pump::Pump) on one dedicated thread that captures the
//! real microphone, denoises each 480-sample hop with the same
//! [`dpdfnet_ladspa::Processor`] the LADSPA plugin drives, and renders the
//! result into a virtual cable's input endpoint.
//!
//! Everything the Linux controller did around the child has a counterpart
//! here: the asset preflight that names a missing file instead of failing
//! silently inside the host, the mic fallback when the saved device is gone,
//! the latency contract (3840 samples / 80 ms, see
//! [`crate::win::pump::LATENCY_MS`]) and a lock-free mode switch. The one
//! deliberate gap is `set_default`: Windows has no supported API to repoint
//! the system default input, so the chain records the gap in
//! [`Chain::describe`] instead of pretending.

use crate::config::{Config, Inference};
use crate::control::{mode_word, RunMode};
use crate::win::audio::{self, WasapiDuplex};
use crate::win::control::{LATENCY_SAMPLES, LIGHT_MODEL};
use crate::win::pump::{Pump, PumpConfig, Stats};
use crate::win::{Duplex, HopProcessor, HOP, RATE};
use dpdfnet_ladspa::Processor;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Endpoint-name fragments that identify a virtual cable's INPUT endpoint, in
/// preference order. Users install one of these; the chain renders into it and
/// the user picks the matching OUTPUT as their microphone.
const CABLE_NAMES: [&str; 3] = ["CABLE Input", "VoiceMeeter Input", "VB-Audio"];

/// The mode a stored control value names. The wire vocabulary itself
/// ([`RunMode`]) is shared with the control channel — the chain only keeps the
/// plugin's raw 0/1/2 in an atomic.
fn mode_of(control: u8) -> RunMode {
    match control {
        1 => RunMode::Bypass,
        2 => RunMode::Mute,
        _ => RunMode::Suppress,
    }
}

/// The engine's files, resolved the way a Windows install lays them out:
/// everything ships beside `hushmic.exe`, with the same env overrides a Linux
/// install prints (a debugging override always wins).
///
/// Public because the rule is not the chain's private business: the doctor and
/// `status` must name the same files the engine will actually load, and a second
/// copy of this rule is a copy that can drift.
pub struct Assets {
    pub model: PathBuf,
    /// The light model, only when this run can actually fall back (the main
    /// model is not already the light one and the file exists) — the engine
    /// degrades to raw audio instead when it cannot.
    pub fallback: Option<PathBuf>,
    pub ort: PathBuf,
}

/// A path-like environment override; an empty value counts as unset, exactly
/// like the engine's own readers treat it.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The directory `hushmic.exe` lives in — the install root everything else is
/// relative to. `None`-worthy failure only when the OS cannot name our own
/// binary.
fn exe_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .ok_or_else(|| {
            "cannot locate hushmic.exe, so the models and onnxruntime.dll cannot be found"
                .to_string()
        })
}

/// The directory the models load from: `HUSHMIC_MODEL_DIR`, else
/// `<exe_dir>\models`.
pub fn model_dir() -> Result<PathBuf, String> {
    match env_path("HUSHMIC_MODEL_DIR") {
        Some(dir) => Ok(dir),
        None => Ok(exe_dir()?.join("models")),
    }
}

/// The main model for `stem`: `HUSHMIC_MODEL_PATH` wins, else
/// `model_dir()/<stem>.onnx`.
pub fn model_path(stem: &str) -> Result<PathBuf, String> {
    match env_path("HUSHMIC_MODEL_PATH") {
        Some(path) => Ok(path),
        None => Ok(model_dir()?.join(format!("{stem}.onnx"))),
    }
}

/// The ONNX Runtime library: `ORT_DYLIB_PATH` wins, else
/// `<exe_dir>\onnxruntime.dll`.
pub fn ort_path() -> Result<PathBuf, String> {
    match env_path("ORT_DYLIB_PATH") {
        Some(path) => Ok(path),
        None => Ok(exe_dir()?.join("onnxruntime.dll")),
    }
}

/// The light model the engine may degrade to: `HUSHMIC_FALLBACK_MODEL_PATH`
/// wins, else the sibling `model_dir()/<light>.onnx` when it exists and is not
/// already the main model.
pub fn fallback_model_path(stem: &str) -> Option<PathBuf> {
    if let Some(path) = env_path("HUSHMIC_FALLBACK_MODEL_PATH") {
        return Some(path);
    }
    let light = model_dir().ok()?.join(format!("{LIGHT_MODEL}.onnx"));
    (stem != LIGHT_MODEL && light.exists()).then_some(light)
}

impl Assets {
    /// Resolve and preflight the assets for the model `stem` names.
    ///
    /// Preflighting matters as much here as on Linux: a missing model or
    /// `onnxruntime.dll` otherwise fails inside the engine, which degrades to
    /// silence, and the user gets a working-looking chain with no audio and no
    /// message. Naming the exact missing file is the whole point.
    pub fn resolve(stem: &str) -> Result<Assets, String> {
        let model = model_path(stem)?;
        let ort = ort_path()?;
        for (what, path, hint) in [
            ("model file", &model, "HUSHMIC_MODEL_DIR"),
            ("ONNX Runtime library", &ort, "ORT_DYLIB_PATH"),
        ] {
            if !path.exists() {
                return Err(format!(
                    "{what} not found at {} — place it next to hushmic.exe, or set {hint}",
                    path.display()
                ));
            }
        }
        Ok(Assets {
            model,
            fallback: fallback_model_path(stem),
            ort,
        })
    }
}

/// The `HUSHMIC_INFERENCE` this process started with, captured on first use —
/// before [`install_env`] ever writes it.
///
/// The Linux host passes the engine choice into each child's environment, so a
/// value the user exported survives untouched. Here the engine runs in our own
/// process, whose environment outlives a chain restart: remembering the
/// original is what keeps "the plugin's default is auto" (no value at all)
/// distinguishable from a value *we* set on an earlier run.
static INHERITED_INFERENCE: LazyLock<Option<OsString>> =
    LazyLock::new(|| std::env::var_os("HUSHMIC_INFERENCE").filter(|v| !v.is_empty()));

/// Publish the engine's assets and the configured engine choice.
///
/// The engine reads `HUSHMIC_MODEL_PATH`, `HUSHMIC_FALLBACK_MODEL_PATH` and
/// `ORT_DYLIB_PATH` from the environment before it loads anything. On Linux
/// those are the spawned child's environment; in-process they are the process's
/// own, which is why a value this run does not want must be *removed* rather
/// than merely left alone — a stale fallback from the previous chain would
/// otherwise outlive a model change.
///
/// Only called before the audio thread exists: `set_var` is not safe against
/// concurrent environment reads.
fn install_env(assets: &Assets, inference: Inference) {
    std::env::set_var("HUSHMIC_MODEL_PATH", &assets.model);
    std::env::set_var("ORT_DYLIB_PATH", &assets.ort);
    match &assets.fallback {
        Some(path) => std::env::set_var("HUSHMIC_FALLBACK_MODEL_PATH", path),
        None => std::env::remove_var("HUSHMIC_FALLBACK_MODEL_PATH"),
    }
    match INHERITED_INFERENCE.as_ref() {
        Some(value) => std::env::set_var("HUSHMIC_INFERENCE", value),
        // Only `onnx` needs saying: unset is the engine's `auto`.
        None if inference == Inference::Onnx => std::env::set_var("HUSHMIC_INFERENCE", "onnx"),
        None => std::env::remove_var("HUSHMIC_INFERENCE"),
    }
}

/// Case-insensitive substring match, the same rule the audio layer selects an
/// endpoint by.
fn name_matches(name: &str, needle: &str) -> bool {
    name.to_lowercase().contains(&needle.to_lowercase())
}

/// The capture endpoint to open, plus the friendly name it pins when the saved
/// mic is still present. An empty string is the system default capture.
///
/// A saved mic that vanished (unplugged, renamed, re-enumerated) must not fail
/// the start: the Linux chain drops it and follows the default, which reads as
/// "working on the default" instead of "broken chain", and mic recovery can
/// pin the device again once it returns.
fn resolve_capture(saved: Option<&str>, captures: &[String]) -> (String, Option<String>) {
    let Some(saved) = saved.filter(|s| !s.trim().is_empty()) else {
        return (String::new(), None);
    };
    match captures.iter().find(|name| name_matches(name, saved)) {
        Some(found) => (found.clone(), Some(found.clone())),
        None => {
            eprintln!(
                "[hushmic] saved microphone '{saved}' is not present — following the \
                 system default"
            );
            (String::new(), None)
        }
    }
}

/// The render endpoint to open: the virtual cable's INPUT side.
///
/// Order: an explicit request (`HUSHMIC_SINK`), then the known cables in the
/// order they are commonly installed. A missing cable is the one start failure
/// worth spelling out — nothing the user can configure in HushMic fixes it, so
/// the error names the driver to install *and* the endpoints we did find.
fn resolve_render(requested: Option<&str>, renders: &[String]) -> Result<String, String> {
    if let Some(want) = requested.filter(|s| !s.trim().is_empty()) {
        if let Some(found) = renders.iter().find(|name| name_matches(name, want)) {
            return Ok(found.clone());
        }
        eprintln!("[hushmic] the requested output device '{want}' is not present");
    }
    for want in CABLE_NAMES {
        if let Some(found) = renders.iter().find(|name| name_matches(name, want)) {
            return Ok(found.clone());
        }
    }
    Err(format!(
        "no virtual audio cable found — install VB-Audio Virtual Cable \
         (https://vb-audio.com/Cable/) or VoiceMeeter and restart HushMic, then select \
         its output as your microphone. Render devices found: {}",
        if renders.is_empty() {
            "(none)".to_string()
        } else {
            renders.join(", ")
        }
    ))
}

/// One hop in, one hop out: the pump's engine handle.
///
/// The attenuation limit is fixed per chain (a change restarts the chain, like
/// the Linux conf), but the mode is live: the UI thread stores it in an atomic
/// and the audio path only ever loads it, so a mode switch never allocates and
/// never waits on a lock the audio thread would have to take.
struct Engine {
    core: Processor,
    attn_db: f32,
    mode: Arc<AtomicU8>,
}

impl HopProcessor for Engine {
    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let mode = f32::from(self.mode.load(Ordering::Relaxed));
        self.core.process_hop(input, output, self.attn_db, mode);
    }

    fn describe(&self) -> String {
        self.core.engine_label()
    }
}

/// The running Windows chain: the pump thread, its stop flag, the live mode and
/// the counters a UI or the doctor polls.
pub struct Chain {
    stop: Arc<AtomicBool>,
    mode: Arc<AtomicU8>,
    stats: Arc<Stats>,
    thread: Option<JoinHandle<()>>,
    /// `capture -> render` as the audio layer names it, fixed per chain.
    devices: String,
    /// What the engine runs on, read once at start.
    engine: String,
    /// `set_default` was requested; Windows has no supported way to honour it.
    set_default: bool,
}

impl Chain {
    /// Resolve assets + devices, load the engine and start the pump thread.
    pub fn start(cfg: &Config) -> Result<Chain, String> {
        Chain::start_with(cfg, None)
    }

    /// [`Chain::start`] with an explicit render endpoint (the `HUSHMIC_SINK`
    /// override, or a doctor probe); `None` auto-detects the cable.
    pub fn start_with(cfg: &Config, sink: Option<&str>) -> Result<Chain, String> {
        let (captures, renders) = audio::list_devices()
            .map_err(|e| format!("could not read the audio device list: {e}"))?;
        let (capture, pinned) = resolve_capture(cfg.mic.as_deref(), &captures);
        // Settings follow the device in use (per-mic profiles). Only a pinned,
        // present mic can own one: while the chain follows the system default
        // Windows does not tell us which device that is, so the globals apply —
        // the same rule Linux follows when it cannot name the default source.
        let profile = cfg.profile_for(pinned.as_deref());
        let assets = Assets::resolve(&profile.model)?;
        let requested_sink = sink.map(str::to_string).or_else(|| {
            std::env::var("HUSHMIC_SINK")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
        let render = resolve_render(requested_sink.as_deref(), &renders)?;

        // Everything that can fail has failed by now: no worker thread and no
        // open endpoint is left behind by a bad start.
        install_env(&assets, cfg.inference);
        let mut dev = WasapiDuplex::open(&capture, &render)?;
        let devices = dev.describe();

        let mut core = Processor::new(RATE as u64);
        core.activate();
        let engine = core.engine_label();

        let stop = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU8::new(RunMode::default().control_value()));
        let stats = Arc::new(Stats::new());
        let pump_cfg = PumpConfig {
            attn_db: profile.attn_limit,
            mode_control: f32::from(RunMode::default().control_value()),
            latency_report: true,
        };
        let thread = {
            let (stop, mode, stats) = (Arc::clone(&stop), Arc::clone(&mode), Arc::clone(&stats));
            std::thread::Builder::new()
                .name("hushmic-chain".into())
                .spawn(move || {
                    let engine = Engine {
                        core,
                        attn_db: pump_cfg.attn_db,
                        mode,
                    };
                    if let Some(reason) =
                        Pump::run(&mut dev, Box::new(engine), pump_cfg, &stats, &stop)
                    {
                        // The watchdog respawns the chain on `running()` going
                        // false; the reason is what the log preserves for it.
                        eprintln!("[hushmic] the audio chain stopped: {reason}");
                    }
                })
                .map_err(|e| format!("could not start the audio thread: {e}"))?
        };

        eprintln!(
            "[hushmic] chain: {devices} (engine {engine}, attn {:.0} dB, {} samples latency)",
            profile.attn_limit, LATENCY_SAMPLES
        );
        Ok(Chain {
            stop,
            mode,
            stats,
            thread: Some(thread),
            devices,
            engine,
            set_default: cfg.set_default,
        })
    }

    /// Stop the pump thread and with it the devices it owns. Idempotent, and
    /// called by [`Drop`] so a quit tears the chain down like the Linux
    /// controller's `disable()`.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // The pump checks the flag between reads, so it normally ends within a
        // hop. A capture endpoint that stalls without reporting invalidation can
        // hold `read` blocked (the `Duplex` contract has no read timeout), and
        // the UI must not hang on that: detach the thread instead. Its devices
        // and engine go with the process.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            eprintln!(
                "[hushmic] the audio chain did not stop within 1 s (a stalled capture \
                 device); leaving it to exit on its own"
            );
        }
    }

    /// True while the pump thread is alive. The watchdog reads this exactly
    /// like the Linux `Controller::is_running`.
    pub fn running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Live counters (hops, overruns, underruns, silence, output peak). Plain
    /// atomics, so a UI thread may poll it while audio runs.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// One line for the log, the doctor and `status`: the endpoints actually in
    /// use, what the engine runs on, the mode, and the `set_default` gap.
    pub fn describe(&self) -> String {
        let ms = LATENCY_SAMPLES as f32 * 1000.0 / RATE as f32;
        let mut line = format!(
            "{} [{} Hz mono, {} hop, {} samples / {:.0} ms latency] engine {} mode {}",
            self.devices,
            RATE,
            HOP,
            LATENCY_SAMPLES,
            ms,
            self.engine,
            mode_word(Some(mode_of(self.mode.load(Ordering::Relaxed))))
        );
        if self.set_default {
            line.push_str(
                "; set_default cannot be honoured on Windows — there is no supported API to \
                 repoint the system default input; select the cable's output device as your \
                 microphone in Windows sound settings",
            );
        }
        line
    }

    /// Switch modes from any thread. Never touches the audio path under a lock.
    pub fn set_mode(&self, mode: RunMode) {
        self.mode.store(mode.control_value(), Ordering::Relaxed);
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        self.stop();
    }
}
