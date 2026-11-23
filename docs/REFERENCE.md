# Reference

How the agent behaves, in detail.

## Providers and models

The model catalog is refreshed with `makedo models --refresh` and automatically
once an hour. Custom OpenAI-compatible providers (for example Qwen on Vast) are
defined in `~/.makedo/models.json` (`$MAKEDO_HOME/models.json` when overridden):

```json
{
  "qwen-vast": {
    "api": "http://YOUR_HOST:PORT/v1",
    "env": ["QWEN_VAST_TOKEN"],
    "models": {
      "YOUR_SERVED_MODEL_ID": {
        "context": 0, "output": 0,
        "in": 0, "out": 0, "tools": true
      }
    }
  }
}
```

Use the ID from `/v1/models` and the server's real limits; `0` means an unknown
limit. This entry replaces the catalog provider with the same name and survives a
refresh. The provider appears in `/models` after makedo starts; the token is
entered there, or taken from the named variable or from
`~/.makedo/credentials.toml`.

For `provider.kind = "openai"` (including a custom `base_url`) and the
`api.openai.com` address, the Responses API is used with `max_output_tokens`:
reasoning works together with tools, and its encrypted state is kept across
`--resume`. Requests are sent with `store: false`. Other compatible providers use
Chat Completions with `max_tokens`.

### ChatGPT subscription

`makedo login chatgpt` opens the OpenAI sign-in in the browser (OAuth with PKCE;
the response comes back to `127.0.0.1`). After signing in,
`provider.kind = "chatgpt"` works through the Responses API against the plan's
quota — in makedo's own loop, with all its tools, unlike the `codex-cli` delegate.
`makedo login chatgpt <model>` also makes that model the current one; the
available models are listed in `/models`. Tokens are stored in
`~/.makedo/chatgpt.json` (mode 0600) and refreshed automatically; to switch
accounts, delete that file and sign in again. `max_output_tokens` is not sent on
this route: OpenAI does not accept it there. Prefix caching does not apply on a
subscription (the server reports zero tokens written and read), so every step
spends quota in full; no dollar price is shown for it.

### Response limit

When a response hits its output limit, makedo keeps the context it received and
continues automatically (up to two attempts), raising the response budget within
the limits of the model and the context window. Tool calls from a truncated
response are not executed: the agent receives their status and issues complete
calls again.

### Reasoning level

`/effort` opens the reasoning-level picker; `/effort xhigh` sets it directly. The
level is shown next to the model, takes effect from the next request and is saved
in `~/.makedo/effort.json` separately for each provider, endpoint and model. The
default model does not change.

For a verified OpenAI-compatible endpoint, the profile is added to its entry in
`~/.makedo/models.json`, next to `api`, `env` and `models`. For example, for Swift
1.5 / Qwen3.8 on Vast:

```json
"effort": {
  "qwen3.8": { "levels": ["low", "medium", "xhigh"], "default": "low" }
}
```

Without an explicit profile the `reasoning_effort` parameter is not sent. Levels
are not guessed from the model name; `high` is not a substitute for `xhigh` for
this Qwen.

## Message queueing

While the agent is working, Enter queues the message: it appears in the feed right
away with a `queued` mark. Before the next request to the model the mark is
removed and the message is not duplicated. Ctrl+Enter interrupts the current step
and hands the agent the queue together with the new text. Ctrl+Enter with an empty
input sends the queue accumulated so far. Results of finished tools are kept;
calls that are interrupted receive a cancellation result before work continues.

## Versioned file editing

`read` returns a SHA-256 `version` of the entire file, including when reading a
line range. Existing files require that version in `edit.expected_version` or
`write.expected_version`. Without a version, `write` only creates missing files.
Successful writes return the new version; a conflict requires another read.

`apply_patch` accepts a `patch` string with `*** Begin Patch` / `*** End Patch`,
`*** Add File: path`, `*** Update File: path` with `@@` context hunks, and
`*** Delete File: path`. Pass `expected_versions: {"path": "sha256:…"}` for
every update/delete. Matching is exact; ambiguous context is rejected. Moves
use add + delete. Deletion through a symlink requires its resolved path explicitly.
Updates preserve LF/CRLF and the final newline convention;
use `edit` for mixed line endings.

Native writes share per-path OS locks across sessions/processes of the same user
(`~/.makedo/file-locks`, independent of `MAKEDO_HOME`). Different files proceed
independently. Temporary files are synced and published atomically on the same
filesystem; existing permission bits and valid symlinks are preserved. New files
start with private permissions (0600). Readers
see the old or new file. All patch versions/hunks are validated before staging.
A multi-file patch is atomic per file, not a transaction: a later I/O failure
reports already-applied paths. Advisory locks cannot serialize external editors
or shell scripts; use worktrees for independent tasks with overlapping edits.

## Execution logs, Heavy and embedded skills

Shell commands write complete stdout/stderr directly to unique files under
`.makedo/spill/commands/` (or the project's external MAKEDO data directory). The
model receives `OK`/`FAIL`, exit status, elapsed time and a log path. On failure
it searches the log and reads the relevant lines. For commands whose output is
the requested data, such as `git diff`, use `bash` with `output="content"`;
its preview is bounded to 8000 bytes. Shell calls are foreground operations;
detached children that outlive the shell are terminated and reported as a failure. `/logs` displays complete artifact paths.
Verification uses the same runner, retains logs on cancellation/timeout and
supports cancellation while checks are running.

Status-mode commands and verification automatically use the installed `heavy`
binary for machine-wide slots. `heavy=false` opts a cheap command out;
`heavy=true` also applies slots to content-mode commands. The existing
`HEAVY_*` and CI behavior belongs to Heavy. If Heavy is absent, the result says
`heavy=unavailable`; MAKEDO's own per-batch concurrency limit still applies.

Ponytail, Open Code Review, its delegate mode, Heavy, worktree guidance and TRIZ
(the cheapest-change check: ideal result, cheapest knob, binding constraint, one
`optimality:` line per change) are embedded in the binary. `skill_search` finds a skill and `skill_load` loads its instructions
on demand. Only Ponytail (full by default), TRIZ with Lean and a selected
doctrine are always included in the system prompt. `[skills] triz = false`
moves TRIZ and Lean back to on-demand; Lean is the lean-manufacturing side (the
eight wastes, go and see, stop the line, five whys) and TRIZ picks the change. Global Claude/Codex/plugin skill directories are
not read at runtime. `.makedo/skills/<name>/SKILL.md` can add project skills or
override a built-in; disabling Ponytail also disables a project override.
`ocr` remains an external executable with its existing provider configuration.

Text submitted during a turn is queued and delivered between model steps. Esc
cancels the active turn and its still-pending messages. Queued input and its
acceptance are recorded in the shared project SQLite database, including subagent
transcripts. `makedo --resume latest`, `makedo --resume <session-id>` or a legacy
JSONL path restores history into a new session and preserves the original.
Completed tool calls are not replayed; interrupted
calls are marked for inspection before any retry. This is recovery, not an
exactly-once guarantee for external side effects. `/clear` clears the screen,
while keeping model context.

Project memory lives at `<git-common-dir>/makedo/memory.sqlite`, shared by all
worktrees. `memory_search`, `memory_read` and `memory_write` expose ranked facts
and conversation excerpts through native tools and `makedo mcp-serve`. Retrieval
combines FTS5 with cached multilingual embeddings from local Ollama
(`ollama pull embeddinggemma`; run `ollama serve`). If the embedding service is
unavailable, results explicitly report `lexical_only` and a setup hint.
`makedo memory status|search|read|write|export` provides the same access from the CLI.
Notes retain revisions, corrections and independently observed merge/check
evidence; relevance scores are not probabilities of correctness.
See [memory behavior and configuration](MEMORY.md).

Details: [upstream integration](UPSTREAM.md), [independent UI audit](UI-AUDIT.md).

`makedo skills` lists the built-in catalog; `makedo skills <name>` prints one skill.
`/ponytail [lite|full|ultra|off]` changes intensity for the session without an LLM call.
Worktree ownership and integration rules: [WORKTREES.md](WORKTREES.md).
Finish uses Git >= 2.38 for merge preview, requires a clean parent on the recorded
base branch, and retains worktrees containing runtime logs/history.

## Agent progress

The main plan is a horizontal line above the input. Subagents appear in a right
panel with `name │ elapsed` and an indented horizontal plan. Plans come from
`todo_write`; tool completions appear as they finish, independently of batch order.
No additional model request is needed to render progress.

F6 cycles agent details (activity, worktree, latest log), F7 or `/agents` toggles
the panel. Below 114 terminal columns it collapses to a compact summary. `/logs`
also includes subagent log paths. `makedo demo` previews the production interface
with labelled fixture events and no model requests or commands.

## YOLO in chat

Send `YOLO` on its own, or include it as a word anywhere in a prompt, to grant
all tool permissions for the current session. Matching ignores case. A standalone
`YOLO` switches the mode locally without a model request; a prompt containing
`YOLO` still runs normally. The input border shows `YOLO` once enabled.

This also works while a tool is waiting for approval: type `YOLO` and press Enter.
In that dialog `y` + Enter approves once; `a`, `A` and `n` retain their shortcuts.
`/yolo`, `/allow-all` and `--yolo` enable the same mode. Repeated-call approval
guards do not interrupt YOLO. The mode is session-only; restored transcripts,
files and tool output do not activate it.

While a turn is running, the status uses a one-cell spinner and a compact elapsed
time (for example `⠋ bash · 9s`). Text is muted; parallel tools and waiting calls
add short counts. The completed-turn token/cache/cost summary is unchanged.
