# UI audit, 2026-11-06

Independent agent review of MAKEDO against local Grok Build `2bdd1d6a` and OpenCode
`3ec9b3b0a6`. Evidence: source and existing render fixtures. The auditor did not
launch applications, edit files or run tests; this is not a claim of live visual
acceptance.

The existing theme, layout, user-message plates, palette and sticky question do
not need replacement. The important defects are state handling and access to
information.

| Priority | Finding | Decision |
|---|---|---|
| P1 | Bracketed paste ignores the API-key modal and writes into the ordinary prompt. | Route paste to the active field; regressions check the prompt and rendered screen. |
| P1 | Concurrent tools with the same name finish another invocation's row. | Store and match `call_id`; keep the same compact tool rows. |
| P1 | Input is capped at six visible lines without a cursor-following viewport; long lines and emoji/CJK columns can disappear or misalign. | Separate follow-up: retain the frame, add a viewport and correct display-width accounting. |
| P2 | Enter is advertised while busy but does nothing; simply enabling it also corrupts transcript ordering and busy state. | Accept text while busy, journal it, and add it to the visible conversation only after the session acknowledges a safe delivery boundary. Slash actions remain disabled while busy. |
| P2 | Compact tool previews cut off long log paths. | `/logs` shows full paths separately; keep log contents out of the model unless requested. |
| P2 | Scroll position is measured from the end, so streaming moves history under a reader who scrolled up. | Separate follow-up: anchor the visible entry; auto-follow only at the bottom. |
| P2 | A long permission request can clip both command text and choices. | Separate follow-up: scroll the description and pin choices; later show a small edit diff. |
| P2 | `/clear` appears to clear context but only clears the screen; usage is only for the last turn. | Explain the current `/clear` behavior. Separate follow-up: explicitly label last-turn cost and add context/session totals. |

Reference areas: Grok's `xai-grok-pager/src/scrollback` and
`views/permission_view.rs`; OpenCode's `packages/tui/src/component/prompt`,
`dialog-shell-output.tsx`, and `routes/session/permission.tsx`.

The larger follow-ups are documented rather than bundled into a visual redesign.

A subsequent implementation adds a right-side agent list with nested horizontal
plans, elapsed time, F6 details and F7 collapse. The main plan is horizontal above
the input. Executor events now distinguish queued work, actual tool starts and
immediate completions; model-facing results retain their original call order.
A real local-API/PTY smoke exercises root-to-child event forwarding, plans,
keyboard toggles, resize and exit; render fixtures cover widths 140, 80, 40 and 20.
`makedo demo` displays explicitly labelled fixture events using the production UI.

User screenshot follow-up: running tool rows and the active status now use a
one-cell spinner. The status text is muted, elapsed time is abbreviated and the
duplicate Esc hint is removed. The idle token/cache/cost line is preserved.
YOLO can be typed in the permission dialog; its y/Y prefix is buffered until
Enter so it cannot be mistaken for a single-call approval.
