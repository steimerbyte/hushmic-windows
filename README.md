# HushMic for Windows

An unofficial Windows port of [HushMic](https://github.com/Fovty/hushmic): real-time microphone noise suppression that runs entirely on the CPU, with the application hosting the audio itself instead of a sound server.

> **Unverified.** This port type-checks and links for `x86_64-pc-windows-msvc`, but it has never been run on Windows hardware. Treat it as unreleased code. "Verified" would mean `hushmic.exe --doctor` printing `no problems found` on a real machine, plus a call in which the far end reports clean voice; neither has happened.
>
> **Unofficial.** Not affiliated with or endorsed by the HushMic authors. Bugs in this build belong here, not upstream.
>
> **No binaries.** This repository publishes no release artifacts; build from source below.
>
> **No build badge.** None is shown because the inherited CI was red when this README was written — `fmt + clippy + test` stops on `cargo fmt --check`, and `packaging smoke` fails — even though the `windows check (x86_64-pc-windows-msvc)` job passes. Add a badge only when a run is actually green.

## What this fork changes

Everything below is this fork's Windows layer. On Windows there is no PipeWire server to run a filter chain, so `hushmic.exe` becomes the audio host:

- **Capture → denoise → render.** WASAPI captures your microphone, mixes to mono, resamples to 48 kHz, processes 10 ms hops (480 samples) through the same DPDFNet engine the Linux plugin drives, and renders the result into a virtual cable's input device, resampling again if that endpoint's mix rate differs. Declared algorithmic latency is 80 ms (3840 samples) — the same figure the Linux chain reports.
- **The engine is reused, not reimplemented.** `dpdfnet-ladspa`'s portable core (inference worker, latency aligner, quality/light/passthrough tier ladder) is compiled into the EXE and driven in-process. The LADSPA ABI itself is Unix-only and absent here, so ONNX Runtime is the only inference engine on Windows.
- **Tray icon via `Shell_NotifyIcon`**: state icons for on/off/bypass/mute/error, and notifications through the same call (toasts on Windows 10+).
- **Control channel over a per-user named pipe** (`\\.\pipe\hushmic-<user>`), so every `status`, `mode`, `toggle`, `config` and `quit` works from any terminal without a second daemon.
- **Global hotkeys through `RegisterHotKey`**, registered on the tray's message window.
- **Start with Windows through the `HKCU` `Run` key** instead of a systemd unit or XDG autostart file.
- **`--doctor`**, implemented for this platform: device list, cable detection, the exact asset paths the engine will load, the `Run` entry, and whether an instance or a chain is running. It works with no daemon and no cable installed, in which case the report says so and exits 1.
- **`--test-window`**, an A/B window with live raw/cleaned panes and a recorded 10 s take. The suppressed leg is derived offline by replaying the raw take through a fresh engine, so both legs align exactly. When no window can open, the tray's mic test falls back to a 5 s audio-only recording.

## Requirements

| Item | Why |
| --- | --- |
| Windows on x86-64 | The only target this fork builds for. |
| [Microsoft Visual C++ 2015–2022 Redistributable](https://aka.ms/vs/17/release/vc_redist.x64.exe) | The MSVC build imports `VCRUNTIME140.dll`, which Windows does not ship. |
| VB-CABLE or VoiceMeeter | Supplies the virtual cable the processed audio is rendered into. |
| [ONNX Runtime 1.27.0 (x64)](https://github.com/microsoft/onnxruntime/releases/tag/v1.27.0) + DPDFNet models | `onnxruntime.dll` and `dpdfnet8_48khz_hr.onnx` (default) plus `dpdfnet2_48khz_hr.onnx` (light tier), all beside `hushmic.exe`. |

HushMic renders into the first render device whose name contains `CABLE Input`, `VoiceMeeter Input` or `VB-Audio`, in that order. `HUSHMIC_SINK=<part of a device name>` overrides the search.

## Build

```bash
scripts/setup-assets.sh        # fetch the hash-pinned models (same files Linux ships)
                               # then put onnxruntime.dll in assets/win/, or set HUSHMIC_ORT_DLL
./scripts/build-windows.sh     # also on Windows, under Git Bash
```

[`scripts/build-windows.sh`](scripts/build-windows.sh) cross-compiles with `cargo xwin` for the MSVC target on Linux (no Windows host needed) and is a plain `cargo build` under Git Bash on Windows. It stages a directory that can be zipped and unpacked anywhere:

```
dist/hushmic-windows-x86_64/
├── hushmic.exe
├── onnxruntime.dll
└── models/
    ├── dpdfnet8_48khz_hr.onnx
    └── dpdfnet2_48khz_hr.onnx
```

That exact layout is what the binary resolves at run time — `models\<stem>.onnx` and `onnxruntime.dll` next to the EXE. The script fails *before* the long compile when a model or the DLL is missing, and again after staging if the directory is incomplete, rather than shipping a binary that cannot start.

## Run

```bash
dist/hushmic-windows-x86_64/hushmic.exe --doctor   # first: devices, cable, assets, autostart
dist/hushmic-windows-x86_64/hushmic.exe --tray
```

`--doctor` exits 1 when it finds problems and prefixes each failing line with `!!`. Run it before anything else on a new machine; it names the capture and render endpoints, the cable it would use, every asset with `ok`/`MISSING`, and whether the `Run` entry matches this EXE's path.

**One step stays manual.** HushMic renders into the cable's *input*, so select the cable's *output* (`CABLE Output`) as the microphone — in Windows sound settings, or directly in the app that should use it. Windows has no supported API to repoint the system default input, so the `set_default` config key, the tray's **Set as default microphone** entry and `--doctor` all state that instead of pretending.

## Command line

```
hushmic                 run the tray and forward "show yourself" to a running instance
hushmic --tray          run the tray only (what the autostart entry registers)
hushmic --headless      run without a tray icon
hushmic --enable-once   enable the mic until terminated
hushmic --test-window   open the A/B microphone test window
hushmic --about         the About window (with "Copy diagnostics")
hushmic --version       print the version, config path and model directory
hushmic --doctor        print a diagnostics report (exits 1 on problems)
hushmic --help          this text

control (talks to the running instance):
hushmic status [--json]      what it is doing
hushmic mode [STATE]         print or set: suppress|bypass|mute|off
hushmic toggle mute|bypass   hotkey-friendly overlay toggle
hushmic quit                 stop it

settings (applied live while running, saved to the file otherwise):
hushmic config [--json]           every key = value
hushmic config get KEY [--json]
hushmic config set KEY VALUE
hushmic config path
hushmic devices [--json]          microphones usable as `mic`
```

Config keys: `mic model attn_limit set_default autostart tray tray_icon notifications inference`. Control commands exit 2 when no instance is running. `hushmic service install` answers that systemd does not exist here and points at autostart.

## Hotkeys, config, autostart

- **Hotkeys**, fixed and not configurable: `Ctrl+Alt+M` toggles mute, `Ctrl+Alt+B` toggles bypass, `Ctrl+Alt+T` is push-to-talk (suppressed while held, muted on release rather than passing the raw mic), `Ctrl+Alt+Q` push-to-mute (silent while held). A combination another app already owns is reported as a warning on stderr and the other three still register.
- **Config file**: `%APPDATA%\hushmic\hushmic\config\config.toml`. The doubled name comes from `directories` deriving the path from organization and application name; `hushmic config path` prints it. Model and suppression strength are per microphone there (`mic_prefs`), edited with `hushmic config set`.
- **Autostart**: the `HushMic` string value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, written by `hushmic config set autostart true` as `"…\hushmic.exe" --tray`. The value is compared by content, not existence, so an install that was moved reads back as *off*.

## Limitations

| Gap | Why |
| --- | --- |
| No LADSPA plugin | Windows has no LADSPA host. The same engine runs in-process instead, so there is no plugin to load into another effects app. |
| No system-default takeover | No supported API repoints the default input; the tray entry, `set_default` and the doctor print the manual step. |
| No settings window | The tray's **Settings…** entry opens nothing; it points at `hushmic config` / `hushmic config set`. |
| Hotkeys are fixed | The four combinations are compiled in; there is no rebinding. |
| Second launch shows nothing | A running instance receives the "show" ping but has no window to raise; the message only reaches stderr of the daemon. |
| ONNX Runtime only | The native W7A8 engine is compiled for x86-64 Linux, so this build runs ONNX Runtime exclusively. |
| No anti-alias filter when resampling | The rate converter is linear interpolation; if a cable's mix rate is 96 kHz, content above 24 kHz folds back. |

## Troubleshooting

**No virtual cable found.** The chain refuses to start with `no virtual audio cable found — install VB-Audio Virtual Cable … Render devices found: …` and lists what WASAPI enumerated. Install VB-CABLE, reboot so the driver loads, and check that a render device name contains `CABLE Input`; `HUSHMIC_SINK` overrides the search. `hushmic.exe --doctor` shows the same list plus the `virtual cable:` verdict.

**The tray says On, but callers hear silence.** Look at the chain's start line on stderr: `[hushmic] chain: MIC [format] -> CABLE [format] (engine LABEL, attn N dB, 3840 samples latency)`. An `engine silent` label there means the chain is running and emitting zeros *by design* — no usable model for the configured one, or an inference thread that could not be spawned. The doctor's `assets:` block names the file it wanted and whether it exists.

**A model or `onnxruntime.dll` is missing.** The chain refuses to start and names the file: `model file not found at … — place it next to hushmic.exe, or set HUSHMIC_MODEL_DIR`, or `ONNX Runtime library … set ORT_DYLIB_PATH`. Overrides: `HUSHMIC_MODEL_DIR` (the `models` directory), `HUSHMIC_MODEL_PATH` (one file), `HUSHMIC_FALLBACK_MODEL_PATH` (the light model), `ORT_DYLIB_PATH` (the DLL).

**The microphone is unplugged or reset during a call.** The capture read fails, the chain stops with the reason on stderr, and a watchdog restarts it: it ticks every 5 s, acts after two consecutive bad verdicts, then stays quiet for 30 s so a broken device cannot become a restart loop. While the saved mic is gone the chain follows the system default — logged as `saved microphone '…' is not present — following the system default`, and `--doctor` reports `active mic: (system default)`.

**Autostart gives no tray icon.** Check that `--doctor` reports `autostart entry: present`. If you moved the folder after enabling it, toggle `autostart` off and on again to rewrite the path. `--headless` runs without a tray by design.

**The EXE is a GUI-subsystem build**, so double-clicking it opens no console. Logs go to stderr: start it from a terminal, or use `--doctor`.

## A Windows microphone driver?

Asked for repeatedly, so it is written up once: [the driver memo](docs/windows-virtual-mic-driver.md) covers the existing APO/ACX/PortCls options, the signing chain and the sample material. Verdict — *technically possible, not worth building*. A capture endpoint is a documented ACX pattern, but new kernel drivers need a WHCP signature (attestation signing is documented for testing, not retail), which means an EV certificate, a hardware dev centre account and HLK runs for a C component this project has nowhere else. VB-CABLE stays the dependency; if it ever has to go, the memo's variant D (render endpoint plus kernel-side loopback) is the only design that would justify the price.

## Relationship to upstream

This fork is upstream [`v0.11.0`](https://github.com/Fovty/hushmic/tree/v0.11.0) (commit `045daab`) plus the Windows work, and its history contains upstream's commits as they were published. The Linux application still builds from this tree; its build, install and usage documentation in [`docs/`](https://github.com/Fovty/hushmic/tree/main/docs) is upstream's and is not repeated here. Upstream is dual-licensed `MIT OR Apache-2.0`; so is this fork.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option. Both files are present and unchanged. Their copyright notice belongs to the upstream HushMic project; this fork adds no relicensing claim over that work.
