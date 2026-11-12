# Product core: cheap LLMs + discipline + a knowledge layer

Date: 2026-08-21. Based on measurements from 22 benchmark runs (see
[BENCHMARKS.md](BENCHMARKS.md)), not on hypotheses. This document is the centre
of gravity of the product; PLAN.md remains the technical plan of the harness.

## Thesis

Cheap models (deepseek, qwen, local ones) fail on real codebases not for lack of
intelligence but for lack of context discipline: they wander around the
repository, re-read files and reinvent conventions that already exist. Three
things close the gap for routine tasks at about 1/30 of the price of a frontier
model:

1. **A knowledge layer** — an LLM wiki: a versioned, typed graph of facts about
   the project, filled in by a cheap model (~$0.15 for a 350k LOC monorepo).
2. **Discipline** — enforced wiki-first (search before grep), a duty to enrich
   (learned it → wrote it down), ponytail (brevity), and a doctrine with risk
   horizons for expensive decisions.
3. **Self-checking** — the agent must run type checks and tests until they are
   green; in the measurements this affected quality more than the choice of model.

Measured: a feature with tests and self-checking on deepseek-v4-pro with a
warmed-up wiki cost $0.13, against $3.5–5.2 on sonnet at comparable quality; the
wiki cuts the full cost of a run by 17–57% (the input side is ~80% of the money).

## Positioning

**"Serious codebases on cheap models."** Not "one more Claude Code": as a wrapper
around a frontier CLI it adds almost no value (their subagents and ecosystem are
better and develop faster). The niche of makedo is what frontier CLIs cannot do by
definition: they are locked to their own models, and generic agents have no
knowledge layer.

Who it is for: teams with a large repository and sensitivity to price;
on-prem and compliance setups (ollama, local models); solo developers on
subscription limits.

## Core (protect and develop)

- `makedo-wiki` — the graph plus its tools (`wiki_node`/`wiki_trace`; node search
  is `memory_search`), seeding, and the typed-edge schema. This is the product's IP.
- The native agent loop (`makedo-session`) plus API providers: the anthropic
  dialect, openai-compat (deepseek/qwen/openrouter/ollama), gemini. Cheap models
  work HERE — with makedo's tools and its discipline.
- Skills: ponytail (embedded), the ddd/hexagonal doctrines with risk horizons.
- `task` subagents (read-only research / edits in a worktree) plus the worktree
  tools: context hygiene and isolation of edits.
- `makedo mcp-serve` — exports the knowledge layer to OTHER agents (claude/codex/
  grok/any MCP client). The wiki is portable — that is a distribution channel, not
  a leak.
- The benchmark harness in `benchmarks/` — the product must measure itself; every
  change to the core is checked for $/feature and % of defects.

## Periphery (convenience, not the centre of meaning)

- The claude/codex/grok CLI delegates — keep as a bridge for subscription owners,
  do not invest beyond keeping them working.
- TUI polish, themes. Performance budgets for old machines are hygiene, not a niche.

## To build (gaps in the core, in priority order)

1. **`makedo wiki enrich [area]`** — a first-class command: enrich the wiki with a
   cheap model by area or diff (today it is a manual prompt with `--yolo`).
   Increment: after a merge, enrich only what was touched.
2. **Verification gate** — a project config for "how to check yourself" (the
   typecheck/test/lint commands — a wiki node of `type: verify`); the native loop
   must run them until green before finishing a turn that made edits. The largest
   measured quality lever.
3. **Model routing** — `[models] work / design / research` in the config: a
   routine task → a cheap model, a design decision (the doctrine fired) → an
   expensive one, a research subagent → the cheapest. The insertion point is ready
   (the task tool, the ponytail comment about a per-task provider).
4. **Plan phase** — plan with an expensive model → execute the plan with a cheap
   one. The hypothesis with the largest price potential; measure it with the
   benchmark first.
5. **Wiki health** — extend `makedo wiki check`: staleness (a node older than the
   edits to its files), orphans, broken edges; a report in the TUI.

## Product metrics (build in, show to the user)

- $/task and tokens by class (usage+cost already exist) — plus a weekly aggregate.
- Wiki hit rate: the share of tasks that started with a successful `memory_search`
  over the wiki.
- Escalation rate: % of tasks that moved from a cheap model to an expensive one.
- Defects: % of turns with edits that did not pass the verification gate.

## What counts as success

A team runs a real repository for a month: ≥70% of tasks are closed by a cheap
model without escalation, defects are no higher than the frontier model's level
from the benchmark, and the average price of a task is ≤ $0.5. If that does not
hold, the thesis is revised honestly, as in the reports.
