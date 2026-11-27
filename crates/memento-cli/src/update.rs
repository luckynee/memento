use std::env;
use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use clap::Args;
use serde::Deserialize;

use crate::{Cli, CliError};

#[derive(Debug, Args)]
pub(super) struct UpdateArgs {
    #[arg(
        long,
        help = "Report whether an update is available without installing it"
    )]
    check: bool,
    #[arg(long, help = "Install a specific release version")]
    version: Option<String>,
    #[arg(long, help = "Refresh saved components without replacing binaries")]
    refresh: bool,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
}

fn version_tuple(value: &str) -> Result<(u64, u64, u64), CliError> {
    let version = value.strip_prefix('v').unwrap_or(value);
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(CliError::other(format!("invalid version: {value}")));
    }
    let parsed = parts
        .iter()
        .map(|part| part.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CliError::other(format!("invalid version: {value}")))?;
    Ok((parsed[0], parsed[1], parsed[2]))
}

pub(super) fn release_version() -> Result<String, CliError> {
    let location = env::var("MEMENTO_RELEASES_API")
        .unwrap_or_else(|_| "https://api.github.com/repos/luckynee/memento/releases/latest".into());
    let body = if let Some(path) = location.strip_prefix("file://") {
        fs::read(path)
            .map_err(|error| CliError::other(format!("cannot read release metadata: {error}")))?
    } else {
        reqwest::blocking::Client::new()
            .get(&location)
            .header(
                "User-Agent",
                format!("memento-cli/{}", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::bytes)
            .map_err(|error| CliError::other(format!("cannot fetch release metadata: {error}")))?
            .to_vec()
    };
    let release: Release = serde_json::from_slice(&body)
        .map_err(|error| CliError::other(format!("invalid release metadata: {error}")))?;
    version_tuple(&release.tag_name)?;
    Ok(release.tag_name.trim_start_matches('v').to_string())
}

fn inside(path: &Path, parent: &Path) -> bool {
    path.starts_with(parent)
}

fn download_asset(base: &str, name: &str) -> Result<Vec<u8>, CliError> {
    let source = format!("{}/{}", base.trim_end_matches('/'), name);
    if let Some(path) = source.strip_prefix("file://") {
        return fs::read(path)
            .map_err(|error| CliError::other(format!("cannot read asset {name}: {error}")));
    }
    reqwest::blocking::Client::new()
        .get(source)
        .header(
            "User-Agent",
            format!("memento-cli/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::bytes)
        .map(|bytes| bytes.to_vec())
        .map_err(|error| CliError::other(format!("cannot download asset {name}: {error}")))
}

fn verify_sha256(data: &[u8], checksum: &[u8]) -> Result<(), CliError> {
    use sha2::{Digest, Sha256};
    let expected = std::str::from_utf8(checksum)
        .map_err(|error| CliError::other(format!("invalid checksum: {error}")))?
        .split_whitespace()
        .next()
        .ok_or_else(|| CliError::other("empty checksum"))?;
    let actual = format!("{:x}", Sha256::digest(data));
    if expected != actual {
        return Err(CliError::other("release checksum mismatch"));
    }
    Ok(())
}

fn extract_binaries(archive: &[u8]) -> Result<Vec<(String, Vec<u8>)>, CliError> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| CliError::other(format!("cannot read release archive: {error}")))?;
    let mut binaries = Vec::new();
    for entry in entries {
        let mut entry = entry
            .map_err(|error| CliError::other(format!("cannot read archive entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| CliError::other(format!("invalid archive path: {error}")))?;
        let mut components = path.components();
        let name = match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => name.to_string_lossy().to_string(),
            _ => return Err(CliError::other("release archive contains an unsafe path")),
        };
        if !["memento", "memento-mcp", "mementod"].contains(&name.as_str())
            || !entry.header().entry_type().is_file()
        {
            return Err(CliError::other(format!(
                "unexpected release archive entry: {name}"
            )));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)
            .map_err(|error| CliError::other(format!("cannot extract {name}: {error}")))?;
        binaries.push((name, bytes));
    }
    if binaries.len() != 3
        || !binaries.iter().any(|(name, _)| name == "memento")
        || !binaries.iter().any(|(name, _)| name == "memento-mcp")
        || !binaries.iter().any(|(name, _)| name == "mementod")
    {
        return Err(CliError::other(
            "release archive must contain memento, memento-mcp and mementod",
        ));
    }
    Ok(binaries)
}

fn refresh(cli: &Cli) -> Result<(), CliError> {
    let receipt = crate::receipt::read().map_err(CliError::other)?;
    if receipt.get("setup").is_some() {
        crate::setup::reapply_saved_setup(cli)?;
        for step in crate::daemon::managed_refresh_steps(
            receipt.get("setup").and_then(serde_json::Value::as_object),
        ) {
            match step {
                crate::daemon::RefreshStep::Pull => crate::daemon::pull()?,
                crate::daemon::RefreshStep::Up => crate::daemon::start(cli)?,
            }
        }
        if let Some(setup) = receipt.get("setup").and_then(serde_json::Value::as_object)
            && crate::daemon::saved_mode(Some(setup))? == crate::daemon::DaemonMode::Native
        {
            crate::daemon::restart_native(cli, setup)?;
        }
    } else {
        println!("nothing to refresh");
    }
    Ok(())
}

pub(super) fn run(_cli: &Cli, args: &UpdateArgs) -> Result<(), CliError> {
    if args.refresh {
        return refresh(_cli);
    }
    let current = env!("CARGO_PKG_VERSION");
    let target = match &args.version {
        Some(version) => version.trim_start_matches('v').to_string(),
        None => release_version()?,
    };
    let is_newer = version_tuple(&target)? > version_tuple(current)?;
    if !is_newer {
        println!("memento {current} is up to date");
        return Ok(());
    }
    if args.check {
        println!("memento {target} is available (current: {current})");
        return Ok(());
    }
    let receipt = crate::receipt::read().map_err(CliError::other)?;
    let bin_dir = receipt
        .get("bin_dir")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            CliError::other("no installer receipt; reinstall with the curl installer")
        })?;
    let executable = env::current_exe()
        .map_err(|error| CliError::other(format!("cannot determine executable: {error}")))?
        .canonicalize()
        .map_err(|error| CliError::other(format!("cannot resolve executable: {error}")))?;
    let bin_dir = Path::new(bin_dir)
        .canonicalize()
        .map_err(|error| CliError::other(format!("cannot resolve install directory: {error}")))?;
    if receipt.get("method").and_then(serde_json::Value::as_str) != Some("script")
        || !inside(&executable, &bin_dir)
    {
        return Err(CliError::other(
            "binary replacement requires a script installation running from its recorded bin_dir; use cargo install --path crates/memento-cli or reinstall with the curl installer",
        ));
    }
    let base = env::var("MEMENTO_DOWNLOAD_BASE").unwrap_or_else(|_| {
        format!("https://github.com/luckynee/memento/releases/download/v{target}")
    });
    let asset = format!("memento-{}.tar.gz", env!("MEMENTO_TARGET"));
    let archive = download_asset(&base, &asset)?;
    let checksum = download_asset(&base, &format!("{asset}.sha256"))?;
    verify_sha256(&archive, &checksum)?;
    let binaries = extract_binaries(&archive)?;
    let temporary = bin_dir.join(format!(
        ".memento-update-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| CliError::other(format!("system clock error: {error}")))?
            .as_nanos()
    ));
    fs::create_dir(&temporary)
        .map_err(|error| CliError::other(format!("cannot create update directory: {error}")))?;
    for (name, bytes) in binaries {
        let staged = temporary.join(&name);
        fs::write(&staged, bytes)
            .map_err(|error| CliError::other(format!("cannot stage {name}: {error}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
                .map_err(|error| CliError::other(format!("cannot chmod {name}: {error}")))?;
        }
        fs::rename(&staged, bin_dir.join(&name))
            .map_err(|error| CliError::other(format!("cannot replace {name}: {error}")))?;
    }
    fs::remove_dir(&temporary)
        .map_err(|error| CliError::other(format!("cannot remove update directory: {error}")))?;
    crate::receipt::merge_fields(serde_json::json!({"version": target}))
        .map_err(|error| CliError::other(format!("cannot update receipt: {error}")))?;
    let status = Command::new(bin_dir.join("memento"))
        .arg("--url")
        .arg(&_cli.url)
        .env("MEMENTO_TOKEN", _cli.token.as_deref().unwrap_or_default())
        .args(["update", "--refresh"])
        .status()
        .map_err(|error| CliError::other(format!("cannot run post-update refresh: {error}")))?;
    if !status.success() {
        return Err(CliError::other(format!(
            "post-update refresh exited with {status}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_compares_numeric_versions() {
        assert_eq!(version_tuple("v1.12.3").unwrap(), (1, 12, 3));
        assert!(version_tuple("1.2").is_err());
        assert!(version_tuple("1.x.0").is_err());
        assert!(version_tuple("1.10.0").unwrap() > version_tuple("1.9.9").unwrap());
    }

    #[test]
    fn rejects_checksum_mismatch() {
        assert!(verify_sha256(b"abc", b"bad  file").is_err());
    }

    #[test]
    fn rejects_unexpected_and_traversal_archive_entries() {
        fn archive_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            for (path, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                if *path == "../memento" {
                    header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
                } else {
                    header.set_path(path).unwrap();
                }
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                archive.append(&header, *bytes).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap()
        }

        let unexpected = archive_with(&[
            ("memento", b"cli"),
            ("memento-mcp", b"mcp"),
            ("extra", b"unexpected"),
        ]);
        assert!(extract_binaries(&unexpected).is_err());

        let traversal = archive_with(&[("../memento", b"escape"), ("memento-mcp", b"mcp")]);
        assert!(extract_binaries(&traversal).is_err());
    }

    #[test]
    fn extracts_all_expected_archive_binaries() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (path, bytes) in [
            ("memento", b"cli".as_slice()),
            ("memento-mcp", b"mcp"),
            ("mementod", b"daemon"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, path, bytes).unwrap();
        }
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let extracted = extract_binaries(&bytes).unwrap();
        assert_eq!(extracted.len(), 3);
        assert!(extracted.contains(&("memento".into(), b"cli".to_vec())));
        assert!(extracted.contains(&("memento-mcp".into(), b"mcp".to_vec())));
        assert!(extracted.contains(&("mementod".into(), b"daemon".to_vec())));
    }

    #[test]
    fn binary_path_check_is_component_based() {
        assert!(inside(Path::new("/tmp/bin/memento"), Path::new("/tmp/bin")));
        assert!(!inside(
            Path::new("/tmp/bin2/memento"),
            Path::new("/tmp/bin")
        ));
    }
}
