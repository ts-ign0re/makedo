# makedo — development plan

A Rust coding-agent harness for machines that are 4–6 years old (8 GB RAM, 4 cores, no AVX2 guarantee).
This is a reference document: every decision is either justified or marked **[open question]**.
Basis: an analysis of grok-build (Rust, xAI) and opencode (TS/Bun) — we take what is proven, fix the bugs we found, and leave out the rest.

---

## 1. Design principles

### 1.1 Strict Rust conventions

**Edition / MSRV.** Edition 2024. MSRV = stable − 2 releases, pinned in `rust-toolchain.toml` and checked in CI. Rationale: we need RPITIT and async fn in traits (≥1.75), and there is no reason to chase nightly; "−2" gives distributions and contributors some slack.

**Lint policy** (workspace-level, `[workspace.lints]`):

```toml
[workspace.lints.rust]
unsafe_code = "deny"          # whole workspace; the only exception is a separate crate with a justification in that crate's README
missing_debug_implementations = "warn"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"          # in lib crates; tests — allow
expect_used = "deny"
panic = "deny"
dbg_macro = "deny"
todo = "deny"
large_enum_variant = "deny"   # a direct RAM metric
```

From pedantic we selectively `allow`: `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc` (at the start). We do not enable `nursery` wholesale (unstable false positives); we pick individual lints: `significant_drop_tightening` (it directly catches "guard held across .await" — a class of bugs documented in grok-build).

`clippy.toml`:
- `disallowed-methods`: `tokio::sync::mpsc::unbounded_channel` (backpressure is mandatory — grok-build uses an unbounded mpsc for tool results, we forbid it), `std::thread::sleep` (in async code), `std::process::exit` (except in the bin).
- `disallowed-types`: `std::sync::Mutex` in async crates (use either `parking_lot` with the rule "never hold across .await", or `tokio::sync`).
- `cognitive-complexity-threshold = 25`.

The lesson behind this: in grok-build the security-critical sandbox crate carries `#![allow(dead_code, unreachable_code, ...)]` — our review policy forbids per-file allow; only per-expression allow with a comment is permitted.

**Error handling by layer.**
- Lib crates: `thiserror`, typed error enums at the crate boundary. An error visible to the model (the text in a tool result) is a separate type/field, not the `Display` of an internal error: what the LLM reads is an API, and a refactoring must not change it by accident (lesson from opencode: `InvalidArgumentsError` with model-facing text is the right pattern).
- `bin`/TUI top layer: `anyhow` is allowed.
- Panics: forbidden on runtime paths (see the lints above). `panic = "abort"` in release ⇒ any panic kills the process, so a panic means only "a programmer invariant was violated", never input handling.

**Invariants through types.** Newtypes instead of `String`/`usize` wherever mixing values up is a real bug:
- `QualifiedToolName` — the constructor validates exactly one `__` boundary and the regex `^[A-Za-z_][A-Za-z0-9_-]{0,63}$` (the strictest common denominator across providers, from grok-build). The type itself fixes two bugs we found: `contains("__")` as the test for an MCP tool, and silent name collisions in opencode.
- `ToolId`, `ServerName`, `WikiNodeName`, `EdgeKind`, `ByteBudget`, `TokenEstimate`.
- Channels: bounded only; the capacity is a const with a comment that justifies it.

**The price of laziness (ponytail discipline).** Non-trivial logic (a parser, a locking scheme, a state machine) must leave behind a minimal runnable test. Deliberate simplifications are marked `// ponytail: <ceiling and upgrade path>` and collected into a ledger.

### 1.2 Performance principles for old hardware

**Principle #1: makedo knows what it is running on.** At startup it detects RAM/cores/disk (`makedo_types::resources`, algorithmically, with no dependencies) and derives a single `Budget` from them: the cap on parallel tools, the capture cap, the blocking pool. <8GB RAM = "tight" mode (we squeeze harder). Token savings follow the same principle: the ponytail skill (less code = fewer tokens) and the LLM wiki (fewer search iterations), and **the wiki is written in ponytail/caveman style** — telegraphic nodes without prose: every token saved is saved again on every future read.

**Budgets are CI gates, not wishes** (the numbers are starting values, to be refined after the first measurement on the reference machine; reference: MacBook Pro 2019 i5-8257U / 8 GB and a Linux laptop of the same class):

| Metric | Budget |
|---|---|
| Cold start → first TUI frame | < 150 ms |
| Idle RSS (TUI open, empty session) | < 50 MB |
| RSS in a typical session (excluding child processes) | < 300 MB |
| Deterministic wiki seed, 10k-file repo | < 5 s |
| `search_tool` p99 | < 5 ms |
| Binary per target (stripped) | < 15 MB |
| Clean CI build | < 5 min; incremental local build < 15 s |

**Allocation and data rules:**
- Large data goes to disk, not RAM: tool output over the limit is spilled to a file (both codebases do this; we also take grok-build's "steering" hints: JSON → "use jq").
- Streaming: a rolling window for bash output (the opencode model: 2×limit in memory, then a file sink); TUI deltas are append-only, with a 16 KiB cap per frame (the grok-build model, including UTF-8-safe boundaries).
- A `Cow<'_, str>` fast path in truncation (grok-build pattern), `with_capacity` when the size is known, an initial buffer capacity of ≤ 64 KiB even with large ceilings (grok-build grep pattern).
- No full snapshot clones per request (anti-pattern: BM25 in grok-build clones the whole schema catalog on every `search_tool`) — indexes are cached by generation.
- No re-serializing the world after every tool call (anti-pattern: `Resources::serialize` in grok-build) — only the changed component is persisted, tracked with dirty flags.

**Scheduler.** tokio, `worker_threads = min(4, cores)`, `max_blocking_threads` is capped (the default is 512 — on 8 GB that is potentially gigabytes of stacks). `spawn_blocking` is only for real blocking I/O (file system walks, git commands); CPU tasks > 1 ms (building BM25, parsing the vault) go there too, so they do not choke the reactor. Polling is forbidden: `Notify`/`watch` instead of sleep loops (anti-pattern: the 50 ms interjection polling in grok-build).

**Dependency policy.** Every dep is a row in `docs/DEPS.md`: why it is needed, what was rejected, the cost (transitive count, contribution to the binary per `cargo bloat`). CI metrics: the number of transitive deps (gate: growth needs a justification in the PR), `cargo deny` (licenses + advisories). Feature minimalism: `default-features = false` everywhere, features are enabled by name.

---

## 2. Workspace architecture

```
makedo/
├── crates/
│   ├── makedo-types      # shared vocabulary: newtypes, events, boundary errors. Depends only on serde
│   ├── makedo-config     # TOML, global→project merge, ~/.makedo layout
│   ├── makedo-tools      # Tool trait, registry, built-in tools, truncation, spills
│   ├── makedo-wiki       # vault parser, graph, BM25, init/seed, check
│   ├── makedo-skills     # SKILL.md loader, embedded ponytail
│   ├── makedo-providers  # Provider trait, Anthropic + OpenAI-compat, SSE decoder
│   ├── makedo-mcp        # rmcp client, lifecycle, meta-tools
│   ├── makedo-session    # session actor, agent loop, permissions, compaction
│   └── makedo-tui        # ratatui rendering, spinners, themes
└── src/main.rs         # bin `makedo`: CLI parsing, wiring, anyhow
```

**Dependency direction** (strictly downward; a cycle is a build error):

```
makedo-types ← makedo-config ← {tools, wiki, skills, providers} ← makedo-mcp ← makedo-session ← makedo-tui ← bin
```

- `makedo-mcp` depends on `makedo-tools` (MCP tools are registered in the shared registry; does it take the BM25 engine from `makedo-wiki`? — no: **BM25 lives in `makedo-types` as a small dependency-free module**, used by both wiki and mcp. ~150 lines, shared).
- `makedo-tui` does not know about providers/mcp directly — only session events (`makedo-types::Event`).
- The public API of each crate is minimal: `pub(crate)` by default, `pub` is a deliberate choice. `#[non_exhaustive]` on events and config structs.

**Trait seams — only where a second implementation is real:**
- `Provider` (≥ 2 implementations from day one).
- `TokenCounter` (bytes/4 now, BPE later — a seam from grok-build that they built and never used; at least we will not overpay for it, it is one trait object in session).
- `ToolDispatch` (an object-safe boundary for the use_tool → registry recursion).
Everything else is concrete types. No "interface with one implementation".

---

## 3. Key subsystems

### 3.1 Tool runtime (`makedo-tools`)

**Design.** A typed trait modeled on grok-build (the best there is in either codebase):

```rust
trait Tool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + Send;
    type Output: ToolOutput + Send;
    fn id(&self) -> ToolId;
    fn description(&self) -> &str;
    fn is_read_only(&self) -> bool;
    async fn run(&self, ctx: &ToolCtx, args: Self::Args) -> Result<Self::Output, ToolError>;
}
```

RPITIT on the typed path (zero boxing); the `#[async_trait]` equivalent is used only at the `ToolDyn` erasure boundary (a blanket impl does the JSON decode + the call). Progressive streaming (`ToolStream` with the invariant `[Progress*, Terminal]`) is **deferred**: in M0 tools return a single result, and only bash streams its output, through its own channel. `// ponytail: single-shot tool results; ToolStream when a second tool with progress appears`.

**Schemas.** `schemars` draft-07, `inline_subschemas = true` (both codebases inline `$ref` — providers cannot digest them), plus the LLM normalizations from opencode: remove `null` from the anyOf of optional fields, collapse single-element anyOf, guarantee `type/properties/required`. Schemas are generated once, when the registry is built, and cached by `ToolId`.

**Registry.** `IndexMap<ToolId, Arc<dyn ToolDyn>>` (order = declaration order, O(1) lookup — this fixes the linear scan over a Vec in grok-build). Tools are given to the model in **alphabetical order by name** (opencode: prompt cache stability). Runtime mutation (hot-adding MCP tools) goes through an RwLock, and the guard never lives across `.await` (a grok-build invariant; here it is enforced by the `significant_drop_tightening` lint).

**Parallelism.** What nobody else has:
- A `Semaphore` with `min(4, cores)` permits per batch of tool calls (opencode has no cap at all: 30 parallel bash calls = 30 processes; on 8 GB that is fatal).
- Per-path write locks from grok-build: paths from the arguments of the non-read-only calls in a batch each get a `Mutex`; they are acquired **in sorted path order** (grok locks one path per call; if a call touches several, sorting rules out deadlock).
- The permission phase runs sequentially before the batch; a denial synthesizes a tool_result for the remaining calls (the tool_use/tool_result pair is never broken — the invariant grok did this for).

**Cancellation.** A `CancellationToken` is actually threaded into every `ToolCtx` (grok built it and never wired it up — for us the wiring is part of the M0 DoD): turn cancellation → cancel token → tools with processes kill their children (SIGTERM → 3 s → SIGKILL, process group via setsid, grok-build pattern), the rest drop the future. Reconciliation: after cancellation every unfinished call gets a synthetic `interrupted` result (the opencode model, 250 ms grace).

**Output.** The default limit is 40 KB / 2000 lines (both codebases converge on ≈ 10k tokens); anything above is spilled to `~/.makedo/spill/<session>/<call>.{txt,json}` + content classification and a steering hint (JSON → jq, long lines → bash slicing; from grok-build) + an agent-aware variant (from opencode). Spills are retained for 7 days and cleaned up in the background.

**Built-in tools in M0:** `read` (limits: 2000 lines, 2000 chars/line), `write`, `edit` (string-replace), `bash` (fg/bg, default timeout 120 s, ceiling 10 min, rolling-window output), `grep`, `glob`, `todo`. Then: `webfetch` (cap 5 MB, timeout 30 s), wiki tools (§3.3), MCP meta-tools (§3.2). Deferred: task/subagents, lsp, websearch, media.

**Permissions.** The opencode model: an ordered ruleset `{permission, pattern, action: allow|ask|deny}`, the last match wins, the default is ask; session-level "always" rules auto-resolve pending requests. The doom-loop guard (N identical calls in a row → forced ask) is cheap and useful, so we take it. The LLM classifier of grok-build's Auto mode is **deferred** (YAGNI: one more LLM call for every bash command is expensive for our audience). Parsing a bash command into prefixes: a conservative split on `;|&&`, no tree-sitter in M0. **[open question]** tree-sitter-bash (accuracy) vs a hand-written split (0 dependencies): we decide based on the actual false positives after dogfooding.

**Risks:** the semaphore + per-path locks + the permission queue are three waiting mechanisms; a test pins down the absence of deadlock (we do not pull in loom — a property test with a timeout).

### 3.2 MCP (`makedo-mcp`)

**Client.** `rmcp` (the official SDK; rejected: a hand-written client — the protocol keeps moving, and that is not our job). If the rmcp version drags in a reqwest conflict, we quarantine it inside the crate, as grok-build does (we do not re-export reqwest).

**Exposure to the model — meta-tools; MCP schemas do not go into the manifest.** Reasons (from grok-build, confirmed by their production use): a stable tool set = the KV cache is not invalidated by a server that connects mid-session; the context is not bloated with schemas; RAM does not hold N×schema in every request.
- `search_tool(query, limit)` — BM25 over the catalog; we fix both grok-build bugs: the index is cached by catalog generation (rebuilt only on `tools/list_changed`), and `total_hidden_tools` = the tools that are actually hidden, not all of them.
- `use_tool(name, input)` — dispatch by `QualifiedToolName`; we take the "native tool correction" (a "call it directly" hint when the name matches a built-in tool).
- A collision of qualified names at registration = a loud server status error, not a silent overwrite (an opencode bug).

**Transports.** stdio + streamable HTTP (an SSE config maps to streamable HTTP, as in grok-build — there is no separate SSE transport).
- stdio framing is resilient (a broken line does not drop the connection, garbage notifications are ignored, we do not answer garbage with `-32600` — grok-build's rationale: the server can get into an echo loop), **but with a 10 MB line ceiling** — an unlimited `read_until` in grok-build = OOM from a hostile server. Exceeding it → a decode-error event + disconnect.
- Child processes: their own process group (setsid); on drop — killpg + reap; stderr goes to the server's log file.

**Lifecycle.** The grok-build state machine is taken as the reference almost verbatim (this is code written "after a series of incidents"): `Empty → Pending → Initializing → Ready`; single-flight handshake (competitors park on a `Notify` with a bounded wait), an RAII guard restores the state on cancellation, a generation counter discards the results of stale initializations, `client_id` in events guards against stale teardown. Startup: `buffer_unordered(4)` (grok: 8; our machines are weaker), Progressive strategy (the first prompt does not wait for all servers; `search_tool` honestly answers `status: "partial"`).

**Restart.** Auto-restart with backoff — stdio `[1, 4, 16] s`, HTTP up to ~2.5 min (rolling deploy) — taken from grok-build (opencode has no restart at all — a crashed server stays dead until someone intervenes manually). The guard against a double restart is an atomic claim + RAII.

**Notifications.** `tools/list_changed` → coalescing in a 50 ms window by `(server, kind)` (grok-build: a burst of 100 notifications = 1 event) → BM25 generation invalidation + an event in the TUI.

**Timeouts.** startup 30 s; tool call **60 s** by default (not 100 minutes, as in grok-build), with a per-tool override in the config and a `resetTimeoutOnProgress` equivalent.

**Config** — in `~/.makedo/config.toml` / `.makedo/config.toml`:

```toml
[mcp.github]
command = ["npx", "-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "..." }
startup_timeout_s = 30
tool_timeout_s = 60

[mcp.linear]
url = "https://mcp.linear.app/mcp"
headers = { Authorization = "Bearer ..." }
```

**Compatibility with the Claude Code ecosystem:** the protocol is shared (rmcp = the same spec), plus import of `.mcp.json` from the project root (the `mcpServers` map: a `command` string + `args` → our array; `type: http|sse|streamable-http`; `${VAR}` / `${VAR:-default}` substitution in command/args/env/url/headers). Precedent: grok-build imports `~/.claude.json` and `~/.cursor/mcp.json`. We do not support the WebSocket transport (YAGNI).

**Deferred:** OAuth (PKCE+DCR is a big chunk; bearer headers cover the start; the design references are ready: grok-build's two-layer dedup, opencode's pending-provider) — phase M3; MCP resources/prompts/sampling/elicitation (grok lives perfectly well without them); on-disk tool descriptors.

### 3.3 LLM Wiki (`makedo-wiki`)

**Format.** An Obsidian-compatible vault at `.makedo/wiki/`: markdown nodes, `[[wikilinks]]`, typed edges as inline fields `key:: [[Target]]` (the traceability-graph grammar: `satisfies`, `implemented-by`, `verified-by`, … — a default set that can be overridden in `schema.md`). Obsidian compatibility is a feature: a person opens the vault and sees the graph.

**Parser.** Hand-written (~100–200 lines): frontmatter (`serde_yaml`? — no: **[decision]** a minimal key: value parser, full YAML is not needed — `// ponytail: flat frontmatter; serde_yaml if nested structures appear`), wikilinks, inline fields. The graph is a `HashMap<NodeId, Node>` + edge lists; petgraph was rejected (we only need lookup and BFS traversal).

**Index.** The shared BM25 module (see §2): corpus = node name + headings + description; cached by generation (an mtime hash of the vault files); built in `spawn_blocking`. Hundreds of nodes = milliseconds and megabytes — we measure, we do not optimize in advance.

**Tools:** `wiki_node(name)` (the node + its incoming/outgoing edges), `wiki_trace(from, edge, depth)` (BFS by edge type). Node search goes through the single `memory_search` (the wiki is imported into the memory index).

**Init flow (mandatory when there is no wiki):**
1. No `.git` → offer `git init`; refusal = exit (requirement: the wiki lives in git).
2. `.makedo/wiki` is in `.gitignore` → warn/remove.
3. A deterministic seed without an LLM: a walk via `ignore` (respects .gitignore), manifests, the module tree, entry points → skeleton nodes. Progress: the `scan` spinner + a file counter.
4. One LLM pass **over the seed summary only** (not over the code) → the `index.md` map.
5. `git add .makedo/wiki`; the commit goes together with the user's work commit.
6. Enrichment is incremental, during the work (a duty of the agent, built into the system prompt).

**`makedo wiki check`** — a port of trace-check.js: no dead links, the edges the grammar requires are present; warnings vs errors. It runs before a wiki commit and is available to the user as a CI command.

**Invariants:** wiki operations never block the agent loop (everything goes through spawn_blocking + a channel); the vault is read lazily, and a full parse happens only when the index is built.

**Risk:** the quality of incremental enrichment depends on prompt discipline — the metric is "coverage": the % of files changed in a PR that have a node. We measure it while dogfooding.

### 3.4 Skills (`makedo-skills`)

A Claude-compatible format: `SKILL.md` with frontmatter (name, description) + references/. Sources: embedded (in the binary, `include_str!`) → `~/.makedo/skills/` → `.makedo/skills/`; a local skill overrides a global one by name.

**Ponytail** — embedded, active by default, the level is set in the config (`skills.ponytail = "full" | "lite" | "ultra" | false`). Before embedding it: check the license of DietrichGebert/ponytail (MIT expected) + a NOTICE file.

Injection: a section of the system prompt. Loading a skill dynamically on demand (the `skill` tool) is M2; in M0 the skills are simply in the system prompt (there are few of them).

### 3.5 Session / agent loop (`makedo-session`)

**Actor.** One session = one task that owns its state (history, todo, statuses) — no shared locks on the hot path (in grok-build this is the ChatState actor; they also provide the counter-example — a global `Mutex<Resources>` that chokes their own parallelism; our tool state is separate and is passed into `ToolCtx` through explicit `Arc` handles only where needed). Input is a bounded mpsc of commands, output is a `watch`/broadcast of events for the TUI. **[decision]** The actor is Send and runs on the shared runtime; `LocalSet`+`!Send` (as in grok) is an optimization we will return to only if Send boundaries start to cost noticeable clones.

**Loop:** `prepare tools → stream LLM → batch of tool calls (see 3.1) → push results → repeat until stop`. Tolerance for `finish: stop` when tool calls were emitted (real providers do this — opencode). Repair of wrong letter case in a tool name + an `invalid` sink for garbage calls (opencode) — cheap, and it saves turns.

**Tokens.** `bytes/4` (+765/image) through the `TokenCounter` trait — both codebases run this way in production. No BPE in M0.

**Compaction** — two levels in M1, a third level later:
1. Prune: hide old tool outputs from the model (not delete them — the UI sees everything), protecting the last ~40k tokens and the last 2 turns (opencode's numbers, proven in production).
2. Truncate on replay: old tool outputs down to 2000 characters.
3. Summarize agent — deferred (YAGNI until we actually hit context limits while dogfooding).
Invariant: tool_use/tool_result pairs are never broken (both engines honor this; Anthropic rejects a violation).

**Streaming:** SSE from the provider → events → deltas in the TUI (append-only, 16 KiB/frame cap, UTF-8-safe boundaries — the grok-build contract).

### 3.6 TUI (`makedo-tui`)

Full-screen, modeled on grok-build. `ratatui` + `crossterm` (rejected: cursive — heavier and less active; a hand-written renderer — not our job).

- Render only when dirty + on the tick of an active spinner; no "always 60 fps" — that costs battery and CPU on old machines.
- Spinners: frame sets from cli-loaders (`unicode-animations` → a static `loaders.rs`, MIT + NOTICE). The `process_kind → spinner` mapping is a fixed table (braillewave→LLM, snake→tool, columns→batch, breathe→idle). An ASCII fallback for terminals without braille glyphs (detected from TERM/locale + a manual flag) — our audience has old environments.
- Every long operation must show: a spinner + the phase + a counter (`⡿ indexing wiki — 142/380 files`).
- Customization from `~/.makedo/config.toml`: theme (palette), spinner mapping, keybindings.

### 3.7 Worktree mode (session)

Parallel work on features without collisions, through git worktrees:

- `makedo --worktree <name>` (or `/worktree <name>` in the TUI): creates the branch `makedo/<name>` and runs `git worktree add .makedo/worktrees/<name>`; the session works in that directory. Two makedo runs in two terminals = two isolated features; per-path locks do not affect them, because the files are physically different.
- `.makedo/.gitignore` is created automatically and contains `worktrees/` and `spill/` (the user may remove them — that choice is then respected: the file is not overwritten, only appended to when an entry is missing).
- **Finishing happens only on explicit request** (`/finish` in the TUI or `makedo finish <name>`): merge the branch `makedo/<name>` into the repository's main branch (the one checked out in the main tree), then `git worktree remove` + delete the branch. Uncommitted changes in the worktree are committed before the merge with a summary message. The merge is checked beforehand without changing the main tree (Git >= 2.38). A conflict is fixed in the worktree; the main tree must be clean and stay on the recorded base branch. A worktree with logs or history is kept after the merge. Full rules: [WORKTREES.md](WORKTREES.md).
- The wiki (`.makedo/wiki`) is part of the tree: wiki edits made in a worktree arrive in main together with the branch merge, so the history of how the feature was understood is never lost.
- Implementation: git CLI commands (`git worktree add/remove`, `git merge`), no libgit2 (an extra dep; the target audience has git by definition — the wiki already requires it).

### 3.8 Config (`makedo-config`)

TOML. Merge: `~/.makedo/config.toml` → `.makedo/config.toml` (the project wins **per whole entry** — grok-build semantics: no deep field merge, predictability matters more than flexibility; we document this explicitly). `MAKEDO_*` env overrides. Unknown keys are not fatal, but they are collected into diagnostics (`makedo doctor`), a grok-build pattern. `$schema`-like validation is simply `serde(deny_unknown_fields)` on every section + a report.

Secrets (API keys): `~/.makedo/credentials.toml`, chmod 0600, separate from the config (grok-build pattern).

### 3.9 Providers (`makedo-providers`)

```rust
trait Provider: Send + Sync {
    fn stream(&self, req: CompletionRequest) -> BoxStream<'_, Result<LlmEvent, ProviderError>>;
}
```

M0: **Anthropic Messages API** + **OpenAI-compatible chat/completions**. M3: **openai-chatgpt** — a ChatGPT subscription through the Codex backend (opencode's mechanism: OAuth PKCE on auth.openai.com with the Codex CLI client_id + device flow; requests go to chatgpt.com/backend-api/codex/responses with the ChatGPT-Account-Id header; the dialect is the Responses API, a third EventMapper). The OAuth infrastructure is shared with MCP OAuth (M3). Risk: this is an unofficial channel and OpenAI may close it — the second one covers xAI, OpenRouter, Ollama/llama.cpp for free (local models are relevant for an audience with no API budget). The SSE decoder is hand-written (~80 lines; rejected: reqwest-eventsource — an extra dep for a simple format). **The provider/model catalog is the same as in opencode: the models.dev database** (M3): a snapshot embedded in the binary + a background refresh into `~/.makedo/cache/models.json`; the catalog supplies context limits (for compaction), prices (for the TUI usage line) and model name validation; `makedo models` lists what is available. Test provider: DeepSeek (`deepseek-v4-flash`, openai-compat, base_url api.deepseek.com/v1). Retries: ≤3 with exponential backoff + jitter on 429/5xx, respect Retry-After; cancellation propagates into an HTTP abort. Tool calls of both dialects are normalized into a single `LlmEvent`.

---

## 4. Dependencies (shortlist)

| Crate | Why | Rejected | Cost |
|---|---|---|---|
| `tokio` (rt-multi-thread, macros, process, io-util, fs, sync, time, signal) | runtime | async-std (dead), smol (the rmcp/reqwest ecosystem = tokio) | large, but there is no alternative |
| `serde`, `serde_json` | everything | — | baseline |
| `toml` | config | — | small |
| `thiserror` | lib errors | snafu | zero (macro) |
| `anyhow` | bin | — | small |
| `schemars` (draft-07) | tool schemas | hand-written schemas (error-prone) | medium, compile-time |
| `rmcp` (client, minimal transport features) | MCP | hand-written client | medium; risk of a reqwest version conflict — quarantine |
| `reqwest` (rustls-tls, json, stream; default-features=false) | HTTP/SSE | ureq (no async streaming), hyper directly (a lot of manual work) | large; rustls instead of openssl — cross-compilation without system libs |
| `ratatui` + `crossterm` | TUI | cursive, hand-written | medium |
| `ignore` + `globset` | walk/glob (the ripgrep engine as a lib) | subprocess rg (absent on old machines, +a binary), walkdir (does not respect gitignore) | small |
| `grep-searcher` + `grep-regex` | grep tool | subprocess rg | medium |
| `tokio-util` | CancellationToken | — | small |
| `parking_lot` | sync locks outside .await | std Mutex | small |
| `indexmap` | tool registry | — | small |
| `tracing` + `tracing-subscriber` (fmt, env-filter) | logs to a file | log | medium; in release — file logger only |
| `dirs` | ~/.makedo | hand-written | small |
| BM25 | **hand-written module** (~150 lines in makedo-types) | tantivy (mmap indexes, huge compile time — for a corpus of a few hundred documents that is a sledgehammer to crack a nut), the `bm25` crate (an extra dep for one formula) | zero |
| frontmatter/wikilinks | hand-written parser | combinators (nom/winnow) — the grammar is trivial | zero |

Not taken in M0: `notify` (file-watching the wiki — YAGNI, generation by mtime on access), tree-sitter (see 3.1), `criterion`, OpenTelemetry.

Gate: a `cargo deny` + `cargo bloat` report in CI; every new dep is a row in DEPS.md.

---

## 5. Build and releases

**Profiles:**

```toml
[profile.release]          # local checks
opt-level = 3
lto = "thin"

[profile.dist]             # release binaries
inherits = "release"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

**[open question]** `opt-level = 3` vs `"s"`: old CPUs with a small cache are sometimes faster on compact code. The decision is made only from the bench harness (startup + wiki seed + BM25), not by taste.

**Target matrix:**

| Target | Audience | Note |
|---|---|---|
| `x86_64-unknown-linux-gnu` | Linux laptops | old glibc baseline (built on an old image / zig cc) |
| `x86_64-unknown-linux-musl` | old/non-standard distributions | fully static — the main binary for "old devices" |
| `x86_64-apple-darwin` | Mac Intel | `MACOSX_DEPLOYMENT_TARGET` = the lowest reasonable value **[open question: 10.15 or 11.0]** |
| `aarch64-apple-darwin` | M1/8GB | deployment target 11.0 |

No `target-cpu` tweaks: the default x86-64 baseline already does not require AVX2. **[open question]** x86-64-v2 (SSE4.2, ~2009+) is acceptable and gives a little speed; measure it.

**CI (GitHub Actions):** fmt + clippy `-D warnings` + tests + `cargo deny` on every PR; the build matrix + budget gates (binary size, hyperfine startup on a linux runner) on a tag. Release: binaries + checksums; installation via a `curl | sh` script and direct links. No npm/brew in M0.

---

## 6. Roadmap

**M0 — "walking skeleton"** (an agent that can be used to fix itself):
workspace + lints + CI; config (global→project merge); providers (Anthropic + OpenAI-compat, streaming, cancellation); tool runtime (read/write/edit/bash/grep/glob/todo, semaphore, per-path locks, CancellationToken, truncation+spill); session loop (actor, streaming, permissions ask/allow/deny + doom-loop); TUI (rendering, input, spinners, theme).
*Exit:* dogfood — makedo completes a real task on its own in its own repo on the reference machine; the startup/RSS budgets are in the green; cancelling a turn kills all child processes (test).

**M1 — LLM Wiki + worktrees:**
vault parser, graph, BM25 module, wiki_search/wiki_node/wiki_trace, init flow with git enforcement, `makedo wiki check`, spinner progress for indexing, prompt discipline for enrichment; compaction (prune + truncate); worktree mode (§3.7): `--worktree`/`finish`, `.makedo/.gitignore`, merge on explicit request.
*Exit:* a first run in someone else's 10k-file repo: init < budget, the wiki is in git, search tasks are solved faster through the wiki than with grep (measured as the number of tool calls before an answer on 10 benchmark questions about the codebase).

**M2 — MCP:**
rmcp client, stdio+HTTP, handshake state machine, resilient framing with a line ceiling, restart backoff, notification coalescing, search_tool/use_tool with cached BM25, statuses in the TUI, `.mcp.json` import (compatibility with the Claude Code ecosystem) + `${VAR}` expansion.
*Exit:* 3 real servers (github, a filesystem one, one remote HTTP) survive: kill -9 of the server (auto-restart), garbage in stdout (the connection stays alive), 100 list_changed notifications (1 event), a giant line (disconnect without OOM).

**M3 — Skills + polish:**
SKILL.md loader, embedded ponytail, skill tool; OAuth infrastructure (PKCE, localhost callback, device flow, refresh) — used twice: MCP OAuth (PKCE+DCR, two-layer dedup) and the openai-chatgpt provider (a ChatGPT subscription through the Codex backend, §3.8); `makedo doctor`; ASCII fallback for spinners.
*Exit:* ponytail is active out of the box and can be turned off in the config; an OAuth server (Linear) passes the auth→refresh→re-auth cycle.

**M3.5 — Providers × Prompt Caching (a separate stage, critical for cost):**
Goal: the user NEVER falls out of the prompt cache because of us — uncached input tokens cost 4–10× more.

*Providers:* cover all opencode providers. The catalog (models.dev, 191 providers) is already embedded; the work is the API dialects: (a) anthropic Messages, (b) openai-compat — already there; add (c) Google Gemini generateContent, (d) AWS Bedrock (sigv4), (e) Azure OpenAI (api-key/deployment routing), (f) GitHub Copilot, (g) openai-chatgpt (Codex, §3.9). The dialect is chosen by the catalog's `api` field + an override in the config.

*Prompt caching — correctness invariants:*
1. **A byte-for-byte stable prefix**: the system prompt, the tool manifest (already alphabetical) and message serialization are deterministic; no volatile information (time, counters) in the prefix.
2. **Compaction does not break the cache**: the current prune mutates old messages — that invalidates the prefix and costs money. Solution: prune only IN SYNC with summarize compaction (one cache break instead of constant ones), or track the cache breakpoint and mutate only beyond it. [open question — to be designed in this stage]
3. **Anthropic**: explicit `cache_control` breakpoints — tools, system, the tail of the history (up to 4 points); the breakpoint is updated every turn.
4. **OpenAI-compat**: the automatic cache needs a stable prefix of ≥1024 tokens — item 1 guarantees it; `prompt_cache_key`/the user parameter where supported.
5. **Metrics**: `Usage` is extended with `cache_read/cache_write`; the TUI shows the turn's cache-hit-rate; a bench scenario in `benchmarks/`: a 20-turn session, gate "≥90% of input tokens from the cache starting with turn 2".
*Exit:* on Anthropic and DeepSeek the 20-turn scenario holds cache-hit ≥90%; changing the model/provider needs no changes to session code (only the dialect).

**M4 — Release 0.1:**
binary matrix, install script, bench harness as a CI gate, complete DEPS.md/NOTICE, docs.
*Exit:* all §1.2 budgets are CI gates; installation on a clean old machine (Ubuntu 18.04 class, macOS Intel) with one command.

**Deliberately deferred (YAGNI, with a return point):** subagents/task (we come back when we hit context limits), summarize compaction (on real overflows), OS sandbox Landlock/Seatbelt (when there is demand for untrusted code), LSP tool, websearch, code mode, MCP resources/prompts, hooks system, HTTP server/headless mode, tree-sitter bash parsing, BPE tokenizer, file-watching the wiki.

---

## 7. Success metrics and how we measure them

The §1.2 budgets = the contract. Tooling:

- **Startup:** `hyperfine 'makedo --version'` and `makedo --bench-startup` (to the first frame, self-timed) — a CI gate on a linux runner, a manual run on the reference Mac.
- **RSS:** Linux — a `/proc/self/smaps_rollup` self-report via the `makedo doctor --mem` command; macOS — `/usr/bin/time -l`. It is taken at 3 points: after startup, after wiki init, after a 20-turn session (a scripted scenario in makedo-bench).
- **Functional wiki metric:** "cost of an answer" — the number of tool calls and the wall time on 10 benchmark questions about the codebase, wiki vs grep-only; the goal is ≥ 2× fewer calls.
- **Size:** the `cargo bloat` top 20 in CI artifacts; binary < 15 MB is a gate.
- A regression of any gate means a red CI; a justified budget increase is a separate PR with a single change.

---

*This is a living document: decisions change only through a PR that updates the relevant section and notes which measurement or incident motivated the change.*
