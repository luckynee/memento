---
name: memento-cleanup
description: Inspect and safely remove unwanted memento memories or collect deleted sessions. Review project and lineage boundaries, preview garbage collection or stale sweeping, and obtain confirmation before any destructive forgetting.
metadata:
    source: memento-pool
---

# Clean up memory

Use when the user wants to forget memories, review deleted sessions, or inspect stale-session cleanup.

## Steps

1. Identify the project, harness, and session; review the relevant sessions and ancestry:

   ```sh
   memento project-key
   memento sessions list --harness H --project PROJECT
   memento sessions lineage --harness H --session S
   memento stats --harness H --project PROJECT --session S
   ```

2. Preview deletion detection without changing sessions or cursors:

   ```sh
   memento gc --project DIR --dry-run
   ```

   To preview a stale sweep, with daemon authentication configured as needed:

   ```sh
   curl -sS -X POST http://127.0.0.1:7377/v1/maintenance/sweep \
       -H 'Content-Type: application/json' \
       -d '{"project":"PROJECT","dry_run":true}'
   ```

3. Explain the exact scope and preview findings to the user. `memento forget` has no dry-run flag; inspect sessions, lineage, and stats as its non-destructive preview. Obtain explicit confirmation before running one of:

   ```sh
   memento forget --harness H --project PROJECT --session S
   memento forget --harness H --project PROJECT --lineage S
   memento forget --harness H --project PROJECT --whole-project
   ```

   Session removes one session's memories; lineage removes S and descendants; whole-project removes memories across harnesses. Do not add `--yes` without explicit approval.
4. If the user approves garbage collection after its preview, run `memento gc --project DIR`. The API supports retention `keep` on a session update. For an existing or new child link, `memento sessions link --harness H --project PROJECT --child S --parent P --retention keep` also sets it; linking changes lineage, so do not use it solely to change retention without approval.

## Verify

Rerun the relevant `memento stats` and `memento gc --project DIR --dry-run`; check session lineage where relevant. Deleted parents may remain available to uncollected descendants or pending continuations until eligible for leaf-first collection.

## Safety

Forgetting is destructive and can affect descendants or every harness in a project. `keep` protects against automatic collection, not an explicit forget. Never run an actual stale sweep or gc merely to preview it, and use existing secure authentication configuration without printing credentials.
