# copilot-helix

A thin Rust LSP proxy that makes GitHub Copilot work in Helix editor.

Helix has no plugin system but has first-class LSP support. The Copilot language
server uses a custom `textDocument/inlineCompletion` method that Helix doesn't
understand. This proxy sits between Helix and the language server, translating
`textDocument/completion` ↔ `textDocument/inlineCompletion` and forwarding
everything else transparently.

```
Helix → copilot (this project) → copilot-language-server → GitHub API
```

## Repository layout

- `src/` — Rust proxy source (see PLAN.md for module breakdown)

## References

- Implementation plan: [PLAN.md](PLAN.md)
- Copilot language server package: <https://github.com/github/copilot.vim/tree/release/copilot-language-server>
- Protocol source of truth: <https://github.com/github/copilot.vim/blob/release/autoload/copilot/client.vim>
