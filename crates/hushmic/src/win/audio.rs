//! The two real audio endpoints, driven by one thread.
//!
//! Linux hands the graph to PipeWire; on Windows nobody does that on our
//! behalf, so this module owns the microphone and the virtual cable's input
//! directly through COM: one `IMMDeviceEnumerator`, one shared-mode
//! `IAudioClient` per flow, event callbacks, and a single wait on both event
//! handles. That single thread is the point. A wrapper crate (cpal and
//! friends) opens a callback thread per device and hides the ring between
//! them, so the drift between the mic's clock and the cable's clock can no
//! longer be measured, let alone corrected — and the pump above us must never
//! be the place where a sample is dropped. The COM surface we need is small
//! (`IMMDeviceEnumerator` -> `IMMDevice` -> `IAudioClient` -> capture/render
//! client), so it is used raw; [`Duplex`] is the only seam the pump sees.
//!
//! Processing format is fixed at 48 kHz mono f32 ([`RATE`]). The endpoints'
//! shared-mode mix formats are whatever Windows reports — f32/i16/i24/i32,
//! 1..=8 channels, 44.1/48/96 kHz — and every packet is converted per push:
//! average to mono, scale to f32, resample to [`RATE`] when needed.
//!
//! Drift is absorbed here and nowhere else. A [`RING_SAMPLES`] (400 ms) ring
//! sits between capture and render and is held at [`RING_TARGET`] (40 ms) of
//! occupancy. Leaving the +/- [`RING_WINDOW`] (20 ms) window around that
//! target trims or pads whole samples with a 1 ms linear crossfade, so the
//! splice is inaudible and the pump's hop cadence never changes. Correction
//! counts are kept as counters, never logged per event.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{Error, IUnknown, GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

use super::{Duplex, RATE};

/// Ring capacity between capture and render: 400 ms at [`RATE`].
const RING_SAMPLES: usize = 19_200;
/// Occupancy the drift absorber holds: 40 ms of absorbed jitter.
const RING_TARGET: usize = 1_920;
/// Occupancy deviation tolerated before a correction: 20 ms.
const RING_WINDOW: usize = 960;
/// Samples of linear crossfade per correction: 1 ms.
const CROSSFADE: usize = 48;
/// Longest single wait, so the stop flag is seen promptly.
const WAIT_MS: u32 = 20;
/// How long a blocking trait call sleeps before it re-checks the state.
const WAIT_SLICE: Duration = Duration::from_millis(100);

/// The two endpoints' shared-mode mix format GUIDs, compared as `GUID`s so the
/// module needs no KernelStreaming feature.
const SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// WASAPI capture + render pair; see the module docs.
pub struct WasapiDuplex {
    inner: Arc<Inner>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    setup: Setup,
}

impl WasapiDuplex {
    /// Open the capture device whose friendly name contains `capture_match`
    /// (empty = the system default capture device) and the render device
    /// whose friendly name contains `render_match` (empty = the default
    /// render device). Matching is case-insensitive.
    pub fn open(capture_match: &str, render_match: &str) -> Result<WasapiDuplex, String> {
        let inner = Arc::new(Inner::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, opened) = mpsc::channel::<Result<Setup, String>>();

        let thread = {
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            let capture_match = capture_match.to_string();
            let render_match = render_match.to_string();
            thread::Builder::new()
                .name("hushmic-wasapi".to_string())
                .spawn(move || run_session(&capture_match, &render_match, &inner, &stop, ready))
                .map_err(|error| format!("could not start the audio thread: {error}"))?
        };

        match opened.recv() {
            Ok(Ok(setup)) => Ok(WasapiDuplex {
                inner,
                stop,
                thread: Some(thread),
                setup,
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the audio thread exited before the devices were open".to_string())
            }
        }
    }
}

impl Duplex for WasapiDuplex {
    fn read(&mut self, out: &mut [f32]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| io_failure("the audio state lock is poisoned"))?;
        loop {
            if let Some(message) = state.error.as_deref() {
                return Err(io_failure(message));
            }
            if !state.captured.is_empty() {
                break;
            }
            if state.stopped {
                return Ok(0);
            }
            let (next, _) = self
                .inner
                .ready
                .wait_timeout(state, WAIT_SLICE)
                .map_err(|_| io_failure("the audio state lock is poisoned"))?;
            state = next;
        }
        let count = out.len().min(state.captured.len());
        for (slot, sample) in out.iter_mut().zip(state.captured.drain(..count)) {
            *slot = sample;
        }
        Ok(count)
    }

    fn write_hop(&mut self, hop: &[f32]) -> io::Result<()> {
        if hop.is_empty() {
            return Ok(());
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| io_failure("the audio state lock is poisoned"))?;
        if let Some(message) = state.error.as_deref() {
            return Err(io_failure(message));
        }
        if state.stopped {
            return Err(io_failure("the audio device was closed"));
        }
        // The device thread counts every 48 kHz sample it takes over into its
        // endpoint feed; the hop counts as rendered once that count has passed
        // its length. The feed itself stays one endpoint buffer (<= 10 ms)
        // wide, so a hop may be up to that far from being audible.
        let target = state.consumed + hop.len() as u64;
        state.pending.extend(hop.iter().copied());
        while state.consumed < target {
            if let Some(message) = state.error.as_deref() {
                return Err(io_failure(message));
            }
            if state.stopped {
                return Err(io_failure("the audio device was closed"));
            }
            let (next, _) = self
                .inner
                .ready
                .wait_timeout(state, WAIT_SLICE)
                .map_err(|_| io_failure("the audio state lock is poisoned"))?;
            state = next;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        let mut text = format!(
            "{} [{}] -> {} [{}]",
            self.setup.capture,
            self.setup.capture_format,
            self.setup.render,
            self.setup.render_format
        );
        if let Ok(state) = self.inner.state.lock() {
            if state.trims > 0 || state.pads > 0 {
                text.push_str(&format!(
                    " (ring trims {}, pads {})",
                    state.trims, state.pads
                ));
            }
        }
        text
    }
}

impl Drop for WasapiDuplex {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The device thread checks the flag every `WAIT_MS` and signals the
        // condvar on its way out, so a caller blocked in read/write_hop is
        // released before this join returns.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Active capture and render friendly names, for the doctor.
pub fn list_devices() -> Result<(Vec<String>, Vec<String>), String> {
    with_com(|| unsafe {
        let enumerator = create_enumerator()?;
        Ok((
            device_names(&enumerator, eCapture)?,
            device_names(&enumerator, eRender)?,
        ))
    })
}

/// The selected devices' shared-mode mix formats as text, for the doctor.
pub fn mix_formats(capture_match: &str, render_match: &str) -> Result<(String, String), String> {
    with_com(|| unsafe {
        let enumerator = create_enumerator()?;
        let capture = pick_device(&enumerator, eCapture, capture_match)?;
        let render = pick_device(&enumerator, eRender, render_match)?;
        Ok((mix_format_text(&capture)?, mix_format_text(&render)?))
    })
}

/// True when a render endpoint whose friendly name contains `needle` exists.
pub fn has_render(needle: &str) -> bool {
    let wanted = needle.to_lowercase();
    with_com(|| unsafe {
        let enumerator = create_enumerator()?;
        Ok(device_names(&enumerator, eRender)?
            .iter()
            .any(|name| name.to_lowercase().contains(&wanted)))
    })
    .unwrap_or(false)
}

// ---------------------------------------------------------------- threading

struct Setup {
    capture: String,
    render: String,
    capture_format: String,
    render_format: String,
}

/// Shared between the device thread and the pump. One lock is enough: the
/// device thread holds it only for the length of a packet copy.
struct Inner {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    /// Capture ring, 48 kHz mono.
    captured: VecDeque<f32>,
    /// Render queue, 48 kHz mono, drained by the device thread.
    pending: VecDeque<f32>,
    /// 48 kHz samples moved from `pending` into the render endpoint.
    consumed: u64,
    /// The ring was primed, so correction may run.
    primed: bool,
    stopped: bool,
    error: Option<String>,
    trims: u64,
    pads: u64,
}

impl Inner {
    fn new() -> Inner {
        Inner {
            state: Mutex::new(State {
                captured: VecDeque::with_capacity(RING_SAMPLES),
                ..State::default()
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "the audio state lock is poisoned".to_string())
    }

    /// Record a fatal device error; every blocked caller wakes up with it.
    fn fail(&self, message: String) {
        if let Ok(mut state) = self.state.lock() {
            if state.error.is_none() {
                state.error = Some(message);
            }
            state.stopped = true;
            self.ready.notify_all();
        }
    }

    /// The device thread is gone; blocked readers get end-of-stream.
    fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            self.ready.notify_all();
        }
    }
}

fn run_session(
    capture_match: &str,
    render_match: &str,
    inner: &Arc<Inner>,
    stop: &Arc<AtomicBool>,
    ready: mpsc::Sender<Result<Setup, String>>,
) {
    let _com = match Com::init() {
        Ok(com) => com,
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    // Declared after `_com`, so the COM objects below are released before
    // `CoUninitialize` runs.
    let mut session = match unsafe { Session::open(capture_match, render_match) } {
        Ok(session) => session,
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    if ready.send(Ok(session.setup())).is_err() {
        return;
    }
    match session.run(inner, stop) {
        Ok(()) => inner.finish(),
        Err(message) => inner.fail(message),
    }
}

/// Owns this thread's COM initialization.
struct Com {
    uninitialize: bool,
}

impl Com {
    fn init() -> Result<Com, String> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            return Ok(Com { uninitialize: true });
        }
        // Another part of the process may already have picked an apartment
        // (tray, shell integration). COM works either way; we only must not
        // pair someone else's init with our `CoUninitialize`.
        if result == RPC_E_CHANGED_MODE {
            return Ok(Com {
                uninitialize: false,
            });
        }
        Err(hr_message("CoInitializeEx", result))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

// ------------------------------------------------------------------- session

struct Session {
    capture_audio: IAudioClient,
    render_audio: IAudioClient,
    capture_client: IAudioCaptureClient,
    render_client: IAudioRenderClient,
    capture_event: Event,
    render_event: Event,
    capture_format: MixFormat,
    render_format: MixFormat,
    render_buffer: u32,
    capture_name: String,
    render_name: String,
    // Packet scratch and converters belong to the device thread and are reused
    // so no allocation happens on the event path.
    scratch: Vec<f32>,
    converted: Vec<f32>,
    render_input: Vec<f32>,
    /// Resampled render samples, consumed from `cooked_at` to the end.
    cooked: Vec<f32>,
    cooked_at: usize,
    capture_resampler: LinearResampler,
    render_resampler: LinearResampler,
}

impl Session {
    unsafe fn open(capture_match: &str, render_match: &str) -> Result<Session, String> {
        let enumerator = create_enumerator()?;
        let capture_device = pick_device(&enumerator, eCapture, capture_match)?;
        let render_device = pick_device(&enumerator, eRender, render_match)?;
        let capture_name = friendly_name(&capture_device)
            .unwrap_or_else(|_| "(unnamed capture device)".to_string());
        let render_name =
            friendly_name(&render_device).unwrap_or_else(|_| "(unnamed render device)".to_string());

        let capture_audio: IAudioClient = capture_device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| com_message("activating the capture device", error))?;
        let render_audio: IAudioClient = render_device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| com_message("activating the render device", error))?;

        let capture_mix = MixFormatMemory::read(&capture_audio)?;
        let render_mix = MixFormatMemory::read(&render_audio)?;
        let capture_format = capture_mix.format;
        let render_format = render_mix.format;

        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        capture_audio
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                0,
                0,
                capture_mix.pointer(),
                None,
            )
            .map_err(|error| com_message("initializing the capture stream", error))?;
        render_audio
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                0,
                0,
                render_mix.pointer(),
                None,
            )
            .map_err(|error| com_message("initializing the render stream", error))?;

        let capture_event = Event::new()?;
        let render_event = Event::new()?;
        capture_audio
            .SetEventHandle(capture_event.handle())
            .map_err(|error| com_message("arming the capture event", error))?;
        render_audio
            .SetEventHandle(render_event.handle())
            .map_err(|error| com_message("arming the render event", error))?;

        let capture_client = capture_audio
            .GetService::<IAudioCaptureClient>()
            .map_err(|error| com_message("acquiring the capture client", error))?;
        let render_client = render_audio
            .GetService::<IAudioRenderClient>()
            .map_err(|error| com_message("acquiring the render client", error))?;
        let render_buffer = render_audio
            .GetBufferSize()
            .map_err(|error| com_message("reading the render buffer size", error))?;

        capture_audio
            .Start()
            .map_err(|error| com_message("starting the capture stream", error))?;
        if let Err(error) = render_audio.Start() {
            let _ = capture_audio.Stop();
            return Err(com_message("starting the render stream", error));
        }

        Ok(Session {
            capture_audio,
            render_audio,
            capture_client,
            render_client,
            capture_event,
            render_event,
            capture_format,
            render_format,
            render_buffer,
            capture_name,
            render_name,
            scratch: Vec::with_capacity(4096),
            converted: Vec::with_capacity(8192),
            render_input: Vec::with_capacity(4096),
            cooked: Vec::with_capacity(8192),
            cooked_at: 0,
            capture_resampler: LinearResampler::new(capture_format.rate),
            render_resampler: LinearResampler::new(render_format.rate),
        })
    }

    fn setup(&self) -> Setup {
        Setup {
            capture: self.capture_name.clone(),
            render: self.render_name.clone(),
            capture_format: self.capture_format.describe(),
            render_format: self.render_format.describe(),
        }
    }

    fn run(&mut self, inner: &Arc<Inner>, stop: &Arc<AtomicBool>) -> Result<(), String> {
        let handles = [self.capture_event.handle(), self.render_event.handle()];
        let outcome = loop {
            if stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_MS) };
            if wait == WAIT_OBJECT_0 {
                if let Err(message) = self.capture(inner) {
                    break Err(message);
                }
            } else if wait.0 == WAIT_OBJECT_0.0 + 1 {
                if let Err(message) = self.render(inner) {
                    break Err(message);
                }
            } else if wait == WAIT_TIMEOUT {
                continue;
            } else {
                let code = unsafe { GetLastError() };
                break Err(format!(
                    "WaitForMultipleObjects returned {} (GetLastError {})",
                    wait.0, code.0
                ));
            }
        };
        // Best effort: a stopped endpoint reports errors we no longer care
        // about, and shutdown must not be blocked by them.
        unsafe {
            let _ = self.capture_audio.Stop();
            let _ = self.render_audio.Stop();
        }
        outcome
    }

    fn capture(&mut self, inner: &Arc<Inner>) -> Result<(), String> {
        loop {
            let frames = unsafe { self.capture_client.GetNextPacketSize() }
                .map_err(|error| com_message("reading the capture packet size", error))?;
            if frames == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut count = 0u32;
            let mut flags = 0u32;
            unsafe {
                self.capture_client
                    .GetBuffer(&mut data, &mut count, &mut flags, None, None)
            }
            .map_err(|error| com_message("reading a capture packet", error))?;

            let outcome = unsafe { self.convert_capture(inner, data, count, flags) };
            unsafe { self.capture_client.ReleaseBuffer(count) }
                .map_err(|error| com_message("releasing a capture packet", error))?;
            outcome?;
        }
    }

    unsafe fn convert_capture(
        &mut self,
        inner: &Arc<Inner>,
        data: *const u8,
        frames: u32,
        flags: u32,
    ) -> Result<(), String> {
        if frames == 0 {
            return Ok(());
        }
        let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
        self.scratch.clear();
        if silent {
            self.scratch.resize(frames as usize, 0.0);
        } else if data.is_null() {
            return Err("the capture packet has no data".to_string());
        } else {
            self.capture_format
                .read_mono(data, frames, &mut self.scratch);
        }
        self.converted.clear();
        self.capture_resampler
            .run(&self.scratch, &mut self.converted);

        let mut state = inner.lock()?;
        push_captured(&mut state, &self.converted);
        drop(state);
        inner.ready.notify_all();
        Ok(())
    }

    fn render(&mut self, inner: &Arc<Inner>) -> Result<(), String> {
        let padding = unsafe { self.render_audio.GetCurrentPadding() }
            .map_err(|error| com_message("reading the render padding", error))?;
        let frames = self.render_buffer.saturating_sub(padding) as usize;
        if frames == 0 {
            return Ok(());
        }

        // Always start from this event's samples: the previous `render_input`
        // was already consumed by the resampler.
        self.render_input.clear();
        {
            let mut state = inner.lock()?;
            if !state.pending.is_empty() {
                self.render_input.extend(state.pending.drain(..));
                state.consumed += self.render_input.len() as u64;
                inner.ready.notify_all();
            }
        }
        if !self.render_input.is_empty() {
            self.render_resampler
                .run(&self.render_input, &mut self.cooked);
        }

        let buffer = unsafe { self.render_client.GetBuffer(frames as u32) }
            .map_err(|error| com_message("acquiring a render buffer", error))?;
        if buffer.is_null() {
            return Err("GetBuffer returned a null render buffer".to_string());
        }
        let channels = self.render_format.channels as usize;
        let stride = self.render_format.frame_bytes();
        let mut written = 0usize;
        while written < frames && self.cooked_at < self.cooked.len() {
            let sample = self.cooked[self.cooked_at];
            self.cooked_at += 1;
            let base = unsafe { buffer.add(written * stride) };
            for channel in 0..channels {
                unsafe {
                    self.render_format
                        .write_sample(base.add(channel * self.render_format.kind.bytes()), sample)
                };
            }
            written += 1;
        }
        if self.cooked_at == self.cooked.len() {
            self.cooked.clear();
            self.cooked_at = 0;
        }
        if written < frames {
            // Underrun: the endpoint still needs the whole buffer written.
            unsafe {
                std::ptr::write_bytes(buffer.add(written * stride), 0, (frames - written) * stride)
            };
        }
        unsafe { self.render_client.ReleaseBuffer(frames as u32, 0) }
            .map_err(|error| com_message("releasing a render buffer", error))?;
        Ok(())
    }
}

/// Trim or pad the capture ring so the pump sees a constant 48 kHz cadence.
fn push_captured(state: &mut State, chunk: &[f32]) {
    if chunk.is_empty() {
        return;
    }

    let mut skip = 0usize;
    let mut pad = 0usize;
    let projected = state.captured.len() + chunk.len();
    if !state.primed {
        // Prefill: the first packet starts the absorbing buffer.
        state.primed = true;
        pad = RING_TARGET.saturating_sub(projected);
    } else if projected > RING_TARGET + RING_WINDOW {
        skip = (projected - RING_TARGET).min(chunk.len());
        state.trims += 1;
    } else if projected < RING_TARGET.saturating_sub(RING_WINDOW) {
        pad = RING_TARGET - projected;
        state.pads += 1;
    }

    if pad > 0 {
        // Bridge from the last queued sample to the incoming stream instead of
        // holding a DC level or starting with a step.
        let last = state.captured.back().copied().unwrap_or(0.0);
        let first = chunk.get(skip).copied().unwrap_or(0.0);
        for step in 1..=pad {
            let weight = step as f32 / (pad + 1) as f32;
            state.captured.push_back(last + (first - last) * weight);
        }
    }

    if skip > 0 {
        // Crossfade across the dropped samples so the splice does not click.
        let last = state.captured.back().copied().unwrap_or(0.0);
        let kept = &chunk[skip..];
        let fade = CROSSFADE.min(kept.len());
        for (offset, sample) in kept.iter().take(fade).enumerate() {
            let weight = (offset + 1) as f32 / (fade + 1) as f32;
            state
                .captured
                .push_back(last * (1.0 - weight) + sample * weight);
        }
        state.captured.extend(kept[fade..].iter().copied());
    } else {
        state.captured.extend(chunk.iter().copied());
    }

    // Only reachable when the pump stopped reading: keep the ring bounded.
    if state.captured.len() > RING_SAMPLES {
        let excess = state.captured.len() - RING_SAMPLES;
        state.captured.drain(..excess);
        state.trims += 1;
    }
}

// ------------------------------------------------------------------ formats

#[derive(Clone, Copy)]
struct MixFormat {
    rate: u32,
    channels: u16,
    kind: SampleKind,
}

/// PCM width to sample kind; shared by the plain and the extensible tag.
fn sample_kind(bits: u16) -> Result<SampleKind, String> {
    match bits {
        16 => Ok(SampleKind::I16),
        24 => Ok(SampleKind::I24),
        32 => Ok(SampleKind::I32),
        bits => Err(format!("unsupported PCM mix width: {bits} bits")),
    }
}

impl MixFormat {
    unsafe fn parse(raw: *const WAVEFORMATEX) -> Result<MixFormat, String> {
        // `WAVEFORMATEX` is `repr(packed)`, so every field is copied into a
        // local up front: referencing a packed field is a compile error.
        let base = *raw;
        let tag = base.wFormatTag;
        let channels = base.nChannels;
        let rate = base.nSamplesPerSec;
        let block_align = base.nBlockAlign;
        let bits = base.wBitsPerSample;
        let ext_size = base.cbSize;
        if channels == 0 || rate == 0 || block_align == 0 {
            return Err(format!(
                "unusable mix format: {rate} Hz, {channels} channels, block align {block_align}"
            ));
        }
        let kind = match tag {
            1 => sample_kind(bits)?,
            3 => SampleKind::F32,
            0xFFFE if ext_size >= 22 => {
                let ext = &*(raw as *const WAVEFORMATEXTENSIBLE);
                // `WAVEFORMATEXTENSIBLE` is packed too: read the GUID by value.
                let subformat = ext.SubFormat;
                if subformat == SUBTYPE_IEEE_FLOAT {
                    SampleKind::F32
                } else if subformat == SUBTYPE_PCM {
                    sample_kind(bits)?
                } else {
                    return Err(format!(
                        "unsupported mix subformat {}",
                        format_guid(&subformat)
                    ));
                }
            }
            0xFFFE => {
                return Err(
                    "the extensible mix format carries no subformat (cbSize < 22)".to_string(),
                )
            }
            tag => return Err(format!("unsupported mix format tag 0x{tag:04X}")),
        };
        let container = block_align as usize / channels as usize;
        if container != kind.bytes() {
            return Err(format!(
                "mix format claims {bits}-bit {} samples but packs {container} bytes",
                kind.label()
            ));
        }
        Ok(MixFormat {
            rate,
            channels,
            kind,
        })
    }

    fn describe(&self) -> String {
        format!(
            "{} Hz, {} ch, {}",
            self.rate,
            self.channels,
            self.kind.label()
        )
    }

    fn frame_bytes(&self) -> usize {
        self.channels as usize * self.kind.bytes()
    }

    /// Average every frame to one mono f32 sample.
    unsafe fn read_mono(&self, data: *const u8, frames: u32, out: &mut Vec<f32>) {
        let channels = self.channels as usize;
        let width = self.kind.bytes();
        let stride = self.frame_bytes();
        out.reserve(frames as usize);
        for frame in 0..frames as usize {
            let base = data.add(frame * stride);
            let mut sum = 0.0f32;
            for channel in 0..channels {
                sum += read_sample(self.kind, base.add(channel * width));
            }
            out.push(sum / channels as f32);
        }
    }

    /// Write one mono f32 sample into a device sample slot, duplicated across
    /// all channels.
    unsafe fn write_sample(&self, target: *mut u8, value: f32) {
        let clamped = value.clamp(-1.0, 1.0);
        match self.kind {
            SampleKind::F32 => std::ptr::write_unaligned(target as *mut f32, clamped),
            SampleKind::I16 => {
                std::ptr::write_unaligned(target as *mut i16, (clamped * 32_767.0) as i16)
            }
            SampleKind::I32 => {
                std::ptr::write_unaligned(target as *mut i32, (clamped * 2_147_483_647.0) as i32)
            }
            SampleKind::I24 => {
                // 24-in-32 is left-justified: the sample lives in bits 8..32.
                let packed = ((clamped * 8_388_607.0) as i32) << 8;
                std::ptr::write_unaligned(target as *mut i32, packed.to_le())
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SampleKind {
    F32,
    I16,
    I32,
    I24,
}

impl SampleKind {
    fn bytes(self) -> usize {
        match self {
            SampleKind::I16 => 2,
            _ => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SampleKind::F32 => "f32",
            SampleKind::I16 => "i16",
            SampleKind::I32 => "i32",
            SampleKind::I24 => "i24",
        }
    }
}

unsafe fn read_sample(kind: SampleKind, data: *const u8) -> f32 {
    match kind {
        SampleKind::F32 => {
            f32::from_bits(u32::from_le(std::ptr::read_unaligned(data as *const u32)))
        }
        SampleKind::I16 => {
            i16::from_le(std::ptr::read_unaligned(data as *const i16)) as f32 / 32_768.0
        }
        SampleKind::I32 => {
            i32::from_le(std::ptr::read_unaligned(data as *const i32)) as f32 / 2_147_483_648.0
        }
        SampleKind::I24 => {
            (i32::from_le(std::ptr::read_unaligned(data as *const i32)) >> 8) as f32 / 8_388_608.0
        }
    }
}

/// Linear-interpolation rate converter to [`RATE`].
///
/// `position` and `previous` carry the read position and the last input sample
/// across calls, so a packet boundary is not an interpolation boundary.
///
/// shortcut: linear interpolation has no anti-alias filter; 96 kHz endpoint
/// mixes therefore let content above 24 kHz fold back. Upgrade to a polyphase
/// windowed-sinc when the cable's mix rate is 96 kHz on real hardware.
struct LinearResampler {
    step: f64,
    position: f64,
    previous: f32,
}

impl LinearResampler {
    fn new(rate: u32) -> LinearResampler {
        LinearResampler {
            step: rate as f64 / RATE as f64,
            position: 0.0,
            previous: 0.0,
        }
    }

    fn run(&mut self, input: &[f32], output: &mut Vec<f32>) {
        let count = input.len();
        if count == 0 {
            return;
        }
        if self.step == 1.0 {
            output.extend_from_slice(input);
            self.previous = input[count - 1];
            return;
        }
        let last = (count - 1) as f64;
        let mut position = self.position;
        while position < last {
            let index = position.floor() as isize;
            let fraction = (position - index as f64) as f32;
            let before = if index < 0 {
                self.previous
            } else {
                input[index as usize]
            };
            let after = input[(index + 1) as usize];
            output.push(before + (after - before) * fraction);
            position += self.step;
        }
        self.previous = input[count - 1];
        self.position = position - count as f64;
    }
}

// --------------------------------------------------------------- COM helpers

/// The mix format as reported by the endpoint, kept alive until `Initialize`
/// has copied it.
struct MixFormatMemory {
    format: MixFormat,
    raw: *mut WAVEFORMATEX,
}

impl MixFormatMemory {
    unsafe fn read(client: &IAudioClient) -> Result<MixFormatMemory, String> {
        let raw = client
            .GetMixFormat()
            .map_err(|error| com_message("reading the shared-mode mix format", error))?;
        if raw.is_null() {
            return Err("GetMixFormat returned a null format".to_string());
        }
        let format = match MixFormat::parse(raw) {
            Ok(format) => format,
            Err(message) => {
                CoTaskMemFree(Some(raw as *const c_void));
                return Err(message);
            }
        };
        Ok(MixFormatMemory { format, raw })
    }

    fn pointer(&self) -> *const WAVEFORMATEX {
        self.raw as *const WAVEFORMATEX
    }
}

impl Drop for MixFormatMemory {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.raw as *const c_void)) };
    }
}

/// An auto-reset Win32 event, closed on drop.
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Event, String> {
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|error| com_message("CreateEventW", error))?;
        Ok(Event(handle))
    }

    fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn with_com<T>(body: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _com = Com::init()?;
    body()
}

unsafe fn create_enumerator() -> Result<IMMDeviceEnumerator, String> {
    CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
        .map_err(|error| com_message("creating the audio device enumerator", error))
}

unsafe fn pick_device(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    needle: &str,
) -> Result<IMMDevice, String> {
    if needle.trim().is_empty() {
        return enumerator
            .GetDefaultAudioEndpoint(flow, eConsole)
            .map_err(|error| {
                com_message(
                    &format!("selecting the default {} device", flow_name(flow)),
                    error,
                )
            });
    }
    let collection = enumerator
        .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
        .map_err(|error| com_message(&format!("enumerating {} devices", flow_name(flow)), error))?;
    let count = collection
        .GetCount()
        .map_err(|error| com_message("counting audio devices", error))?;
    let wanted = needle.to_lowercase();
    let mut available = Vec::new();
    for index in 0..count {
        let device = collection
            .Item(index)
            .map_err(|error| com_message("reading an audio device", error))?;
        let Ok(name) = friendly_name(&device) else {
            continue;
        };
        if name.to_lowercase().contains(&wanted) {
            return Ok(device);
        }
        available.push(name);
    }
    Err(format!(
        "no {} device matching {:?}; available: {}",
        flow_name(flow),
        needle,
        available.join(", ")
    ))
}

unsafe fn device_names(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> Result<Vec<String>, String> {
    let collection = enumerator
        .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
        .map_err(|error| com_message(&format!("enumerating {} devices", flow_name(flow)), error))?;
    let count = collection
        .GetCount()
        .map_err(|error| com_message("counting audio devices", error))?;
    let mut names = Vec::new();
    for index in 0..count {
        let device = collection
            .Item(index)
            .map_err(|error| com_message("reading an audio device", error))?;
        if let Ok(name) = friendly_name(&device) {
            names.push(name);
        }
    }
    Ok(names)
}

unsafe fn friendly_name(device: &IMMDevice) -> Result<String, String> {
    let store = device
        .OpenPropertyStore(STGM_READ)
        .map_err(|error| com_message("opening the device property store", error))?;
    let value = store
        .GetValue(&PKEY_Device_FriendlyName)
        .map_err(|error| com_message("reading the device friendly name", error))?;
    let name = PropVariantToBSTR(&value)
        .map_err(|error| com_message("converting the device friendly name", error))?;
    Ok(name.to_string())
}

unsafe fn mix_format_text(device: &IMMDevice) -> Result<String, String> {
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|error| com_message("activating an audio device", error))?;
    Ok(MixFormatMemory::read(&client)?.format.describe())
}

fn flow_name(flow: EDataFlow) -> &'static str {
    if flow == eCapture {
        "capture"
    } else {
        "render"
    }
}

fn format_guid(guid: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7]
    )
}

fn com_message(what: &str, error: Error) -> String {
    format!("{what}: {error} (HRESULT 0x{:08X})", error.code().0 as u32)
}

fn hr_message(what: &str, result: HRESULT) -> String {
    format!("{what}: HRESULT 0x{:08X}", result.0 as u32)
}

fn io_failure(message: &str) -> io::Error {
    io::Error::other(message.to_string())
}
