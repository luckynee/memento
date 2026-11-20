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

## Round 2: long noisy parent, natural delegation (2026-11-20)

Setup changes:
- Parent: 40 real turns per harness (file edits, commands, chatter, 10 decisions, two of them corrected later: retries 5 -> 7 with 250 ms backoff, TTL 30 s -> 45 s). Each trial forks the parent and resets the workspace to the parent's files.
- Natural delegation: the parent writes the subagent prompt itself ("delegate this to the X subagent"). The number of agreed facts the parent put into the prompt was measured (not possible for Codex: its `spawn_agent` message is encrypted in the rollout).
- Conditions: `none`, `digest` (full parent digest), `filtered` (`profile=subagent`: no parent in-flight request, no acknowledgements under 40 chars). Codex also `native` (`fork_turns="all"`, memento off); Codex memento conditions ask the parent for `fork_turns="none"`.
- Product code under test: OpenCode adapter `subagentMemoryAgents` (cd627ed1), CLI `SubagentStart` hooks (b5a7fc93), Codex fork linking (fb585812), 12000-char subagent budget (72246d2e). Models: OpenCode and Codex `gpt-6-luna`, Claude Code Haiku, parent and child the same.
- Stopped early at 133 trials (target about 300); cells hold 2-6 valid trials. Stronger models (GPT-6.1 Terra, Sonnet) were dropped on request; their 11 pilot trials are excluded.

Bugs found by the pilot and fixed before the counted run: Codex forks were not linked to their parent (memory nearly empty); a 6000-char subagent budget dropped 4-5 of 10 facts on a 40-turn parent; the implement task lacked quota limits, so GPT-6.1 Terra refused in every condition.

Results (quality = facts in `docs/CONVENTIONS.md` out of 11 for recall, convention checks out of 9 for implement; tokens and seconds are subagent means):

| harness | condition | recall | implement | control | first request tokens | total prompt tokens recall / implement | subagent secs recall / implement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| OpenCode | none | 6.0 | 6.25 | 4/4 | 12,441 | 165k / 283k | 32.6 / 63.4 |
| OpenCode | digest | 11.0 | 6.75 | 4/4 | 15,004 | 149k / 201k | 22.9 / 45.2 |
| OpenCode | filtered | 11.0 | 5.0 | 4/4 | 14,906 | 112k / 181k | 21.7 / 35.8 |
| Claude Code | none | 11.0 | 7.5 | 6/6 | 14,630 | 42k / 103k | 7.6 / 19.0 |
| Claude Code | digest | 11.0 | 7.33 | 5/5 | 15,846 | 56k / 102k | 13.6 / 23.4 |
| Claude Code | filtered | 11.0 | 7.0 | 5/5 | 15,873 | 57k / 123k | 12.4 / 24.1 |
| Codex | none | 7.5 | 6.67 | 3/3 | 18,860 | 172k / 147k | 31.8 / 39.4 |
| Codex | digest | 11.0 | 8.67 | 3/3 | 21,467 | 76k / 135k | 15.0 / 32.3 |
| Codex | filtered | 10.75 | 6.67 | 3/3 | 21,398 | 82k / 133k | 15.4 / 33.3 |
| Codex | native | 11.0 | 8.0 (n=1) | 3/3 | 92,006 | 231k / 1,041k | 13.3 / 75.3 |

- The parent's own prompt decides most of the outcome. Claude Haiku parents put all 10 facts into every recall prompt, so memory changed nothing there. OpenCode `gpt-6-luna` parents often passed few: with at most 3 facts in the prompt, recall was 2.67/11 without memory (n=3) and 11/11 with it.
- Memory made subagents cheaper overall in OpenCode and Codex: a larger first request (+2.5k tokens), but fewer exploration steps, lower total prompt tokens and shorter runs. In Claude Code it added about 1.2k tokens and 4-6 s with no quality change.
- Codex `native` full-context forking matched memento's recall quality at 4.3 times the first-request tokens (92k vs 21k).
- `filtered` gave no measurable benefit over `digest` and scored lower on implement in OpenCode and Codex.
- Old values were never used as current: all 19 docs that mentioned 5 attempts or 30 s marked them as superseded.
- Off-task: 0 "Noted."-style answers in 133 trials; one OpenCode `filtered` implement produced no file.
- Codex parents did not delegate (or no subagent was recorded) in 10 of 44 trials, spread across conditions.
- Limits: small cells after the early stop, cheap models only, one parent session per harness.
