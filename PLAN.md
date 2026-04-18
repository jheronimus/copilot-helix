# Implementation Plan

## Architecture

```
Helix (LSP client)
  ↕ stdio (standard LSP)
  copilot  [Rust binary]
  ↕ stdio (LSP + Copilot extensions)
  copilot-language-server  [cached pinned npm package]
  ↕ HTTPS
  GitHub Copilot API
```

Helix supports multiple LSP servers per language — `copilot` coexists with
`rust-analyzer`, `gopls`, etc. and Helix merges completion results.

## Source layout

```
Cargo.toml
src/
  main.rs          entry point, CLI flags (--stdio, --auth)
  jsonrpc.rs       JSON-RPC 2.0 types + Content-Length framing
  proxy.rs         bidirectional message loop (two tokio tasks)
  upstream.rs      subprocess lifecycle for the resolved launch command
  router.rs        PassThrough / Intercept / Drop per method+direction
  translator.rs    completion ↔ inlineCompletion protocol translation
  initializer.rs   LSP init handshake + Copilot-specific setup
  auth.rs          checkStatus at startup; --auth OAuth flow
  config.rs        resolve `node <path>` or the cached pinned language server
  installer.rs     `--install-ls` cache installation workflow
```

## Cargo.toml dependencies

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "process", "io-util", "sync", "time", "macros"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

## Steps

| # | Module | What it does | Testable independently |
|---|---|---|---|
| 1 | `jsonrpc.rs` | Content-Length framing, `Message` type, read/write | Unit tests |
| 2 | `upstream.rs` | Spawn language server subprocess via tokio | Echo script |
| 3 | `proxy.rs` + `router.rs` | Pure pass-through loop | Hover/go-to-def in Helix |
| 4 | `initializer.rs` | Inject `editorInfo`, buffer pre-init messages | Log inspection |
| 5 | `translator.rs` | `textDocument/completion` ↔ `textDocument/inlineCompletion` | Unit + integration tests |
| 6 | `auth.rs` | `checkStatus` at startup; `--auth` OAuth CLI | Manual run |
| 7 | Telemetry | `notifyAccepted`/`notifyRejected` | v2 candidate |

## Step 5 — protocol translation (core)

**Request (Helix → upstream)**

| Helix field | Upstream field | Note |
|---|---|---|
| method `textDocument/completion` | method `textDocument/inlineCompletion` | rename |
| `params.textDocument` | same | pass-through |
| `params.position` | same | pass-through |
| `params.context.triggerKind` | same | pass-through |
| — | `params.formattingOptions` | inject `{insertSpaces:true,tabSize:2}` |

Store `proxy_id → original_helix_id` for response correlation.

**Response (upstream → Helix)** — each `InlineCompletionItem` becomes a `CompletionItem`:

- `label` = first line of `insertText`, max 80 chars
- `kind = 1` (Text), `insertTextFormat = 1` (PlainText)
- `textEdit = {range, newText: insertText}`
- `sortText = "0001"`, `"0002"`, … (preserve order)
- `data._copilot_command` stashes `item.command` for telemetry

`$/cancelRequest` from Helix: remap Helix ID → proxy upstream ID before forwarding.

## Helix configuration

```toml
# ~/.config/helix/languages.toml

[language-server.copilot]
command = "copilot-helix"
args = ["--stdio"]

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "copilot"]
```

## Verification

1. `cargo test` — unit tests (jsonrpc, translator, router)
2. Integration test with fake upstream Node.js fixture server
3. Manual: open a file in Helix, trigger completion, verify Copilot results appear
4. Auth: verify `window/showMessage` when unauthenticated; run `copilot-helix --auth`
