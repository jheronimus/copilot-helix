//! Installs the Copilot language server globally via npm.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::Command;

use crate::config;

/// Install the pinned Copilot language server globally with `npm install -g`
/// and return the resolved `language-server.js` path.
pub async fn install_language_server() -> Result<PathBuf> {
    let npm = config::npm_path()?;
    let status = Command::new(&npm)
        .args(install_command_args())
        .status()
        .await
        .with_context(|| format!("running {} install -g", npm.display()))?;

    if !status.success() {
        anyhow::bail!(
            "npm global install failed with status {} while installing {}",
            status,
            config::package_spec()
        );
    }

    config::global_language_server_path()
}

fn install_command_args() -> Vec<String> {
    vec![
        "install".to_string(),
        "-g".to_string(),
        config::package_spec(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_command_uses_npm_global() {
        let args = install_command_args();

        assert_eq!(args[0], "install");
        assert_eq!(args[1], "-g");
        assert!(args[2].contains("@github/copilot-language-server"));
    }
}
