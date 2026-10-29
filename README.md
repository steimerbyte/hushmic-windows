<h1 align="center">
  <img src="docs/img/hushmic-logo.png" alt="HushMic" width="120"><br>
  HushMic
</h1>

<p align="center">
  <b>Real-time microphone noise suppression for Linux and Windows.</b><br>
  On Linux as a system-wide virtual mic; on Windows into a virtual audio cable.
  Open source, runs on the CPU, your audio never leaves the machine.
</p>

<p align="center">
  <a href="https://github.com/Fovty/hushmic/releases"><img src="https://img.shields.io/github/v/release/Fovty/hushmic?sort=semver" alt="Release"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License"></a>
  <a href="https://github.com/Fovty/hushmic/actions/workflows/release.yml"><img src="https://github.com/Fovty/hushmic/actions/workflows/release.yml/badge.svg" alt="Build"></a>
  <a href="https://ko-fi.com/fovty"><img src="https://img.shields.io/badge/Ko--fi-support-ff5e5b?logo=ko-fi&logoColor=white" alt="Support on Ko-fi"></a>
</p>

HushMic adds a virtual microphone to PipeWire that removes keyboard noise, fans and background chatter from your voice in real time. Pick **HushMic** as the input in Discord, TeamSpeak, OBS or your browser. The model is DPDFNet, which scored above Krisp, DeepFilterNet and RNNoise on the demo clips (see [Why DPDFNet](#why-dpdfnet)). Processing adds 80 ms of latency.

## Demo

Each clip plays the noisy input, then the same audio cleaned by HushMic.

<table>
<tr>
<td align="center"><b>Keyboard</b></td>
<td align="center"><b>Fan / AC hum</b></td>
<td align="center"><b>Café chatter</b></td>
</tr>
<tr>
<td><video src="https://github.com/user-attachments/assets/9cc7abe4-26fc-440f-88da-b86ef33df142" controls width="280"></video></td>
<td><video src="https://github.com/user-attachments/assets/d4b396a7-d980-4e0d-bac2-2a119debd768" controls width="280"></video></td>
<td><video src="https://github.com/user-attachments/assets/f4cd4504-d49a-4a46-95c2-01e56cd2d268" controls width="280"></video></td>
</tr>
</table>

Audio sources and licenses: [docs/demo/ASSETS.md](docs/demo/ASSETS.md). Measurements: [docs/comparison.md](docs/comparison.md).

## Install

Requirements: Linux on x86-64 with PipeWire and WirePlumber. PulseAudio apps need `pipewire-pulse`. A tray icon needs StatusNotifierItem support, which GNOME gets from the *AppIndicator and KStatusNotifierItem Support* extension. Without a tray, HushMic still runs and the command line controls it.

**Install script** (any distro, system-wide):

```bash
curl -fsSL https://raw.githubusercontent.com/Fovty/hushmic/main/scripts/install.sh | sudo sh
```

**Debian / Ubuntu**:

```bash
curl -fsSLO https://github.com/Fovty/hushmic/releases/latest/download/hushmic_0.11.0-1_amd64.deb
sudo apt install ./hushmic_0.11.0-1_amd64.deb
```

**Arch Linux** (AUR):

```bash
yay -S hushmic-bin
```

**Fedora** (COPR):

```bash
sudo dnf copr enable fovty/hushmic
sudo dnf install hushmic
```

**NixOS** (flake, builds from source):

```bash
nix run github:Fovty/hushmic-nix -- --tray
```

**AppImage**:

```bash
curl -fsSLO https://github.com/Fovty/hushmic/releases/latest/download/hushmic-x86_64.AppImage
chmod +x hushmic-x86_64.AppImage
./hushmic-x86_64.AppImage --tray
```

Ubuntu 22.04, custom prefixes, Nix flags, Flatpak and uninstalling: [docs/install.md](docs/install.md).

## Usage

Start HushMic from your application menu. A tray icon appears and noise suppression is on. Pick your microphone in the tray menu, then choose **HushMic** as the input in your app, or turn on **Set as default microphone** so apps that follow the system default use it.

The menu switches between noise suppression, **bypass** (your raw voice) and **mute** (silence on the virtual mic). Switching is instant and does not reconnect your call. It also holds the model choice (quality or light), the suppression strength, start on login and global shortcuts.

**Test my mic** opens a window with the raw microphone and the cleaned output side by side, and records a 10-second sample you can play back both ways.

<p align="center">
  <img src="docs/img/hushmic-ab-window.png" alt="Live A/B mic test" width="720">
</p>

The same controls work from a terminal, so any hotkey tool can drive them:

```bash
hushmic status              # what the running instance is doing
hushmic mode mute           # suppress | bypass | mute | off
hushmic toggle mute         # one key on, same key off
hushmic config set attn_limit strong
hushmic quit
```

Menu screenshots, keyboard shortcuts, every command and every config key: [docs/usage.md](docs/usage.md).

## Starting at login

Turn on **Start on login** in the tray menu, or run `hushmic config set autostart true`. For a daemon without a tray icon, `hushmic --headless` and the systemd user unit are described in [docs/headless.md](docs/headless.md). Use one of the two, not both.

## Windows

The same denoiser, hosted by the app instead of by a server. Windows has no PipeWire, so `hushmic.exe` runs the audio itself: WASAPI captures your microphone, each 10 ms hop goes through the same DPDFNet engine the Linux plugin drives, and the result is rendered into a virtual audio cable's input. Latency is the same 80 ms.

Requirements: Windows x86-64, the [Microsoft Visual C++ 2015–2022 Redistributable](https://aka.ms/vs/17/release/vc_redist.x64.exe) (the MSVC build links `VCRUNTIME140.dll`; Windows does not ship it) and a virtual cable, either [VB-CABLE](https://vb-audio.com/Cable/) or VoiceMeeter. HushMic renders into the output device whose name contains `CABLE Input`, `VoiceMeeter Input` or `VB-Audio`, in that order; a name of your own goes in `HUSHMIC_SINK`.

**One step stays manual.** HushMic renders into the cable's *input*, so select the cable's *output* (`CABLE Output`) as the microphone in Windows sound settings, or directly in the app that should use it. HushMic cannot repoint the system default input: Windows has no supported API for it. The tray's **Set as default microphone** entry and `--doctor` say so instead of pretending, and `set_default` in the config only records the wish.

```bash
./scripts/build-windows.sh                  # also on Windows, under Git Bash
dist/hushmic-windows-x86_64/hushmic.exe --doctor
```

The script cross-compiles with `cargo xwin` on Linux (MSVC target, no Windows host needed) and is a plain `cargo build` on Windows. It stages `hushmic.exe`, `onnxruntime.dll` and `models/*.onnx` beside each other — the layout the binary resolves at run time — and fails when one of them is missing instead of shipping a binary that cannot start. The models come from `scripts/setup-assets.sh`; the DLL from the [ONNX Runtime 1.27.0](https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-win-x64-1.27.0.zip) release archive, via `HUSHMIC_ORT_DLL` or `assets/win/onnxruntime.dll`.

The command line is the Linux one:

```bash
hushmic.exe --tray              # tray icon only (what the autostart entry registers)
hushmic.exe                     # same, and asks a running instance to show itself
hushmic.exe --headless          # no tray icon
hushmic.exe --doctor            # diagnostics report, exits 1 on problems
hushmic.exe --version           # version, config path, model directory
hushmic.exe --help
hushmic.exe status              # what the running instance is doing
hushmic.exe mode mute           # suppress | bypass | mute | off
hushmic.exe toggle bypass
hushmic.exe config set attn_limit strong
hushmic.exe devices             # microphones usable as `mic`
hushmic.exe quit
```

`hushmic.exe service install` answers with a pointer to autostart, because there is no systemd here. The config file is `%APPDATA%\hushmic\hushmic\config\config.toml` — `hushmic` appears twice because `directories` derives it from organization and application name; `hushmic.exe config path` prints it.

**Start with Windows** is the `HushMic` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, written by `hushmic config set autostart true` (remove it again with `false`). It runs `"…\hushmic.exe" --tray`. The value is compared by content, not existence, so a moved folder reads back as off — `--doctor` then says `autostart entry: missing` for an enabled config — and toggling autostart off and on writes the new path.

**Hotkeys**, provisional and not configurable yet: Ctrl+Alt+M toggles mute, Ctrl+Alt+B toggles bypass, Ctrl+Alt+T is push-to-talk (clean while held) and Ctrl+Alt+Q push-to-mute (silent while held).

**What is missing on Windows** — the honest list:

| Gap | Why |
| --- | --- |
| No LADSPA plugin | Windows has no LADSPA host. The same engine runs in-process instead, so there is no EasyEffects-style plugin to load. |
| No system-default takeover | No supported API repoints the default input; `set_default`, the tray entry and the doctor print the manual step. |
| No settings window | The tray's **Settings…** entry opens none: it says the settings live in the config file, edited with `hushmic.exe config`. |

The tray holds noise suppression, bypass and mute, the A/B window (`--test-window`: live panes plus a recorded take), the About window with **Copy diagnostics**, and the four hotkeys. Model and suppression strength are per microphone in the config file (`mic_prefs`), set with `hushmic.exe config`.

The EXE is a GUI-subsystem build, so double-clicking it opens no console window. It logs to stderr: start it from a terminal (`hushmic.exe --tray`) to read that log, or run `hushmic.exe --doctor`.

**Status: compile-verified only.** The Windows build type-checks and links; it has not been run on Windows hardware. `hushmic.exe --doctor` is the first thing to run there — it names the capture and render devices it found, the virtual cable, and every asset with `ok` or `MISSING`.

### Windows troubleshooting

**No virtual cable found.** The start fails with `no virtual audio cable found … Render devices found: …` and lists what WASAPI enumerated. Install VB-CABLE, reboot so the driver loads, and check that the output device name contains `CABLE Input`; `HUSHMIC_SINK=<part of the name>` overrides the search. `hushmic.exe --doctor` shows the same list.

**The tray says On, but everyone hears silence.** The chain's start line on stderr names the engine (`[hushmic] chain: … (engine …, attn … dB, 3840 samples latency)`), and `engine silent` means it degraded instead of failing: no usable model for the configured one, or a worker thread it could not spawn, leaves the chain running and outputting zeros by design. The `assets:` block of the doctor report says which file it wanted and whether it is there.

**A model or `onnxruntime.dll` is missing.** The chain refuses to start and names the file: `model file not found at … — place it next to hushmic.exe, or set HUSHMIC_MODEL_DIR` (or `ONNX Runtime library … set ORT_DYLIB_PATH`). The overrides are `HUSHMIC_MODEL_DIR` for the `models` directory, `HUSHMIC_MODEL_PATH` for one file, `HUSHMIC_FALLBACK_MODEL_PATH` for the light model and `ORT_DYLIB_PATH` for the DLL. The build script produces exactly this layout.

**The microphone is unplugged or reset during a call.** The capture read fails, the chain stops with the reason on stderr, and the watchdog restarts it: it checks every 5 s, acts after two consecutive bad ticks (10 s at most) and then stays quiet for 30 s so a broken device cannot become a restart loop. While the saved mic is gone the chain follows the system default and the doctor's `active mic` says so.

**Autostart does not give you a tray icon.** The Run value must be `"…\hushmic.exe" --tray`; `hushmic config set autostart true` writes exactly that, and `hushmic.exe --doctor` shows `autostart entry: present` or `missing`. If you moved the folder after enabling it, toggle it off and on again. `--headless` has no tray by design.

## Why DPDFNet

I wanted noise suppression on Linux that matches Krisp and installs like an app. I ran the three demo clips through the candidates and scored the output with DNSMOS P.835 (1 to 5, higher is better):

| Model                             | Overall | Background | Speech |
| --------------------------------- | :-----: | :--------: | :----: |
| **DPDFNet** (HushMic)             | **3.20** | **4.15**  | **3.43** |
| DeepFilterNet 3                   |  2.97   |    3.98    |  3.26  |
| Krisp v9.9.3                      |  2.57   |    3.96    |  2.81  |
| khip (older Krisp model port)     |  2.43   |    3.78    |  2.71  |
| GTCRN                             |  2.42   |    3.71    |  2.76  |
| RNNoise (EasyEffects default)     |  2.01   |    3.93    |  2.60  |
| Unprocessed input                 |  1.49   |    1.48    |  2.01  |

Three clips are not a formal benchmark, but the source audio is public. Method, per-clip numbers and CPU cost: [docs/comparison.md](docs/comparison.md). DPDFNet is a DeepFilterNet-lineage model by Ceva ([paper](https://arxiv.org/abs/2512.16420)).

## How it works

- [`hushmic-denoiser`](crates/hushmic-denoiser) runs the DPDFNet model on 48 kHz mono audio, on ONNX Runtime or the native engine. It is a plain Rust library you can embed in your own app.
- `dpdfnet-ladspa` wraps it as a LADSPA plugin.
- `hushmic` writes a PipeWire `module-filter-chain` config and runs it as a child process. PipeWire owns the real-time scheduling, so no `setcap` is needed. A watchdog recreates the virtual mic after a PipeWire restart or suspend, and quitting restores your previous default input. On Windows there is no graph to configure, so the same binary is the host itself ([`crates/hushmic/src/win`](crates/hushmic/src/win)): one thread captures the microphone, denoises and renders into the cable, driving the same engine in-process instead of as a plugin.

## FAQ

**Does my audio go anywhere?** No. Everything runs on the CPU and nothing is uploaded.

**How much latency and CPU?** 80 ms of processing plus PipeWire's own buffering; on PipeWire 1.6 and later apps like OBS can compensate for it. On CPUs with AVX2 and FMA (most Intel Core and AMD Ryzen CPUs since 2015) a native engine runs the models in under half the CPU ONNX Runtime needs, and in about a quarter to a third on CPUs with AVX-VNNI. Other CPUs, including many older Pentium, Celeron and Atom chips, use ONNX Runtime, and on slow ones HushMic runs the light model on its own. Details: [docs/troubleshooting.md](docs/troubleshooting.md#latency-and-cpu).

**No tray icon on GNOME?** Install the *AppIndicator and KStatusNotifierItem Support* extension. HushMic keeps running without it.

**Can I use it in EasyEffects?** Yes. EasyEffects can load HushMic's plugin as the DPDFNet model of its Deep Noise Remover, on your microphone and also on what you hear. Install HushMic from a package, then pick the model in EasyEffects. Details: [docs/usage.md](docs/usage.md#using-the-plugin-in-easyeffects).

More: [docs/troubleshooting.md](docs/troubleshooting.md).

## Alternatives

- [NoiseTorch-ng](https://github.com/noisetorch/NoiseTorch): RNNoise-based virtual mic, needs `setcap`.
- [EasyEffects](https://github.com/wwmm/easyeffects): full PipeWire effects suite with an RNNoise denoiser. Its Deep Noise Remover can also run DPDFNet through HushMic's plugin.
- [noise-suppression-for-voice](https://github.com/werman/noise-suppression-for-voice): RNNoise LADSPA/VST plugin, wired by hand.
- [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet): the model family HushMic builds on, with its own LADSPA plugin.

## Build from source

```bash
git clone https://github.com/Fovty/hushmic
cd hushmic
./scripts/setup-assets.sh        # fetch the DPDFNet models, native weights and ONNX Runtime
cargo build --release
```

This produces `target/release/hushmic` and `target/release/libdpdfnet_ladspa.so`. `scripts/install.sh` shows the install layout; `crates/dpdfnet-ladspa/examples/run-filter-chain.md` shows how to load the plugin by hand. For Windows, `./scripts/build-windows.sh` stages `dist/hushmic-windows-x86_64/` instead (see [Windows](#windows)).

## Contributing

Translations happen on [Weblate](https://hosted.weblate.org/projects/hushmic/), see [TRANSLATING.md](TRANSLATING.md). If HushMic is useful to you, a coffee on [Ko-fi](https://ko-fi.com/fovty) helps keep it maintained.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.

## Credits

[DPDFNet](https://github.com/ceva-ip/DPDFNet) by Ceva (Apache-2.0) is the speech-enhancement model; the native engine is its authors' C runtime, included unmodified in [crates/hushmic-denoiser/native](crates/hushmic-denoiser/native) under the same license. The real-time plugin follows the architecture of [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet). Built with [PipeWire](https://pipewire.org), [ort](https://github.com/pykeio/ort), [rustfft](https://github.com/ejmahler/RustFFT) and [ksni](https://github.com/iovxw/ksni). Demo voice: "After Love" by Sara Teasdale, read by a LibriVox volunteer (public domain); keyboard by C40115 and fan hum by Gravity Sound (CC BY 4.0); café ambience public domain. Details in [docs/demo/ASSETS.md](docs/demo/ASSETS.md).
