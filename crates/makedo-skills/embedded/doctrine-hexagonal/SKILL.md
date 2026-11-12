---
name: doctrine-hexagonal
description: >
  Hexagonal (ports & adapters) doctrine. Keeps the business core free of any
  framework, database, HTTP, or UI dependency; everything external is an adapter
  plugged into a port the core defines. Rigor is ADAPTIVE — a plain script needs
  no ports. Active whenever this doctrine is configured; governs code structure
  and what you record in the wiki.
---

# Doctrine: Hexagonal / Ports & Adapters (adaptive)

ACTIVE EVERY RESPONSE while configured. You build for someone who may not read
code; the payoff is a core that outlives any library choice and stays legible in
the wiki.

## The prime rule: adaptive rigor

Ports & adapters is a seam, not a mandate. Apply the ponytail ladder:

1. **A script / one-shot tool / static site** → no ports. Just write it.
2. **Real logic that must survive a swapped DB, provider, or UI** → put that logic
   in a framework-free core and reach the outside through ports.

Don't wrap a 30-line tool in three layers of interfaces. Add a port only where an
external dependency would otherwise leak into the core.

## The dependency rule (the whole point)

Dependencies point INWARD. The core (domain + use cases) imports nothing external.

- **Port** — an interface the core OWNS (`OrderRepository`, `Clock`, `Notifier`).
  The core calls it; it never knows the implementation.
- **Adapter** — implements a port using a concrete tech (Postgres, an HTTP client,
  the filesystem). Adapters depend on the core, never the reverse.
- **Driving vs driven** — the UI/CLI/API *drives* the core through use-case ports;
  the DB/network are *driven* by the core through its ports.

If a core file imports a framework or a DB driver, that's the bug — move it to an
adapter behind a port.

## Record it in the wiki

- new port / adapter / use case → a wiki node (`type: port`, `type: adapter`,
  `type: use-case`, `type: domain`).
- typed edges: `port:: [[OrderRepository]]`, `adapter-for:: [[OrderRepository]]`,
  `drives:: [[PlaceOrder]]` — the graph shows what plugs into what.
- Keep the core's ports documented so the owner sees the seams; when a request
  changes a port's contract, say so and update the node.

## Risk horizons (this doctrine = long-lived project)

A configured doctrine means the owner expects this system to live. Every DESIGN
decision (new port, dependency, schema, external service, public API shape)
weighs risks on two horizons before you commit:

- **Mid-term (months)**: migration pain, data growth, coupling that slows the
  next feature, a dependency's maintenance health, test brittleness.
- **Long-term (a year+)**: vendor/library lock-in, schema evolution, scaling
  ceilings, compliance/PII exposure, bus-factor of a clever solution.

Rules of engagement:

- Pick the design that survives the risks you KEEP; name the risks you ACCEPT —
  one sentence each, in the owner's language.
- An accepted risk is recorded, not silently swallowed: wiki node
  `type: decision` with the risk, why it's acceptable now, and the upgrade path
  (`mitigated-by:: [[X]]` when one exists).
- Ponytail still governs code volume: risk analysis changes WHICH design you
  pick, it never justifies speculative code. Hedge with seams (a port, a schema
  version field) — the hexagonal seam IS the standard long-term hedge.
- Trivial changes inside an existing boundary skip this — horizons apply to
  decisions that are expensive to reverse.

## Talking to a non-programmer

Explain seams in plain terms ("swapping the database won't touch your rules"),
give a one-line why for each port, and keep wiki nodes short and readable. Only
introduce a seam when something real would otherwise bleed into the core.
