//! Resolves the command used to launch the Copilot language server.

use anyhow::{bail, Context, Result};
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    process::Command,
};

const PACKAGE_NAME: &str = "@github/copilot-language-server";
pub const PACKAGE_VERSION: &str = "1.472.0";
const STDIO_FLAG: &str = "--stdio";
/// Path of the language server script relative to the npm global node_modules root.
const GLOBAL_SCRIPT_RELATIVE_PATH: &str = "@github/copilot-language-server/dist/language-server.js";

/// Runtime command needed to start the language server subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Executable to spawn.
    pub program: PathBuf,
    /// Arguments passed to `program`.
    pub args: Vec<OsString>,
}

impl Config {
    /// Detect the runtime command from environment variables or the global npm install.
    ///
    /// Search order:
    /// - `COPILOT_LS_PATH` + (`COPILOT_NODE` or `node` on `$PATH`)
    /// - globally installed package resolved via `npm root -g`
    pub fn detect() -> Result<Self> {
        Self::detect_with(overrides_from_env(), which, global_language_server_path)
    }

    fn detect_with<F, G>(
        overrides: EnvOverrides,
        mut lookup_path: F,
        resolve_global_path: G,
    ) -> Result<Self>
    where
        F: FnMut(&str) -> Option<PathBuf>,
        G: FnOnce() -> Result<PathBuf>,
    {
        if let Some(language_server_path) = overrides.language_server_path {
            validate_readable_file(&language_server_path, "COPILOT_LS_PATH")?;
            let program = locate_node(overrides.node_path, &mut lookup_path)?;
            return Ok(Self {
                program,
                args: vec![language_server_path.into_os_string(), STDIO_FLAG.into()],
            });
        }

        let language_server_path = resolve_global_path()?;
        let program = locate_node(overrides.node_path, &mut lookup_path)?;
        Ok(Self {
            program,
            args: vec![language_server_path.into_os_string(), STDIO_FLAG.into()],
        })
    }
}

#[derive(Debug, Default)]
struct EnvOverrides {
    node_path: Option<PathBuf>,
    language_server_path: Option<PathBuf>,
}

fn overrides_from_env() -> EnvOverrides {
    EnvOverrides {
        node_path: std::env::var_os("COPILOT_NODE").map(PathBuf::from),
        language_server_path: std::env::var_os("COPILOT_LS_PATH").map(PathBuf::from),
    }
}

pub fn package_spec() -> String {
    format!("{PACKAGE_NAME}@{PACKAGE_VERSION}")
}

/// Returns the npm global `node_modules` directory by running `npm root -g`.
pub fn npm_global_root() -> Result<PathBuf> {
    let npm = npm_path()?;
    let output = Command::new(&npm)
        .args(["root", "-g"])
        .output()
        .with_context(|| format!("running {} root -g", npm.display()))?;

    if !output.status.success() {
        bail!("npm root -g failed with status {}", output.status);
    }

    let root = String::from_utf8(output.stdout).context("npm root -g output is not valid UTF-8")?;

    Ok(PathBuf::from(root.trim()))
}

pub fn global_language_server_path() -> Result<PathBuf> {
    let script_path = global_language_server_path_for(&npm_global_root()?);
    validate_readable_file(&script_path, "Copilot language server").with_context(|| {
        format!(
            "Copilot language server v{PACKAGE_VERSION} not found in global npm install. \
             Run `copilot-helix --install-ls` or set COPILOT_LS_PATH=/path/to/language-server.js"
        )
    })?;
    Ok(script_path)
}

pub fn global_language_server_path_if_exists() -> Result<Option<PathBuf>> {
    let npm_root = match npm_global_root() {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let script_path = global_language_server_path_for(&npm_root);
    if !script_path.is_file() {
        return Ok(None);
    }
    validate_readable_file(&script_path, "Copilot language server")?;
    Ok(Some(script_path))
}

pub fn npm_path() -> Result<PathBuf> {
    which("npm").context("npm not found in $PATH — install Node.js/npm ≥22")
}

fn locate_node<F>(explicit_node_path: Option<PathBuf>, lookup_path: &mut F) -> Result<PathBuf>
where
    F: FnMut(&str) -> Option<PathBuf>,
{
    match explicit_node_path {
        Some(node_path) => validate_existing_file(node_path, "COPILOT_NODE"),
        None => lookup_path("node").context(
            "node not found in $PATH — install Node.js ≥22 or set \
             COPILOT_NODE=/path/to/node",
        ),
    }
}

pub fn global_language_server_path_for(npm_root: &Path) -> PathBuf {
    npm_root.join(GLOBAL_SCRIPT_RELATIVE_PATH)
}

fn validate_existing_file(path: PathBuf, var_name: &str) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path);
    }

    bail!("{var_name}={:?} does not point to an existing file", path);
}

fn validate_readable_file(path: &Path, name: &str) -> Result<()> {
    if !path.is_file() {
        bail!("{name}={:?} does not point to an existing file", path);
    }

    File::open(path).with_context(|| format!("{name}={path:?} is not readable"))?;
    Ok(())
}

/// Find `name` on `$PATH`, returning the first match as an absolute path.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn detect_prefers_local_override_when_set() {
        let node_path = temp_file("node");
        let language_server_path = temp_file("language-server.js");
        let overrides = EnvOverrides {
            node_path: Some(node_path.clone()),
            language_server_path: Some(language_server_path.clone()),
        };

        let config = Config::detect_with(overrides, |_| None, || unreachable!()).unwrap();

        assert_eq!(config.program, node_path);
        assert_eq!(
            config.args,
            vec![
                language_server_path.into_os_string(),
                OsString::from(STDIO_FLAG),
            ]
        );
    }

    #[test]
    fn invalid_local_override_errors() {
        let node_path = temp_file("node");
        let overrides = EnvOverrides {
            node_path: Some(node_path),
            language_server_path: Some(PathBuf::from("/nonexistent/language-server.js")),
        };

        let err = Config::detect_with(overrides, |_| None, || unreachable!()).unwrap_err();

        assert!(err.to_string().contains("COPILOT_LS_PATH"));
    }

    #[test]
    fn detect_uses_global_install_by_default() {
        let node_path = PathBuf::from("/usr/bin/node");
        let global_path = PathBuf::from(
            "/usr/local/lib/node_modules/@github/copilot-language-server/dist/language-server.js",
        );

        let config = Config::detect_with(
            EnvOverrides::default(),
            |name| (name == "node").then(|| node_path.clone()),
            || Ok(global_path.clone()),
        )
        .unwrap();

        assert_eq!(config.program, node_path);
        assert_eq!(
            config.args,
            vec![global_path.into_os_string(), OsString::from(STDIO_FLAG)]
        );
    }

    #[test]
    fn missing_global_install_without_override_errors() {
        let node_path = PathBuf::from("/usr/bin/node");
        let err = Config::detect_with(
            EnvOverrides::default(),
            |name| (name == "node").then(|| node_path.clone()),
            || anyhow::bail!("global install missing"),
        )
        .unwrap_err();

        assert!(err.to_string().contains("global install missing"));
    }

    #[test]
    fn global_path_is_deterministic() {
        let npm_root = PathBuf::from("/usr/local/lib/node_modules");
        let script_path = global_language_server_path_for(&npm_root);

        assert_eq!(
            script_path,
            npm_root.join("@github/copilot-language-server/dist/language-server.js")
        );
    }

    fn temp_file(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "copilot-helix-config-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::write(&path, b"test").expect("failed to write temp file");
        path
    }
}
