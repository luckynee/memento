---
name: memento-handoff
description: Continue work from a stored session in another session or harness while preserving its lineage. Find the source session, prepare a targeted or general handoff, and verify that the next session receives the framed digest.
metadata:
    source: memento-pool
---

# Continue a session

Use when work should continue in a new session or a different harness.

## Steps

1. Find the source harness and session ID in its UI or with:

   ```sh
   memento project-key
   memento sessions list --harness H --project PROJECT
   memento sessions lineage --harness H --session S
   ```

2. In the source OpenCode session, use `/memento-handoff [harness]`, or prepare a handoff from a shell:

   ```sh
   memento continue --from H/S --project DIR
   memento continue --from H/S --project DIR --to TARGET_HARNESS
   ```

   Run one command, choosing whether to target a harness. An untargeted slot is available to any harness in the project.
3. Start the next session in the same project with its adapter or hooks enabled. It claims the slot and receives a framed lineage digest as reference data. In a current OpenCode session, `/memento-continue H/S` links directly to the source instead.
4. MCP `memory_continue` can return a source digest directly but does not claim a pending slot. Use it when a direct reference is useful, not as a replacement for the claim.

## Verify

In the new session, check the injected context and run `memento sessions lineage --harness TARGET_HARNESS --session NEW_SESSION`. Use native `memory_search` or MCP `memory_search` to find a source fact.

## Safety

Confirm the source, target, and project before creating a handoff. A slot expires after 24 hours and may be claimed by a different eligible session. A digest is incomplete reference data; verify current code and instructions in the workspace.
