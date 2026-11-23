---
name: triz
description: >
  Proves a change is the cheapest one that satisfies every hard constraint, using
  TRIZ moves instead of formal maths: the ideal result (nothing to build), the
  cheapest knob, resolving a contradiction instead of trading it off, and reusing
  a harmful side effect. Load it BEFORE any change that adds a file, entity,
  endpoint, table, dependency or abstraction, or picks between implementations,
  and whenever asked "why not simpler?", "prove this is optimal" or "почему так?".
---

# TRIZ: the cheapest change that satisfies every constraint

Pick `x*` = the cheapest candidate that satisfies every hard constraint. Name the
constraint that rules out the candidate just before it: that is the **binding
constraint**. If you cannot point to a rule in a file, an owner decision or a
failing check, you have not proved anything.

## Scope

- A one-file change with no new entity, endpoint, table or dependency needs one
  line in the commit body or the final answer:
  `optimality: chose <x>; cheaper <x0/x1> rejected by <constraint> (<source>)`.
  An obvious `x0` still has to be named.
- Anything bigger (a package, a migration, a new endpoint, infra, a choice
  between implementations) needs the YAML block at the end, in the plan or the
  final answer.
- A review checks the author's block. A cheaper candidate that breaks no sourced
  constraint, or a missing block, is a major finding.

## Objective f

Compare candidates lexicographically, earlier terms win. If the project states
its own order, use that; otherwise:

1. **Owner time.** Questions, waits, manual steps, recovery commands. Self-healing
   beats a runbook.
2. **New surface.** New entities, endpoints, tables, services, files, lines,
   dependencies. Deleting is negative surface.
3. **Run cost.** Latency or throughput on hot paths, CI minutes, infra.

## Constraints

Hard constraints come only from sources you can cite: the project's rules files
(`AGENTS.md`, `CLAUDE.md`, `README`, contracts and schemas), the check commands
that must pass (tests, linters, CI), and owner decisions quoted in the task with
their date. Platform rules the project states (tenancy, security, data formats)
count too.

A constraint you cannot cite is an **assumption**. List it as one and ask; never
use it to reject a cheaper candidate. Never relax a constraint yourself.

## Procedure

1. **State f.** One line: which term decides this case.
2. **List the constraints that apply**, each with its source.
3. **Write candidates from cheapest to dearest.** Always include:
   - `x0`, the ideal result: the problem goes away. An existing part already does
     it, or a reformulated task no longer needs it. The best part is the one that
     does not exist.
   - `x1`, the cheapest knob: a limit, a config value, a flag.
   - `x2…`: real code, smallest diff first.
4. **Resolve a contradiction, do not trade it off.** If A beats B on one term and
   loses on another, look for a C no worse than either on any term (split by
   time, by place or by condition; do it once instead of per call). A and B are
   then dominated and dropped.
5. **Use a harmful effect.** An existing side effect, such as a retry, a sweep, a
   reconnect or a replay, may already deliver the wanted behaviour under other
   conditions.
6. **Pick the first candidate, in f order, that satisfies every constraint.** The
   constraint that rejected the candidate before it is the binding one.
7. **Prove it.** For each rejected cheaper candidate: the violated constraint,
   its source, and a check that takes under a minute (a command, a test name, a
   `file:line`). Where money or safety is involved, the proof is a test of the
   invariant against the real system, never an argument.

## Stop condition

You are done when all of these hold:

- every candidate cheaper than the pick has a violated constraint with evidence;
- no reason is an adjective ("cleaner", "more robust", "better practice");
- no assumption was used to reject a candidate;
- the pick names its binding constraint.

Otherwise either the pick is not optimal (take the cheaper candidate) or a
constraint is missing (ask the owner).

## Output

```yaml
optimality:
  f_term: owner_time | surface | run_cost
  constraints:                       # only the ones that apply
    - id: C1
      rule: "<one line>"
      source: "<file[:line] | owner YYYY-MM-DD>"
  candidates:                        # cheapest first
    - id: x0
      what: "<one line>"
      verdict: rejected | chosen
      violates: C1                   # rejected only
      evidence: "<command | test | file:line>"
  chosen: x2
  binding_constraint: C1
  assumptions: []                    # anything unsourced, to ask about
```

## Example

The question: a service socket gets a per-connection frame limit. Why not none,
and why not a new token bucket?

- **x0, no limit.** Rejected by multi-tenancy (the platform rules): one client
  could flood a shared pod.
- **x1, reuse the one-second window the other socket already has.** Chosen.
- **x2, a new token-bucket module.** Rejected: more surface than x1 and no
  constraint that x1 fails to meet.

Binding constraint: multi-tenancy. The proof is the existing frame-limit test.
