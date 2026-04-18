//! Installs the pinned Copilot language server into the local cache.

use anyhow::{Context, Result};
use std::{fs, process::Stdio};
use tokio::process::Command;

use crate::config;

/// Install the pinned Copilot language server into the local cache and return
/// the cached `language-server.js` path.
pub async fn install_language_server() -> Result<std::path::PathBuf> {
    let install_dir = config::cache_install_dir()?;
    let script_path = config::cached_language_server_path_for(&install_dir);

    if install_dir.exists() {
        fs::remove_dir_all(&install_dir)
            .with_context(|| format!("removing existing install at {}", install_dir.display()))?;
    }
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating install directory {}", install_dir.display()))?;

    let npm = config::npm_path()?;
    let status = Command::new(&npm)
        .arg("install")
        .arg("--no-save")
        .arg("--prefix")
        .arg(&install_dir)
        .arg(config::package_spec())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("running {} install", npm.display()))?;

    if !status.success() {
        anyhow::bail!(
            "npm install failed with status {} while installing {}",
            status,
            config::package_spec()
        );
    }

    verify_install(script_path)
}

fn verify_install(script_path: std::path::PathBuf) -> Result<std::path::PathBuf> {
    if !script_path.is_file() {
        anyhow::bail!("installed package is missing {}", script_path.display());
    }

    Ok(script_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_install_rejects_missing_script() {
        let err = verify_install(std::path::PathBuf::from("/nonexistent/language-server.js"))
            .expect_err("missing script should fail");

        assert!(err.to_string().contains("installed package is missing"));
    }
}
