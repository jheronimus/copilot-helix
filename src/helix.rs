use anyhow::{Context, Result};
use directories::BaseDirs;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelixConfigStatus {
    pub is_configured: bool,
    pub server_name: Option<String>,
    pub path: PathBuf,
}

pub fn languages_toml_path() -> Result<PathBuf> {
    let base_dirs = BaseDirs::new().context("could not determine home/config directory")?;
    Ok(base_dirs.config_dir().join("helix").join("languages.toml"))
}

pub fn detect_languages_toml() -> Result<HelixConfigStatus> {
    let path = languages_toml_path()?;
    let contents = std::fs::read_to_string(&path).ok();
    let mut status = analyze_languages_toml(contents.as_deref())?;
    status.path = path;
    Ok(status)
}

pub fn analyze_languages_toml(contents: Option<&str>) -> Result<HelixConfigStatus> {
    let Some(contents) = contents else {
        return Ok(HelixConfigStatus {
            is_configured: false,
            server_name: None,
            path: PathBuf::new(),
        });
    };

    let value: toml::Value = contents.parse().context("parsing Helix languages.toml")?;
    let matching_server = find_copilot_server_name(&value);
    let is_configured = matching_server
        .as_deref()
        .is_some_and(|server_name| language_uses_server(&value, server_name));

    Ok(HelixConfigStatus {
        is_configured,
        server_name: matching_server,
        path: PathBuf::new(),
    })
}

fn find_copilot_server_name(value: &toml::Value) -> Option<String> {
    value
        .get("language-server")
        .and_then(toml::Value::as_table)
        .and_then(|servers| {
            servers.iter().find_map(|(server_name, server_value)| {
                let command = server_value.get("command")?.as_str()?;
                command_points_to_copilot_helix(command).then(|| server_name.clone())
            })
        })
}

fn language_uses_server(value: &toml::Value, server_name: &str) -> bool {
    value
        .get("language")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .any(|language| {
            language
                .get("language-servers")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
                .any(|entry| entry == server_name)
        })
}

fn command_points_to_copilot_helix(command: &str) -> bool {
    let path = Path::new(command);
    let Some(file_name) = path.file_name().or_else(|| (!command.is_empty()).then_some(OsStr::new(command))) else {
        return false;
    };

    file_name == OsStr::new("copilot-helix") || file_name == OsStr::new("copilot-helix.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_missing_config_file_as_unconfigured() {
        let status = analyze_languages_toml(None).expect("status");

        assert!(!status.is_configured);
    }

    #[test]
    fn requires_matching_server_block_and_language_reference() {
        let status = analyze_languages_toml(Some(
            r#"
[language-server.copilot]
command = "copilot-helix"
args = ["--stdio"]
"#,
        ))
        .expect("status");

        assert!(!status.is_configured);
    }

    #[test]
    fn accepts_absolute_copilot_helix_command_with_language_usage() {
        let status = analyze_languages_toml(Some(
            r#"
[language-server.ai]
command = "/tmp/copilot-helix"
args = ["--stdio"]

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "ai"]
"#,
        ))
        .expect("status");

        assert!(status.is_configured);
        assert_eq!(status.server_name.as_deref(), Some("ai"));
    }
}
