# Project memory

One SQLite database per repository: `git rev-parse --path-format=absolute
--git-common-dir` plus `makedo/memory.sqlite`. Main checkout, nested directories
and linked worktrees resolve to the same file. It is Git metadata and is not
committed. Without Git, the database uses the project's existing global data
directory under `~/.makedo/projects/`. Back up the database with SQLite's backup
API, or stop all Makedo processes before copying it with its WAL files.

Each operation opens its own connection. WAL lets readers see committed
snapshots while a writer is active. Writers serialize short transactions with a
five-second busy timeout; timeout errors are surfaced. Model inference never
holds a write transaction. This is local-host storage, not a network-filesystem
or distributed database.

## Startup and history

Startup syncs wiki pages into the searchable index, re-reading only files newer
than their indexed row and dropping rows of deleted files; a wiki file written by
a tool during a session is indexed right after that call. Search itself never
reads the vault. Startup also imports legacy
JSONL sessions once without changing them, and adds up to eight short recent
notes from the current branch to the initial prompt. Full knowledge is retrieved
on demand. Wiki graph tools remain available for typed-link traversal.

Native parent and child sessions record complete messages, tool arguments,
results, queued/delivered prompts and opaque provider output in SQLite. Search
indexes user and assistant text excerpts (up to 8,000 characters); exact payloads
remain available for recovery. MCP clients share knowledge through the memory
tools; their external chat transcripts are owned by the client.

`--resume latest` selects the latest nonempty main-agent session on this branch, not a child.
`--resume <session-id>` selects a database transcript. Legacy JSONL paths also
work. Resuming creates a new session and preserves the original; completed tools
are not replayed and unfinished calls are marked for inspection. Compaction
changes the model's current context, not the persisted events.

## Search and tools

- `memory_search(task, context?, hypothesis?, kind?, limit?, all_branches?)`
  returns ranked IDs, snippets, branch, source, revision and evidence.
- `memory_read(id)` returns the note, prior revisions and replacements, or a
  conversation excerpt with adjacent messages from the same session.
- `memory_write(title, text, sources, id?, expected_revision?, assessment?,
  supersedes?)` creates a sourced note or revises one with optimistic concurrency.
  A stale expected revision returns a conflict. Corrections may reference an
  older note with `supersedes`. Old text and evidence remain readable.

Search defaults to the current branch plus notes observed merged into it.
`all_branches=true` explicitly includes other branches, labelled in every hit.
The embedding request contains the task, context and optional hypothesis. FTS5
preserves exact identifiers; cosine similarity supplies semantic candidates.
Reciprocal-rank fusion combines their top 40 lists, avoiding incompatible raw
score scales. The final list defaults to eight entries, capped at 25. A small
evidence adjustment affects the final rank; `relevance_score` and
`semantic_similarity` remain separate from the evidence and final `ranking_score`.
None is a calibrated probability that the solution will work.

Documents cache normalized vectors in SQLite, keyed by embedding endpoint/model
and note revision. Missing vectors are populated in batches during search;
`memory_write` computes its vector before saving. Revision checks prevent stale
inference results overwriting a newer note's vector. No background indexing
service is needed. First semantic access to imported history may be slower;
subsequent queries reuse cached document vectors. The initial implementation
uses an exact in-process vector scan; replace it with ANN only after measuring a
corpus large enough to justify it. Long inputs are truncated by the embedding
model's context limit; keyword retrieval still covers the stored excerpt.

## What the harness does without the model

- Before each prompt the harness runs a keyword search over wiki nodes and notes
  and appends up to three hits to the user message (`[makedo: …]`). No embedding
  call, no git. The seeded map, link grammar and unfilled stubs are never hints.
- After a turn that needed three or more tool calls, the harness stores a note:
  the prompt as the title, the files the turn edited or read and named in its
  answer, and the head of the answer. No model call. A turn in which the model
  wrote its own note, or a repeat of an already noted question, stores nothing.
- The wiki index is synced at startup and after a tool writes a wiki file, only
  for files that changed. Rows of a checkout that no longer exists are dropped.

## Local embeddings

Install/run Ollama, then:

```sh
ollama pull embeddinggemma
ollama serve
```

The chat model is independent. Defaults can be overridden in global or project
configuration:

```toml
[memory]
embedding_url = "http://127.0.0.1:11434/api/embed"
embedding_model = "embeddinggemma"
```

The endpoint must implement Ollama's `/api/embed` contract. Credentials in URLs
are rejected and redirects are disabled. A configured remote endpoint receives
the text being embedded. Missing/unavailable models produce an explicit notice
and `lexical_only` retrieval; stored knowledge remains usable. A mid-indexing
failure reports its notice even if some semantic candidates are available.

## Evidence and the right to revise

Models can label a conclusion `hypothesis`, `disputed` or `rejected`; they cannot
declare a merge or a passed harness check through `memory_write`.

After `makedo finish` / `worktree_finish`, the harness records the actual local
target ref and integration SHA against the relevant note revisions, after
checking Git ancestry. This confirms local integration, not push or deployment.
External/squashed merges are not inferred from arbitrary text or guessed commit
matches. Verification-gate results record the command, log path, HEAD and whether
the working tree was dirty; they apply to that observation, not future code.

Search/startup observes known integration refs again. An explicit Git revert or
unreachable integration appends evidence without erasing the original merge.
Arbitrary manual reversals and undiscovered bugs cannot be reliably inferred;
agents should attach a sourced correction/dispute when they discover one. A
revert is not itself proof that the solution was wrong. Absence of a merge is
not failure. Rejected notes stay searchable as negative experience.

`makedo memory export <note-id>` writes a selected note and its evidence to
`.makedo/wiki/memory-<uid>.md` for review and Git versioning. The random 128-bit UID
is stable across revisions and avoids collisions between different developers'
local numeric IDs. Teammates commit/pull these Markdown notes; startup imports
them into each clone's local search index. Full session transcripts stay local.
SQLite remains the
authority for the note's revisions; editing an exported wiki page does not
silently rewrite those revisions.

## Validation

`cargo test -p makedo-memory` covers concurrent writers and reader snapshots,
revision conflicts, legacy import, shared worktrees and merge/revert evidence.
`python3 scripts/smoke_memory.py target/dist/makedo` exercises CLI, native tools,
MCP, semantic ranking/caching and child transcript persistence using local
fixtures. Add `--live http://127.0.0.1:11434/api/embed` to verify real Russian-to-
English retrieval with embeddinggemma. Existing native protocol smoke tests
exercise exact provider replay and resuming database sessions.
