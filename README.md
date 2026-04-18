# copilot-helix

`copilot-helix` is a small Rust binary that lets Helix use GitHub Copilot by
proxying Helix LSP completion requests to the official Copilot language server.

## How to build and install

```sh
git clone https://github.com/jheronimus/copilot-helix
cd copilot-helix
cargo install --path .
```

This compiles a release binary and installs it to `~/.cargo/bin/copilot-helix`, which is on your `$PATH` after a standard Rust installation.

## First run

Run:

```sh
copilot-helix
```

The first-run flow checks three things in order:

1. whether the pinned Copilot language server is installed in the local cache
2. whether GitHub Copilot authentication is already set up
3. whether your Helix `languages.toml` already references `copilot-helix`

If the language server is missing, the tool offers to install it with `npx`.
If you are not authenticated, it offers to run the GitHub device auth flow.
If Helix is not configured yet, it prints an example snippet and exits so you
can append it manually.

Manual helper commands are still available:

```sh
copilot-helix --install-ls
copilot-helix --auth
```

## Configuration

Add `copilot` as a language server in `~/.config/helix/languages.toml` and use
`copilot-helix --stdio` as the command Helix launches:

```toml
[language-server.copilot]
command = "copilot-helix"
args = ["--stdio"]

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "copilot"]
```

You can add `copilot` to other language entries the same way.

Optional environment variables:

- `COPILOT_LS_PATH`: use a specific `language-server.js` instead of the cached install
- `COPILOT_NODE`: use a specific `node` executable
- `HX_VERSION`: override the Helix version string reported upstream
- `RUST_LOG`: enable proxy logging, for example `RUST_LOG=debug copilot-helix --stdio`
