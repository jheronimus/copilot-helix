use anyhow::{bail, Result};
use copilot_helix::{
    auth,
    config::{self, Config},
    setup,
    installer,
    proxy::Proxy,
    upstream::Upstream,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr so stdout stays clean for LSP framing.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("--stdio") => run_proxy().await,
        None => setup::run_setup().await,
        Some("--auth") => auth::run_auth_flow().await,
        Some("--install-ls") => install_language_server().await,
        Some(flag) => {
            bail!("unknown flag: {flag}\nUsage: copilot-helix [--stdio | --auth | --install-ls]");
        }
    }
}

async fn run_proxy() -> Result<()> {
    let config = Config::detect()?;
    let upstream = Upstream::spawn(&config).await?;
    let proxy = Proxy::new(upstream);
    proxy.run(tokio::io::stdin(), tokio::io::stdout()).await
}

async fn install_language_server() -> Result<()> {
    let installed_path = installer::install_language_server().await?;
    println!(
        "Installed GitHub Copilot language server {} at {}",
        config::PACKAGE_VERSION,
        installed_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_usage_mentions_stdio_explicitly() {
        let usage = "Usage: copilot-helix [--stdio | --auth | --install-ls]";
        assert!(usage.contains("--stdio"));
    }
}
