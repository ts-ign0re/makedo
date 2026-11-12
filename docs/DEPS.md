# DEPS — dependency register

For each dependency: why it is here, what was rejected, what it costs. Gate: growth in transitive dependencies needs a justification in the PR. See PLAN §4.

| Crate | Why | Rejected | Cost |
|---|---|---|---|
| serde, serde_json | serialization of everything | — | baseline |
| thiserror | typed errors in the library crates | snafu | zero (macro) |
| anyhow | errors in the binary | — | small |
| toml | config | — | small |
| tokio (min features) + tokio-util | runtime, CancellationToken | async-std (dead) | large, no alternative |
| reqwest (rustls, no default) | HTTP/SSE for providers, models.dev | ureq (no async) | large; rustls means cross-builds without system libraries |
| futures-util, bytes | streams | — | small |
| schemars | JSON schemas for tools | hand-written schemas (error-prone) | medium, compile-time |
| ignore + globset + grep-searcher/grep-regex | walk/glob/grep (the ripgrep engine as a library) | an rg subprocess (missing on old machines) | medium |
| rmcp (client + 2 transports) | MCP | a hand-written client (the protocol keeps moving) | medium |
| rustls (ring) | crypto provider for rmcp's HTTP client: without it MCP over https does not work | aws-lc-rs (heavy C build) | zero: already in the tree through reqwest |
| ratatui + crossterm | TUI | cursive (heavier) | medium |
| rustix (process) | `kill(2)` on a process group without unsafe | the external `kill` command (on Linux it misparses a negative pgid), nix (not in the tree) | zero: already in the tree through crossterm |
| ratatui-wgpu + winit (only the `gui` feature) | the `makedo app` window mode and `Makedo.app` | libghostty (no emulator needed: we draw the TUI ourselves), egui_ratatui (pulls in all of egui) | large: +3.8 MB to the binary (15.2 against 11.4), so it stays out of the default build |
| Hand-written: BM25, SSE decoder, frontmatter/wikilinks parser, spinner frames (codegen) | — | tantivy, reqwest-eventsource, serde_yaml, nom | zero |
