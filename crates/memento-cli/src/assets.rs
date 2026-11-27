use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::CliError;

pub(super) const OPENCODE_BUNDLE_AVAILABLE: bool = cfg!(memento_opencode_bundle);
pub(super) static OPENCODE_BUNDLE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/opencode-index.js"));

pub(super) const SKILLS: &[(&str, &[u8])] = &[
    (
        "memento-cleanup",
        include_bytes!("../../../skills/memento-cleanup/SKILL.md"),
    ),
    (
        "memento-connect-harness",
        include_bytes!("../../../skills/memento-connect-harness/SKILL.md"),
    ),
    (
        "memento-handoff",
        include_bytes!("../../../skills/memento-handoff/SKILL.md"),
    ),
    (
        "memento-recall",
        include_bytes!("../../../skills/memento-recall/SKILL.md"),
    ),
];

pub(super) fn data_dir() -> Result<PathBuf, CliError> {
    if let Some(root) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(root).join("memento"));
    }
    let home = env::var_os("HOME").ok_or_else(|| CliError::other("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share/memento"))
}

pub(super) fn write_opencode_plugin(data_dir: &Path) -> Result<PathBuf, CliError> {
    let directory = data_dir.join("opencode-plugin");
    fs::create_dir_all(&directory).map_err(|error| {
        CliError::other(format!("cannot create {}: {error}", directory.display()))
    })?;
    let package = serde_json::json!({
        "name": "@memento-pool/opencode",
        "version": env!("CARGO_PKG_VERSION"),
        "private": true,
        "type": "module",
        "exports": { ".": "./index.js" }
    });
    let package_path = directory.join("package.json");
    fs::write(&package_path, serde_json::to_vec_pretty(&package).unwrap()).map_err(|error| {
        CliError::other(format!("cannot write {}: {error}", package_path.display()))
    })?;
    let bundle_path = directory.join("index.js");
    fs::write(&bundle_path, OPENCODE_BUNDLE).map_err(|error| {
        CliError::other(format!("cannot write {}: {error}", bundle_path.display()))
    })?;
    Ok(directory)
}

pub(super) fn daemon_image(image_override: Option<&str>) -> &str {
    image_override
        .filter(|image| !image.trim().is_empty())
        .unwrap_or(concat!(
            "ghcr.io/luckynee/mementod:",
            env!("CARGO_PKG_VERSION")
        ))
}

pub(super) fn render_compose(image_override: Option<&str>) -> String {
    let image = daemon_image(image_override);
    include_str!("../assets/compose.yaml")
        .replace("__MEMENTO_IMAGE__", &serde_json::to_string(image).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_skills_include_all_four() {
        assert_eq!(SKILLS.len(), 4);
        assert!(SKILLS.iter().all(|(_, content)| !content.is_empty()));
    }

    #[test]
    fn compose_rendering_substitutes_default_and_override() {
        assert!(render_compose(None).contains(&format!(
            "\"ghcr.io/luckynee/mementod:{}\"",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(
            render_compose(Some("example/mementod:test"))
                .contains("image: \"example/mementod:test\"")
        );
    }

    #[test]
    fn plugin_writer_creates_package_and_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = write_opencode_plugin(directory.path()).unwrap();
        let package: serde_json::Value =
            serde_json::from_slice(&fs::read(plugin.join("package.json")).unwrap()).unwrap();
        assert_eq!(package["name"], "@memento-pool/opencode");
        assert_eq!(package["exports"]["."], "./index.js");
        assert_eq!(fs::read(plugin.join("index.js")).unwrap(), OPENCODE_BUNDLE);
    }
}
