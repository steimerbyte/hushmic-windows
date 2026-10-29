//! Windows diagnostics: `hushmic --doctor` and the About window's
//! "Copy diagnostics" button both render the same plain-text report.
//! Read-only probes, no audio, no network; everything in the report is
//! safe to paste into a public issue (device/model names only).
//!
//! The counterpart of the Linux [`crate::diagnostics`] report: same shape
//! (facts first, `!!`-prefixed problems, problem count last), different
//! probes — the Windows host IS the audio graph, so what matters is the
//! capture device, the virtual cable's render side, the model file and the
//! ONNX Runtime next to the EXE.
//!
//! The rendering half compiles everywhere (so its shape is unit-tested on
//! every host); the probes are Windows-only.

/// One resolved asset path (model / ONNX Runtime).
pub struct AssetFact {
    pub what: &'static str,
    pub path: String,
    pub exists: bool,
}

/// Every fact the report prints. `Option` = the probe could not run
/// (rendered as `unavailable`, counted as a problem only where noted).
pub struct Report {
    pub version: String,
    pub install_type: String,
    pub config_path: String,
    pub enabled: bool,
    pub preferred_mic: Option<String>,
    /// The device the chain would capture from right now: the configured
    /// mic while it is still live, else the system default.
    pub active_mic: Option<String>,
    pub model: String,
    pub attn_limit: f32,
    /// The device whose saved profile is in effect, by name; None = the
    /// defaults apply.
    pub profile: Option<String>,
    pub set_default: bool,
    pub mic_profiles: usize,
    pub inference_setting: String,
    pub autostart: bool,
    /// The HKCU Run entry; None = the probe failed.
    pub autostart_entry: Option<bool>,
    pub tray: bool,
    pub notifications: bool,
    /// None = the probe failed (the mutex could not be inspected).
    pub instance_running: Option<bool>,
    /// The running instance's chain: the chain lives in the tray process,
    /// so the only way to ask is the control channel. None = no instance
    /// answered (the chain then runs nowhere).
    pub chain_running: Option<bool>,
    /// The capture (input) devices; None = the probe failed.
    pub capture_devices: Option<Vec<String>>,
    /// The render (output) devices; None = the probe failed.
    pub render_devices: Option<Vec<String>>,
    /// The virtual cable's render side, by name; None = no cable found.
    pub cable: Option<String>,
    pub assets: Vec<AssetFact>,
}

/// Gather every fact — the I/O half. Never panics: anything un-probeable
/// lands as `None`/`false` and renders as such. Works with no tray running
/// and with no virtual cable installed (both are then facts in the report).
#[cfg(windows)]
pub fn collect() -> Report {
    let cfg = crate::config::Config::load();
    let exe = std::env::current_exe().ok();
    // The device whose settings apply: the configured mic while it is still
    // a live capture device, else the defaults.
    let device = crate::config_cli::offline_device(&cfg);
    let profile = cfg.profile_for(device.as_deref());
    let model = profile.model.clone();
    // The chain's own resolution: the doctor must name the files the
    // engine will actually load.
    let assets = vec![
        asset("model file", crate::win::chain::model_path(&model)),
        asset("ONNX Runtime", crate::win::chain::ort_path()),
    ];
    let (capture_devices, render_devices) = match crate::win::audio::list_devices() {
        Ok((c, r)) => (Some(c), Some(r)),
        Err(_) => (None, None),
    };
    let cable = detect_cable(render_devices.as_deref());
    // The chain lives in the tray process, so the control channel is the
    // only way to ask it whether it runs.
    let status = running_status();
    Report {
        version: env!("CARGO_PKG_VERSION").into(),
        install_type: install_type(exe.as_deref()),
        config_path: crate::config::Config::path().display().to_string(),
        enabled: cfg.enabled,
        preferred_mic: cfg.mic.clone(),
        active_mic: device,
        model,
        attn_limit: profile.attn_limit,
        profile: profile
            .device
            .as_deref()
            .filter(|_| profile.saved)
            .map(str::to_string),
        set_default: cfg.set_default,
        mic_profiles: cfg.mic_prefs.len(),
        inference_setting: cfg.inference.as_str().to_string(),
        autostart: cfg.autostart,
        autostart_entry: crate::win::shell::autostart_enabled().ok(),
        tray: cfg.tray,
        notifications: cfg.notifications,
        instance_running: crate::win::shell::instance_running().ok(),
        chain_running: status
            .as_ref()
            .and_then(|s| s.get("chain"))
            .and_then(|c| c.get("running"))
            .and_then(|r| r.as_bool()),
        capture_devices,
        render_devices,
        cable,
        assets,
    }
}

/// Render the report: plain `key: value` text, failing checks prefixed
/// `!!`, last line the problem summary. Returns (text, problem count);
/// `--doctor` exits 1 when the count is nonzero.
pub fn render(r: &Report) -> (String, usize) {
    let mut out = String::new();
    let mut problems = 0usize;
    // A line whose check failed: `!!`-prefixed and counted.
    let mut line = |out: &mut String, bad: bool, s: String| {
        if bad {
            problems += 1;
            out.push_str("!! ");
        }
        out.push_str(&s);
        out.push('\n');
    };

    out.push_str("hushmic diagnostics\n");
    line(&mut out, false, format!("version: {}", r.version));
    line(&mut out, false, format!("install: {}", r.install_type));
    line(&mut out, false, format!("config: {}", r.config_path));
    line(&mut out, false, format!("  enabled: {}", r.enabled));
    line(
        &mut out,
        false,
        format!(
            "  preferred mic: {}",
            r.preferred_mic.as_deref().unwrap_or("(system default)")
        ),
    );
    line(
        &mut out,
        false,
        format!(
            "  active mic: {}",
            r.active_mic.as_deref().unwrap_or("(system default)")
        ),
    );
    line(&mut out, false, format!("  model: {}", r.model));
    line(
        &mut out,
        false,
        format!("  attenuation limit: {} dB", r.attn_limit),
    );
    line(
        &mut out,
        false,
        match &r.profile {
            Some(p) => format!("  settings: {p} profile"),
            None => "  settings: defaults".to_string(),
        },
    );
    // The documented Windows gap: there is no supported API to change the
    // system default input, so this is never a silent pass. The wording
    // matches the chain's own `describe()`.
    if r.set_default {
        line(
            &mut out,
            true,
            "  set default: requested, but it cannot be honoured on Windows — there is \
             no supported API to repoint the system default input; select the cable's \
             output device as your microphone in Windows sound settings"
                .into(),
        );
    } else {
        line(
            &mut out,
            false,
            "  set default: not honoured on Windows (there is no supported API to \
             repoint the system default input; select the cable's output device as your \
             microphone in Windows sound settings)"
                .into(),
        );
    }
    line(
        &mut out,
        false,
        format!("  per-mic profiles: {}", r.mic_profiles),
    );
    line(
        &mut out,
        false,
        format!("  inference: {}", r.inference_setting),
    );
    line(&mut out, false, format!("  autostart: {}", r.autostart));
    line(&mut out, false, format!("  tray: {}", r.tray));
    line(&mut out, false, format!("  notifications: {}", r.notifications));
    line(
        &mut out,
        false,
        match r.instance_running {
            Some(true) => "instance running: yes".to_string(),
            Some(false) => "instance running: no".to_string(),
            None => "instance running: unavailable".to_string(),
        },
    );
    line(
        &mut out,
        false,
        match r.chain_running {
            Some(true) => "chain running: yes".to_string(),
            Some(false) => "chain running: no".to_string(),
            None => "chain running: unavailable (no instance answered)".to_string(),
        },
    );
    {
        // The same declared latency the chain reports (the pump's figure).
        let samples = latency_samples();
        line(
            &mut out,
            false,
            format!(
                "chain latency: {} ms ({} samples @ 48 kHz)",
                samples * 1000 / crate::win::RATE as usize,
                samples
            ),
        );
    }
    match &r.capture_devices {
        Some(v) if v.is_empty() => line(&mut out, true, "capture devices: 0".into()),
        Some(v) => line(
            &mut out,
            false,
            format!("capture devices: {} ({})", v.len(), v.join(", ")),
        ),
        None => line(&mut out, true, "capture devices: unavailable".into()),
    }
    match &r.render_devices {
        Some(v) if v.is_empty() => line(&mut out, false, "render devices: 0".into()),
        Some(v) => line(
            &mut out,
            false,
            format!("render devices: {} ({})", v.len(), v.join(", ")),
        ),
        None => line(&mut out, false, "render devices: unavailable".into()),
    }
    match &r.cable {
        Some(c) => line(
            &mut out,
            false,
            format!("virtual cable: {c} (render side present)"),
        ),
        None => line(
            &mut out,
            true,
            "virtual cable: missing — install VB-CABLE (https://vb-audio.com/Cable/) \
             and select its output device as your microphone"
                .into(),
        ),
    }
    // The autostart entry IS what "autostart" means, so a config that asks
    // for it without the Run key is a real mismatch.
    match (r.autostart, r.autostart_entry) {
        (true, Some(false)) => line(
            &mut out,
            true,
            "autostart entry: missing (HKCU Run has no HushMic value)".into(),
        ),
        (true, Some(true)) => line(&mut out, false, "autostart entry: present".into()),
        (false, Some(true)) => line(
            &mut out,
            false,
            "autostart entry: present (autostart is off in the config)".into(),
        ),
        (false, Some(false)) => line(&mut out, false, "autostart entry: absent".into()),
        (_, None) => line(&mut out, false, "autostart entry: unavailable".into()),
    }
    out.push_str("assets:\n");
    for a in &r.assets {
        line(
            &mut out,
            !a.exists,
            format!(
                "  {}: {} ({})",
                a.what,
                a.path,
                if a.exists { "ok" } else { "MISSING" }
            ),
        );
    }
    if problems == 0 {
        out.push_str("no problems found\n");
    } else {
        out.push_str(&format!("{problems} problem(s) found\n"));
    }
    (out, problems)
}

/// The whole report: the text and the problem count. `--doctor` prints the
/// text and exits 1 when the count is nonzero.
#[cfg(windows)]
pub fn report() -> (String, usize) {
    render(&collect())
}

/// The chain's declared algorithmic latency in samples — the same figure
/// the pump reports and the Linux chain declares.
pub fn latency_samples() -> usize {
    (crate::win::pump::LATENCY_MS * crate::win::RATE as f32 / 1000.0) as usize
}

/// Classify the install from the EXE's location: a cargo build tree is a
/// development build, everything else is an install next to its own
/// `models\` and `onnxruntime.dll`.
pub fn install_type(exe: Option<&std::path::Path>) -> String {
    let Some(exe) = exe else {
        return "unknown".into();
    };
    let dir = exe.parent().unwrap_or(exe);
    let in_target_tree = dir
        .components()
        .any(|c| c.as_os_str() == "target")
        && dir.file_name().is_some_and(|n| n == "debug" || n == "release");
    if in_target_tree {
        format!("development build ({})", dir.display())
    } else {
        format!("installed ({})", dir.display())
    }
}

/// One resolved asset: where the file is and whether it is there. A
/// resolution failure (the OS cannot name our own EXE) reads as a missing
/// file whose path IS the diagnosis.
#[cfg(windows)]
fn asset(what: &'static str, resolved: Result<std::path::PathBuf, String>) -> AssetFact {
    match resolved {
        Ok(p) => AssetFact {
            what,
            path: p.display().to_string(),
            exists: p.exists(),
        },
        Err(e) => AssetFact {
            what,
            path: e,
            exists: false,
        },
    }
}

/// The running instance's status, asked over the control channel (the
/// chain lives in the tray process, so there is nothing else to read).
/// None = no instance answered.
#[cfg(windows)]
fn running_status() -> Option<serde_json::Value> {
    let args: Vec<String> = ["status", "--json"].iter().map(|s| s.to_string()).collect();
    let (code, out) = crate::win::control::client_run(&args);
    if code != 0 {
        return None;
    }
    serde_json::from_str(&out).ok()
}

/// The virtual cable's render side, by name. The search order is the
/// explicit override first, then the cables HushMic is known to work with.
#[cfg(windows)]
pub fn detect_cable(render_devices: Option<&[String]>) -> Option<String> {
    let mut needles: Vec<String> = Vec::new();
    if let Some(s) = std::env::var_os("HUSHMIC_SINK").filter(|v| !v.is_empty()) {
        needles.push(s.to_string_lossy().into_owned());
    }
    needles.extend(
        ["CABLE Input", "VoiceMeeter Input", "VB-Audio"]
            .iter()
            .map(|s| s.to_string()),
    );
    for needle in &needles {
        if crate::win::audio::has_render(needle) {
            // The full name is what the user picks in the sound settings.
            let found = render_devices.and_then(|v| {
                v.iter()
                    .find(|d| d.to_lowercase().contains(&needle.to_lowercase()))
                    .cloned()
            });
            return Some(found.unwrap_or_else(|| needle.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Report {
        Report {
            version: "0.0.0".into(),
            install_type: "installed (C:/hushmic)".into(),
            config_path: "C:/cfg/config.toml".into(),
            enabled: true,
            preferred_mic: None,
            active_mic: None,
            model: "dpdfnet8_48khz_hr".into(),
            attn_limit: 100.0,
            profile: None,
            set_default: false,
            mic_profiles: 0,
            inference_setting: "auto".into(),
            autostart: false,
            autostart_entry: Some(false),
            tray: true,
            notifications: true,
            instance_running: Some(false),
            chain_running: Some(false),
            capture_devices: Some(vec!["Built-in Mic".into()]),
            render_devices: Some(vec!["CABLE Input (VB-Audio Virtual Cable)".into()]),
            cable: Some("CABLE Input (VB-Audio Virtual Cable)".into()),
            assets: vec![AssetFact {
                what: "model file",
                path: "C:/hushmic/models/dpdfnet8_48khz_hr.onnx".into(),
                exists: true,
            }],
        }
    }

    #[test]
    fn healthy_report_has_no_problems() {
        let (text, problems) = render(&healthy());
        assert_eq!(problems, 0, "{text}");
        assert!(text.starts_with("hushmic diagnostics\n"), "{text}");
        assert!(text.trim_end().ends_with("no problems found"), "{text}");
    }

    #[test]
    fn missing_cable_and_model_are_problems() {
        let mut r = healthy();
        r.cable = None;
        r.assets[0].exists = false;
        let (text, problems) = render(&r);
        assert_eq!(problems, 2, "{text}");
        assert!(text.contains("!! virtual cable: missing"), "{text}");
        assert!(text.contains("VB-CABLE"), "{text}");
        assert!(text.contains("!!   model file:"), "{text}");
        assert!(text.trim_end().ends_with("2 problem(s) found"), "{text}");
    }

    #[test]
    fn set_default_is_never_a_silent_pass() {
        let mut r = healthy();
        r.set_default = true;
        let (text, problems) = render(&r);
        assert_eq!(problems, 1, "{text}");
        assert!(text.contains("Windows sound settings"), "{text}");
    }

    #[test]
    fn install_type_separates_the_target_tree() {
        assert!(
            install_type(Some(std::path::Path::new(
                "C:/src/hushmic/target/release/hushmic.exe"
            )))
            .starts_with("development build"),
        );
        assert!(
            install_type(Some(std::path::Path::new(
                "C:/Program Files/HushMic/hushmic.exe"
            )))
            .starts_with("installed"),
        );
    }
}

