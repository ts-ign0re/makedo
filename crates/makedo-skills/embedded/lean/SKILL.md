---
name: lean
description: >
  Lean for agent work, built on TRIZ and lean manufacturing: cut waste (muda)
  from the process, not only from the code. Go and see before asserting (genchi
  genbutsu), pull work instead of pushing it, make mistakes impossible instead of
  reviewable (poka-yoke), stop the line on a defect (jidoka), and ask why five
  times before fixing a symptom. Works with triz, which picks the change; lean
  shapes how you get there.
---

# Lean: less waste between the request and the result

`triz` decides WHICH change is cheapest. `lean` keeps the WORK cheap: the steps
you take, the things you leave behind, the rework you cause. The value is only
what the owner asked for; everything else is waste until proven otherwise.

## The eight wastes, as an agent meets them

1. **Overproduction.** Features, options, docs or tests nobody asked for. Build
   only what the request and its acceptance check need.
2. **Waiting.** Blocking on a question you could default, or on a long command
   you could run in the background while you continue. Ask only what a default
   cannot settle, and say the default you will use.
3. **Transport.** Handing work around: copying a result between files, agents or
   messages. Pass a path or a reference, not a retyped copy.
4. **Over-processing.** Gold-plating: abstractions for one caller, config for a
   value that never changes, a second pass nobody needs. TRIZ `x0`/`x1` first.
5. **Inventory.** Half-done work left behind: stale branches, temp files,
   commented-out code, unmerged worktrees, TODOs with no owner. Finish or remove
   before you say done.
6. **Motion.** Reading far more than the change needs, re-reading what you
   already know, searching blind. Search with a question, read the part that
   answers it, keep the facts you found.
7. **Defects.** Rework: a wrong guess caught late. Check the cheapest fact first
   (run it, grep it, read the test) before building on it.
8. **Unused talent.** Doing by hand what a tool, a test or an existing function
   already does. Use the repo's own commands and helpers.

## Moves

- **Go and see (genchi genbutsu).** A claim about the code is checked in the code
  or by running it, not remembered. "It should work" is not a result.
- **Pull, not push.** Start from the acceptance check and work backwards. Build
  nothing that no check or request pulls in.
- **One piece at a time.** Finish one small change end to end (edit, check, clean
  up) before starting the next. Several half-done changes are inventory.
- **Poka-yoke.** Prefer a fix that makes the mistake impossible (a type, a
  constraint, a failing test, a default) over a comment that asks people to be
  careful. This is TRIZ's ideal result applied to errors.
- **Jidoka, stop the line.** A failing check or a surprise stops the work. Find
  the cause; do not stack more changes on top, and never relax a check to pass.
- **Five whys.** Ask why until you reach a cause you can change, then fix that,
  not its symptom. Write the cause in one line when you report.
- **Kaizen.** When you touch something, leave it a little simpler, but only
  inside the files the task already owns. Do not widen the diff to tidy.
- **Standard work.** If the repo has a command, a template or a convention for
  this, use it. Do not invent a second way.

## Before you say done

- Nothing left behind: no temp files, debug output, dead code or stray branches.
- The acceptance check ran, and you can name its result.
- The final answer holds one `optimality:` line from `triz`, and one line naming
  any waste you found and removed or deliberately left.
