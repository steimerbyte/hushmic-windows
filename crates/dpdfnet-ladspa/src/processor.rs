//! The portable DSP core: one hop of mono 48 kHz DPDFNet denoising.
//!
//! Both hosts of this crate drive this type — the LADSPA plugin in `lib.rs`
//! and the Windows in-process audio host — so the per-hop contract (the
//! worker handshake, ring pacing, the overflow restart, the silence
//! degradation, the control-value caching) exists exactly once. Nothing
//! here touches LADSPA or libc, so it compiles on every platform the crate
//! ships to.

use crate::align::Aligner;
use crate::worker::WorkerHandle;
use std::time::Duration;

/// The label for "there is no engine": failed init, wrong rate, or an
/// unspawnable worker thread.
const SILENT: &str = "silent";

pub struct Processor {
    /// The RT-side handle to the inference worker that owns the engine
    /// (issue #10: inference must never run on the audio data thread).
    /// `None` = failed init / wrong rate / unspawnable thread: the
    /// established working-but-silent degradation.
    worker: Option<WorkerHandle>,
    aligner: Aligner,
    /// The activation handshake succeeded; false degrades to silence.
    active: bool,
    /// Mid-stream recovery after an input-ring overflow: silence until
    /// the worker acks the reset, then resume fresh (see worker.rs).
    restarting: bool,
    // Control-value caches as bit patterns (NaN-proof compares).
    last_db_bits: u32,
    last_mode_bits: u32,
    /// What the engine runs on (`native w7a8 avx2`, `onnx`, `silent`), read
    /// once at init: the engine itself moves into the worker thread.
    label: String,
}

impl Processor {
    /// The DPDFNet models (and the N_FFT/HOP constants) are 48 kHz-only. A
    /// mismatched host gets the same degradation as a failed engine init —
    /// a working-but-silent stream, because audibly wrong beats subtly
    /// wrong enhancement.
    pub fn new(sample_rate: u64) -> Processor {
        let engine = if sample_rate != 48_000 {
            eprintln!(
                "[dpdfnet-ladspa] unsupported sample rate {sample_rate} (need 48000); \
                 emitting silence"
            );
            None
        } else {
            crate::init_engine()
        };
        let (worker, label) = match engine {
            Some((engine, label)) => (WorkerHandle::spawn(engine), label),
            None => (None, SILENT.to_string()),
        };
        // A refused thread spawn leaves no engine behind, whatever was
        // loaded before it.
        let label = if worker.is_some() {
            label
        } else {
            SILENT.to_string()
        };
        Processor {
            worker,
            aligner: Aligner::new(),
            active: false,
            restarting: false,
            last_db_bits: f32::NAN.to_bits(),
            last_mode_bits: f32::NAN.to_bits(),
            label,
        }
    }

    /// Fresh session: reset the engine (via the worker handshake — the host
    /// calls this before streaming, so a bounded wait is fine here; a stuck
    /// worker degrades to silence and the next activation retries), drop
    /// stale audio, restart the alignment ledger.
    pub fn activate(&mut self) {
        self.aligner.reset();
        self.restarting = false;
        self.last_db_bits = f32::NAN.to_bits();
        self.last_mode_bits = f32::NAN.to_bits(); // force a set on first hop
        self.active = match self.worker.as_mut() {
            Some(w) => {
                let ok = w.request_reset_and_wait(Duration::from_secs(2));
                if ok {
                    w.drain_output();
                } else if !w.engine_dead() {
                    eprintln!("[dpdfnet-ladspa] worker did not ack activation; emitting silence");
                }
                ok
            }
            None => false,
        };
    }

    /// One hop. `input` and `output` are the same length; `attn_db` and
    /// `mode_control` are the raw control values (mode: 0 process, 1
    /// latency-aligned bypass, 2 mute, rounded by the worker).
    pub fn process_hop(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        attn_db: f32,
        mode_control: f32,
    ) {
        // A host that under-sizes the output must not panic the data path;
        // equal lengths are the contract and behave exactly as before.
        let sample_count = input.len().min(output.len());
        // RT contract: nothing below allocates, locks, logs or panics:
        // ring pushes/pops, atomics, and one futex wake at most. The
        // degradation paths (engine death, input overflow) are reported
        // by the worker thread from flags set here.
        let w = match self.worker.as_mut() {
            Some(w) if self.active => w,
            _ => {
                emit_silence(output, sample_count);
                return;
            } // silent node: failed init, wrong rate, or failed activation
        };
        if w.engine_dead() {
            emit_silence(output, sample_count);
            return;
        }
        if attn_db.to_bits() != self.last_db_bits {
            w.set_attn_db(attn_db);
            self.last_db_bits = attn_db.to_bits();
        }
        if mode_control.to_bits() != self.last_mode_bits {
            w.set_mode_control(mode_control);
            self.last_mode_bits = mode_control.to_bits();
        }
        if self.restarting {
            // Overflow recovery: silence until the worker acks the reset.
            // ORDER MATTERS: observe the ack BEFORE the final drain —
            // every worker push happens-before the ack store, so a drain
            // after an observed ack removes any stale partial hop, and
            // the worker stays parked (empty input ring) until fresh
            // input is pushed below. Draining first would let a push
            // landing in the drain->ack window survive into the fresh
            // stream: a stale blip plus permanent latency drift.
            // activate() follows the same ack-then-drain discipline.
            if w.reset_acked() {
                w.drain_output();
                self.restarting = false; // fall through: stream fresh audio
            } else {
                w.drain_output(); // housekeeping: bound ring occupancy meanwhile
                emit_silence(output, sample_count);
                return;
            }
        }
        let accepted = w.push_input(&input[..sample_count]);
        if accepted < sample_count {
            // Input ring full: the worker is >680 ms behind. Restart the
            // stream — resync the mic to now instead of replaying stale
            // audio into a live call (see worker.rs / the design doc).
            w.request_reset();
            self.restarting = true;
            self.aligner.reset();
            w.drain_output();
            emit_silence(output, sample_count);
            return;
        }
        w.note_quantum(sample_count);
        w.wake();
        let plan = self.aligner.plan(sample_count, w.output_available());
        for _ in 0..plan.discard {
            let _ = w.pop_output();
        }
        let mut i = 0;
        for _ in 0..plan.lead_zeros {
            output[i] = 0.0;
            i += 1;
        }
        for _ in 0..plan.real {
            output[i] = w.pop_output().unwrap_or(0.0);
            i += 1;
        }
        for _ in 0..plan.tail_zeros {
            output[i] = 0.0;
            i += 1;
        }
    }

    /// What the hop runs on, for logs and the Windows doctor: the inference
    /// contract word (`native w7a8 avx2`, `onnx`) or `silent`.
    pub fn engine_label(&self) -> String {
        self.label.clone()
    }
}

/// Zero the whole output range (the silent-node degradation).
fn emit_silence(output: &mut [f32], sample_count: usize) {
    for o in output[..sample_count].iter_mut() {
        *o = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_rate_is_working_but_silent() {
        let mut p = Processor::new(44_100);
        assert_eq!(p.engine_label(), "silent");
        p.activate();
        let input = [1.0f32; 8];
        let mut output = [1.0f32; 8];
        p.process_hop(&input, &mut output, 100.0, 0.0);
        assert_eq!(output, [0.0; 8], "the silent degradation is total");
    }
}
