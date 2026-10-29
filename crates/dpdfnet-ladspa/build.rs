use std::path::PathBuf;

fn main() {
    // Repo root = two levels up from this crate (crates/dpdfnet-ladspa).
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = crate_dir.parent().unwrap().parent().unwrap();
    // Packaging can bake install-layout paths into the plugin via the build-time env
    // vars below; otherwise we fall back to the repo-relative *development* defaults.
    // At runtime these are still overridable via HUSHMIC_MODEL_PATH / ORT_DYLIB_PATH.
    let model = std::env::var("HUSHMIC_BUILD_MODEL").unwrap_or_else(|_| {
        repo_root
            .join("assets/models/dpdfnet8_48khz_hr.onnx")
            .display()
            .to_string()
    });
    // The bundled runtime's file name follows the build target, not the
    // build host: a cross-compiled Windows build carries onnxruntime.dll.
    let dylib_name = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        "onnxruntime.dll"
    } else {
        "libonnxruntime.so"
    };
    let dylib = std::env::var("HUSHMIC_BUILD_DYLIB").unwrap_or_else(|_| {
        repo_root
            .join("assets/lib")
            .join(dylib_name)
            .display()
            .to_string()
    });
    println!("cargo:rustc-env=HUSHMIC_DEFAULT_MODEL={model}");
    println!("cargo:rustc-env=HUSHMIC_DEFAULT_DYLIB={dylib}");
    println!("cargo:rerun-if-env-changed=HUSHMIC_BUILD_MODEL");
    println!("cargo:rerun-if-env-changed=HUSHMIC_BUILD_DYLIB");
    println!("cargo:rerun-if-changed=build.rs");
}
