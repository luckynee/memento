# Subagent memory A/B (2026-11-20)

Question: should a subagent receive its parent session's memento memory by default?

## Setup

- Parent session turn 1: six project decisions (error code `QX-4471`, `at_ms` epoch-ms timestamps, `audit.emit` logging and no `print`, 7 retries with 250 ms backoff, tiers `ember < flint < basalt`, cache TTL 30 s), the parent told to answer only "Noted.". Turn 2: correction, TTL is 45 s.
- Each trial forks the parent and tells it to launch a `worker` subagent with an exact prompt that contains none of the facts. The child's first user message was checked for leaked facts: 0 leaks in all 72 trials.
- Tasks: `recall` (4 facts, 4 points), `implement` (`quota.py` must use the error code, `audit.emit`, no `print`, `at_ms`; 4 points), `control` (`slugify`, unit-tested, unrelated to memory; 1 point, plus a check for memory bleeding into the code).
- OpenCode v2.3.2, parent and child `9router/cx/gpt-6-luna`, adapter `subagentMemory` (commit 95020715), daemon at 39d53db1. Conditions: `none` (no digest, `memory_search` denied), `tool` (current default: `memory_search` available), `digest_notool`, `digest`.
- Claude Code 2.1.328, parent and child Haiku, prototype `SubagentStart` hook that imports the parent transcript and returns the framed parent digest as `additionalContext`. Conditions: `none`, `digest`.
- n = 4 per cell. Global user instructions applied to all conditions equally.

## Results

OpenCode:

| condition | recall | implement | control pass | memory bleed | first request tokens |
| --- | --- | --- | --- | --- | --- |
| none | 0.00/4 | 1.00/4 | 4/4 | 0 | 12340 |
| tool | 0.00/4 | 1.00/4 | 4/4 | 0 | 12350 |
| digest_notool | 4.00/4 | 3.00/4 | 4/4 | 0 | 12834 |
| digest | 4.00/4 | 2.00/4 | 4/4 | 0 | 12842 |

Claude Code:

| condition | recall | implement | control pass | memory bleed | first request tokens |
| --- | --- | --- | --- | --- | --- |
| none | 0.00/4 | 1.00/4 | 4/4 | 0 | 14090 |
| digest | 4.00/4 | 4.00/4 | 4/4 | 0 | 15979 |

- `memory_search` was never called by a child in any OpenCode trial (0/24 with the tool available), so tool-only access behaved like no memory. The deny rule in `none` was therefore never exercised.
- The stale TTL (30 s) never appeared; every digest answer used 45 s.
- Off-task failure: 1 of 24 OpenCode digest trials (`digest`, implement, rep 1) replied "Noted.", answering the parent's old acknowledgement request quoted in the digest instead of its task. 0 of 12 Claude Code digest trials did this.
- Implement misses under digest were model judgment (used the error code but skipped logging or timestamps), except the one off-task trial.
- Cost: the digest adds about 490 (OpenCode) and 700 to 1900 (Claude Code) prompt tokens to the child's first request.
- An earlier batch on `commandcode/gpt-6-luna` stopped at provider credit exhaustion; its clean trials (digest recall 4/4 twice, none 0/4 twice) agree.

## Validity limits

- Small n; differences inside the digest conditions (2.0 vs 3.0) are noise-level.
- The zero-leak delegation prompt is a worst case for `none`; real parents often restate key constraints.
- The parent session was short and clean (digest about 1.5k chars); long noisy sessions were not tested.
- OpenCode and Claude Code use different injection channels (first user message vs `additionalContext`).
- Codex and agy were not tested. agy subagents get no parent id at `SessionStart` (verified), so a parent digest cannot be targeted there without extra plumbing.

## Second opinion

agy (Gemini 4 Argon) recommended default-on for OpenCode and Claude Code with a filtered digest (declarative decisions and constraints, no raw past imperatives or the parent's in-flight delegation turn), Codex opt-in until tested, and flagged the same validity limits.

Scripts used: `/tmp/memento-exp/scripts/{exp.py,claude_exp.py,claude_subagent_hook.py,analyze.py}` (ephemeral).
