---
type: module
description: Shared SQLite memory, semantic retrieval, transcripts and revisable evidence
---

`crates/makedo-memory/`. Database: `<git-common-dir>/makedo/memory.sqlite`.
One repository, all worktrees. WAL; independent connections; short write transactions.
Tables: sessions, events, entries, revisions, evidence, entries_fts.

Tools: `memory_search`, `memory_read`, `memory_write`. Native parent/children + MCP.
CLI: `src/memory_cmd.rs`. `makedo memory status|search|read|write|export`.
FTS5 + local multilingual embeddings; reciprocal-rank fusion; scores are relevance.
Default model: Ollama embeddinggemma. Model unavailable => explicit lexical_only notice.

Parent/child transcripts persist exact provider payloads. Resume copies history into a new session.
Old JSONL imported once. Current context compaction does not edit persisted events.
Notes: sources + revisions + expected_revision conflict checks. Corrections retain old conclusions.
`src/worktree.rs::finish` records observed local merge ref/SHA; does not imply push/correctness.
Session verification gate records command/result/log/HEAD/dirty-tree evidence.
Explicit reverts and unreachable integrations append observations; rejected notes remain searchable.

Validation: `cargo test -p makedo-memory`; `scripts/smoke_memory.py`; native protocol smoke.
Details: `docs/MEMORY.md`.

part-of:: [[crates]]
