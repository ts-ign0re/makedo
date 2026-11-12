# Upstream integration, 2026-11-06

Compared local Grok Build `d71f6e0c..2bdd1d6a` and OpenCode
`4e81a0b73f..3ec9b3b0a6`. The selection follows MAKEDO's low-cost, small-runtime
purpose. Mechanisms were adapted to the existing Rust code rather than importing
upstream runtimes.

| Mechanism | Source/reference | MAKEDO integration |
|---|---|---|
| File-backed command output | OpenCode `packages/core/src/shell.ts`; user workflow | Complete stdout/stderr streams directly to a unique file. Default model result is status, exit, duration and log path. `output=content` explicitly requests a bounded tail for data queries. |
| Shared machine limits | Installed Heavy CLI | Status-mode shell calls and verification use `heavy -- sh -c` when available. Existing pool/slot/nesting/CI settings are respected. Missing Heavy is reported in the result. |
| File aliases and conflicting reads | Grok `util/lock_path.rs`, `acp_session_impl/tool_calls.rs` | Resolve paths including symlinks; serialize conflicting calls in invocation order. Independent paths remain concurrent. |
| Bounded reads | Grok `util/file_reader.rs` | Read only the requested line window with a bounded buffer, including oversized individual lines. No full-file allocation for log inspection. |
| Failed-edit diagnostics | OpenCode `tool/plugin/edit.ts` | Return nearby numbered lines or an already-applied hint. Replacement still requires an exact, unambiguous match. |
| MCP lifecycle | Grok `xai-grok-mcp/src/servers.rs` | Propagate cancellation and timeout to `notifications/cancelled`; use configured HTTP headers; normalize only null or a JSON-encoded object. |
| Durable input and resume | OpenCode `session/inbox.ts`, `session/execution/restart.ts`; Grok interjections | Journal queued prompts before delivery, accept them between model steps, and record delivery together with the message. Explicit resume creates a new journal and preserves the original. Interrupted tools receive a recovery marker, not an automatic replay. |
| Overflow recognition | Both projects' provider-error classifiers | Recognize additional proxy/context and payload-size errors while keeping 429 throttling separate. Existing request-budget and changed-payload retries remain in place. |
| Skills | User's Ponytail/OCR requirements | Built-in catalog and on-demand skill bodies live inside the binary. Only Ponytail and an explicitly selected doctrine are always in the system prompt. No global skill directories are read at runtime. |

The embedded OCR skills are snapshots with their licenses. OCR keeps its own
provider and credentials; invoking the skill does not change them. The external
`ocr` and `heavy` executables are not embedded. Project-local `.makedo/skills`
can add or override skills. Global skill/plugin catalogs are not loaded.

Background summarization, automatic model warming, a large permission parser,
and wholesale UI/runtime rewrites are outside this integration. Background LLM
work spends tokens even when its result is never used; MAKEDO first reduces the
output entering the context. Native reasoning-protocol expansion also requires
its own provider compatibility work, rather than copying another client's wire
assumptions.

See [UI-AUDIT.md](UI-AUDIT.md) for the independent UI audit and the narrowly
scoped corrections selected from it.

Validation on the local macOS checkout: 165 workspace tests passed, including
Responses replay, incomplete streams and context compaction. Workspace
Clippy with warnings denied, formatting, wiki links and the dist build passed.
The offline native-binary smoke exercises embedded skill loading, a full 6 MB
command log with a short model result, failed-command diagnosis via grep,
verification and resume without overwriting the original journal, through both
Chat Completions and Responses. The latter checks exact encrypted-reasoning,
message and function-call replay. MCP tests use
real local stdio/HTTP fixtures for cancellation and configured headers.

The final binary is 9,045,296 bytes (8.63 MiB); a local `--version` invocation took
152.7 ms immediately after installation; five subsequent launches measured
3.1–3.8 ms (median 3.5 ms). The embedded catalog was also checked outside the checkout with an empty
MAKEDO_HOME. The fixture checks use no paid model endpoint; terminal rendering is covered
by render fixtures and a real PTY agent session (including F6/F7, resize and exit).
Native Terminal screenshots were unavailable because the UI tool denies access.
The installed binary passed the PTY smoke as well.

Chat YOLO and the compact active status were also verified. The installed binary
passed the real PTY flow that types YOLO into a pending approval and continues
without another approval or model request for the control message.

Native OpenAI now uses Responses with `max_output_tokens` and `store: false`.
The live `gpt-6-sol` check read a temporary file through a tool and continued
after `--resume`, both with the API default effort and explicit `medium`.
Those short live responses returned function/message items only; encrypted
reasoning replay is verified by the offline fixture. Other compatible providers
retain Chat Completions. The previous installed binary is backed up locally.
