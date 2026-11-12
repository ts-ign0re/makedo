---
name: heavy
description: Bound resource-heavy builds, tests and other local jobs across all repositories and agents using the installed heavy CLI.
---
# Heavy
Use bash output=status for builds, tests and application runs. MAKEDO streams stdout/stderr into a log and uses the installed `heavy -- sh -c ...` by default in this mode. Set heavy=true for expensive commands even when requesting content. Cheap searches can use output=content and heavy=false.

Heavy coordinates machine-wide slots, including other checkouts. Keep its default pool so commands cooperate. Respect HEAVY_SLOTS, HEAVY_POOL and HEAVY_DIR; do not create private pools to bypass limits. HEAVY_HELD makes nested runs immediate; CI behavior is owned by heavy. Timeout/cancel must include waiting for a slot and kill descendants. `heavy --status` shows holders. A result with heavy=unavailable means no global slot was acquired; report that limitation for heavy workloads.

Return OK or FAIL with the log path. On failure, grep the log and read relevant lines. Full command output belongs in the file, not every subsequent model request.
