//! Windows audio host: the counterpart of the Linux PipeWire filter-chain.
//!
//! On Linux the PipeWire *server* runs the audio and `hushmic` only renders a
//! filter-chain config; on Windows the EXE is the host itself, so the same
//! processing contract is driven here in-process: the real microphone is
//! captured, denoised with the same [`dpdfnet_ladspa`] core the LADSPA plugin
//! uses, and rendered into a virtual cable's input device. Users select that
//! cable's output as their microphone.
//!
//! Only [`pump`] is platform-neutral: it talks to devices through [`Duplex`]
//! and to the engine through [`HopProcessor`], so its cadence and arbitration
//! logic is unit-tested on every host, not just Windows.

/// The DPDFNet models' only rate (see [`hushmic_denoiser::SAMPLE_RATE`]).
pub const RATE: u32 = 48_000;

/// Samples per processing hop (10 ms), matching the chain's pinned quantum.
pub const HOP: usize = 480;

/// One 48 kHz mono capture + render pair, blocking.
pub trait Duplex: Send {
    /// Block until at least one sample is available, then fill up to
    /// `out.len()`. Returns the number of samples stored; 0 only when the
    /// device is closed and no further audio will arrive.
    fn read(&mut self, out: &mut [f32]) -> std::io::Result<usize>;

    /// Block until all of `hop` has been rendered. Absorbing clock drift
    /// between the two devices is the implementation's job, never the
    /// pump's.
    fn write_hop(&mut self, hop: &[f32]) -> std::io::Result<()>;

    /// `capture -> render` labels, for logs and the doctor.
    fn describe(&self) -> String;
}

/// The per-hop denoiser the pump drives. Implemented by
/// [`dpdfnet_ladspa::Processor`]; the pump's tests use their own fakes.
pub trait HopProcessor: Send {
    /// Process exactly one [`HOP`]-long 48 kHz mono hop.
    fn process(&mut self, input: &[f32], output: &mut [f32]);

    /// Engine description for logs and the doctor.
    fn describe(&self) -> String;
}

pub mod pump;

#[cfg(windows)]
pub mod abtest;
#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod chain;
#[cfg(windows)]
pub mod control;
pub mod doctor;
#[cfg(windows)]
pub mod mictest;
#[cfg(windows)]
pub mod shell;
