---
name: doctrine-ddd
description: >
  Domain-Driven Design doctrine for new and growing projects. Models the
  problem in the user's own domain language, keeps business rules isolated from
  frameworks/DB/UI, and records every domain decision in the LLM wiki so the
  project stays legible to a non-programmer. Rigor is ADAPTIVE — escalate
  architecture only when the domain earns it. Active whenever this doctrine is
  configured; governs how you structure code and what you write to the wiki.
---

# Doctrine: Domain-Driven Design (adaptive)

ACTIVE EVERY RESPONSE while configured. You are building software for someone
who may not read code. Your job is not just "make it work" — it is to make a
system a senior engineer would sign off on, and that its owner can understand
through the wiki. Clean domain model, documented decisions, no framework rot in
the core.

## The prime rule: adaptive rigor

Match architecture to the domain's real complexity, NOT to ceremony. This is the
ponytail ladder applied to design — stop at the first rung that holds:

1. **CRUD / no real invariants** (a list, a form, a static site) → NO DDD
   machinery. Plain functions and records. Do not invent aggregates for a todo.
2. **Real business rules** (money, state that must stay consistent, a workflow,
   more than one actor) → introduce the building blocks below, but only the ones
   the rule needs.
3. **Multiple sub-domains that drift** → separate bounded contexts.

Escalate up, never start at the top. Over-modeling a simple app breaks the
promise of "fast and cheap" — it is the same sin as over-engineering.

## Ubiquitous language ↔ the wiki

The domain vocabulary IS the wiki. This is not optional bookkeeping — it is how
the owner reads their system.

- Every domain term the user says (a noun that matters: "hand", "payout",
  "outs") becomes a wiki node `type: term` with a one-line plain-language
  definition. Use the SAME name in code, wiki, and when you talk to the user.
- Never rename a domain concept silently. If the user's word and the code word
  drift, fix the code to match the user — their language wins.
- Before modeling, `memory_search` the term; reuse the node if it exists.

## Building blocks (use only when the domain earns them)

- **entity** — has identity and a lifecycle (an Account, a Hand).
- **value-object** — defined by its values, immutable (Money, Card).
- **aggregate** — a consistency boundary; one root guards its invariants. Outside
  code touches the root, never its insides. Reach for this only when there IS an
  invariant to protect.
- **domain-event** — a fact that happened ("HandSettled"); use when other parts
  react to it.
- **repository** — a port to load/save an aggregate; the domain declares the
  interface, the adapter implements it. Domain never imports the DB.
- **domain-service** — a rule that doesn't belong to one entity.
- **application-service** — orchestrates a use case; thin, no business rules.

## The dependency rule (keep the core clean)

The domain model depends on NOTHING outward — no framework, no DB driver, no HTTP,
no UI. Those live at the edges as adapters and depend inward on the domain. If a
domain type imports a framework, that's a bug to fix. (This is the seam that lets
the owner's rules outlive any library choice.)

## Record it in the wiki (traceability)

As you implement, keep the wiki as the living design, with typed edges:

- new bounded context / aggregate / term → add a node (`type:` above).
- `enforces:: [[Invariant]]`, `emits:: [[SomeEvent]]`, `implements:: [[FR-x]]`,
  `depends-on:: [[OtherContext]]` — so the graph traces requirement → rule → code.
- When a request CHANGES a documented domain rule, say so plainly and update the
  node — the wiki is the source of truth, don't let it drift.

## Risk horizons (this doctrine = long-lived project)

A configured doctrine means the owner expects this system to live. So every
DESIGN decision (new module boundary, dependency, schema, external service,
public API shape) weighs risks on two horizons before you commit:

- **Mid-term (months)**: migration pain, data growth, coupling that slows the
  next feature, a dependency's maintenance health, test brittleness.
- **Long-term (a year+)**: vendor/library lock-in, schema evolution, scaling
  ceilings, compliance/PII exposure, bus-factor of a clever solution.

Rules of engagement:

- Pick the design that survives the risks you KEEP; name the risks you ACCEPT.
  One sentence each, in the owner's language.
- An accepted risk is recorded, not silently swallowed: wiki node
  `type: decision` with the risk, why it's acceptable now, and the upgrade path
  (`mitigated-by:: [[X]]` when one exists).
- Ponytail still governs code volume: risk analysis changes WHICH design you
  pick, it never justifies speculative code for futures that may not come.
  Hedge with seams (a port, a schema version field), not with implementations.
- Trivial changes inside an existing boundary skip this — horizons apply to
  decisions that are expensive to reverse.

## Talking to a non-programmer

- Name things in domain language, not tech jargon ("payout split", not
  "TransactionAllocationStrategy").
- When you make a modeling choice, give the owner a one-sentence why in their
  terms, then proceed. Don't quiz them on architecture.
- The wiki is their map. Keep it readable: short nodes, plain definitions.

The point: the owner describes their world; you turn it into a clean model, keep
the vocabulary honest, document the decisions, and add only the structure the
domain actually needs.
