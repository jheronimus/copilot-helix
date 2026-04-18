# copilot-helix

GitHub Copilot completions in [Helix](https://helix-editor.com).

Helix has no plugin system, but has first-class LSP support. This project ships
a small Rust binary (`copilot-helix`) that acts as an LSP proxy between Helix
and the official Copilot language server — translating Copilot's
`textDocument/inlineCompletion` extension into the standard
`textDocument/completion` that Helix already understands.

```
Helix  ←→  copilot  ←→  copilot-language-server  ←→  GitHub API
```

Copilot completions appear in the same popup as your primary language server
(rust-analyzer, gopls, etc.) and require no extra key bindings.

---

## Requirements

| Dependency | Version |
|---|---|
| [Rust](https://rustup.rs) | 1.75+ (build only) |
| [Node.js](https://nodejs.org) | ≥ 22 with `npm` available |
| [Helix](https://helix-editor.com) | 23.x+ |
| GitHub Copilot subscription | Individual / Business / Enterprise |

---

## Installation

```sh
# Clone the proxy
git clone https://github.com/jheronimus/copilot-helix
cd copilot-helix

# Build the release binary
cargo build --release

# Install the pinned Copilot language server into the local cache
./target/release/copilot-helix --install-ls

# Copy to somewhere on your PATH (pick one)
cp target/release/copilot-helix ~/.local/bin/
# or: sudo cp target/release/copilot-helix /usr/local/bin/
```

> **Note** — by default the proxy launches a pinned cached
> `language-server.js` from your OS cache directory. If the cached install is
> missing, run `copilot-helix --install-ls`. To use a local `language-server.js`
> instead, set `COPILOT_LS_PATH`.

---

## Authentication

Run this once to authenticate with your GitHub account:

```sh
copilot-helix --auth
```

If this is a fresh install, run `copilot-helix --install-ls` first.

It prints a URL and a one-time code, then waits while you complete the browser
flow.  Your credentials are stored in the OS keychain by the language server and
are reused on every subsequent start.

---

## Helix configuration

Add `copilot` as an additional language server in
`~/.config/helix/languages.toml`:

```toml
[language-server.copilot]
command = "copilot-helix"
args = ["--stdio"]

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "copilot"]

[[language]]
name = "go"
language-servers = ["gopls", "copilot"]

[[language]]
name = "python"
language-servers = ["pyright", "copilot"]

[[language]]
name = "typescript"
language-servers = ["typescript-language-server", "copilot"]
```

Restart Helix and open a file.  Completions from Copilot appear in the standard
completion menu (default: `<C-x>` or automatic on trigger characters).

---

## Environment variables

| Variable | Purpose |
|---|---|
| `COPILOT_LS_PATH` | Absolute path to `language-server.js` (uses `node <path> --stdio` instead of the cached install) |
| `COPILOT_NODE` | Absolute path to the `node` executable used to launch either the cached install or `COPILOT_LS_PATH` |
| `HX_VERSION` | Helix version string reported to Copilot (cosmetic) |
| `RUST_LOG` | Log level for `copilot-helix` itself, e.g. `RUST_LOG=debug` |

Logs are written to **stderr** so they never interfere with LSP framing on
stdout.  To capture them:

```sh
RUST_LOG=debug copilot-helix --stdio 2>/tmp/copilot.log
```

---

## How it works

The proxy intercepts two things and passes everything else straight through:

1. **`initialize`** — injects `editorInfo` / `editorPluginInfo` so Copilot
   identifies the client correctly, and sends `workspace/didChangeConfiguration`
   after the handshake.

2. **`textDocument/completion`** — rewrites the request to
   `textDocument/inlineCompletion` (Copilot's custom method), then maps the
   `InlineCompletionItem` response back to standard `CompletionItem`s that Helix
   can render.

---

## References

- Published language server package: [@github/copilot-language-server](https://github.com/github/copilot.vim/tree/release/copilot-language-server)
- Protocol source of truth: [autoload/copilot/client.vim](https://github.com/github/copilot.vim/blob/release/autoload/copilot/client.vim)

---

## License

MIT
