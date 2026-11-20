use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    if let Err(error) = build() {
        panic!("memento-cli asset build failed: {error}");
    }
}

fn build() -> Result<(), String> {
    let manifest = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "CARGO_MANIFEST_DIR is unavailable".to_string())?;
    let output = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "OUT_DIR is unavailable".to_string())?;
    let configured = env::var_os("MEMENTO_OPENCODE_BUNDLE").map(PathBuf::from);
    let candidate = configured
        .clone()
        .unwrap_or_else(|| manifest.join("../../adapters/opencode/dist/index.js"));
    println!("cargo:rerun-if-changed={}", candidate.display());
    let bundle = output.join("opencode-index.js");
    if configured.is_some() && !candidate.is_file() {
        return Err(format!(
            "configured OpenCode bundle does not exist: {}",
            candidate.display()
        ));
    }
    if candidate.is_file() {
        fs::copy(&candidate, &bundle)
            .map_err(|error| format!("cannot embed {}: {error}", candidate.display()))?;
        println!("cargo:rustc-cfg=memento_opencode_bundle");
    } else {
        fs::write(&bundle, "")
            .map_err(|error| format!("cannot create {}: {error}", bundle.display()))?;
    }
    println!("cargo:rustc-check-cfg=cfg(memento_opencode_bundle)");
    println!("cargo:rerun-if-env-changed=MEMENTO_OPENCODE_BUNDLE");
    println!(
        "cargo:rustc-env=MEMENTO_TARGET={}",
        env::var("TARGET").map_err(|error| format!("TARGET is unavailable: {error}"))?
    );
    println!("cargo:rerun-if-changed=../../skills");
    Ok(())
}
