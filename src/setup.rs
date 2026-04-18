use anyhow::Result;
use std::io::{self, Write};

use crate::{auth, config, helix, installer};

pub async fn run_setup() -> Result<()> {
    if config::cached_language_server_path_if_exists()?.is_none() {
        println!("GitHub Copilot language server is not installed in the local cache.");
        if !prompt_yes_no("Install it now with npx? [y/N] ")? {
            println!("Setup stopped without installing the language server.");
            return Ok(());
        }

        let installed_path = installer::install_language_server().await?;
        println!(
            "Installed GitHub Copilot language server {} at {}",
            config::PACKAGE_VERSION,
            installed_path.display()
        );
    }

    match auth::check_auth_status().await? {
        auth::AuthStatus::Authenticated(Some(user)) => {
            println!("GitHub Copilot is authenticated as {user}.");
        }
        auth::AuthStatus::Authenticated(None) => {
            println!("GitHub Copilot is already authenticated.");
        }
        auth::AuthStatus::Unauthenticated(_) => {
            println!("GitHub Copilot is not authenticated.");
            if !prompt_yes_no("Authenticate now? [y/N] ")? {
                println!("Setup stopped without authentication.");
                return Ok(());
            }
            auth::run_auth_flow().await?;
        }
    }

    let helix_status = helix::detect_languages_toml()?;
    if !helix_status.is_configured {
        println!(
            "Helix is not configured to use copilot-helix in {}.",
            helix_status.path.display()
        );
        println!("Append this example to your languages.toml:");
        println!();
        println!("{}", helix_config_example());
        println!();
        println!("Then run `copilot-helix` again.");
        return Ok(());
    }

    println!("copilot-helix setup is complete.");
    Ok(())
}

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(parse_yes_no(&input))
}

fn parse_yes_no(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn helix_config_example() -> &'static str {
    r#"[language-server.copilot]
command = "copilot-helix"
args = ["--stdio"]

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "copilot"]"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_decline_is_false() {
        assert!(!parse_yes_no("n"));
        assert!(!parse_yes_no(""));
    }

    #[test]
    fn prompt_accepts_yes_variants() {
        assert!(parse_yes_no("y"));
        assert!(parse_yes_no("Yes"));
    }

    #[test]
    fn setup_example_uses_stdio_command() {
        let example = helix_config_example();

        assert!(example.contains("command = \"copilot-helix\""));
        assert!(example.contains("args = [\"--stdio\"]"));
        assert!(example.contains("language-servers = [\"rust-analyzer\", \"copilot\"]"));
    }
}
