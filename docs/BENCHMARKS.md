# Benchmarks and caveats

These measurements describe particular sessions and small experiments, not expected savings or recall guarantees. The underlying recorded observations are in [the code map](MAP.md#verified-runtime-facts). Input-token figures are request totals where recorded, not necessarily uncached tokens; the available notes do not consistently break out cache reads, model versions, or exact sample sizes. They should not be read as cost comparisons.

## Real usage audit

- **Date and environment:** 2026-11-26; OpenCode 2.4.1, real sessions, read-only audit of `opencode.db`. The models and provider versions for the individual observations were not recorded in the map.
- **Sample:** Nine sessions after installation on 2026-11-22; two observed switches, two manual memento compactions, and 32 observed subagents.
- **Measured:** First-request input tokens following switches, context tokens around compaction, and whether a parent digest reached each observed subagent. The audit did not record a cache-read breakdown in the map.
- **Results:** The two switch requests went from 301k to 57k and 76k to 32k tokens. Two manual compactions went from 376k to 80k tokens. All nine sessions registered and ingested; 32/32 subagents received a parent digest.
- **Caveats:** These are observations, not randomized comparisons. Resume compaction never qualified. Models never called `memory_search` on their own in this audit. One compaction duplicated content retained after the cut; a later fix addressed it.

## Manual compaction summary size

- **Date and environment:** 2026-11-26; OpenCode 2.4.2 private server and throwaway daemon. Model and provider version were not recorded in the map.
- **Sample and setup:** A 349k-token history tested with the fixed adapter and the pre-fix control (`537e0176`); three idle manual-compaction checks. The exact number of token-comparison runs was not recorded.
- **Measured:** Summary size in characters, next-request input tokens, retained tagged blocks, and recall. A cache-read breakdown and exact recall question count were not recorded.
- **Results:** The fixed summary was 34.5k characters and the next request 26.0k tokens, versus 65.4k characters and 44.4k tokens for the control. Recall was correct in both.
- **Caveats:** Idle manual compactions recorded no `keepID` and retained no prior messages; all tagged blocks remained (29/29, 41/41, and 22/22). A positive live overlap case was not observed, so this does not verify live removal of overlapping blocks.

## Project primer A/B

- **Date and environment:** 2026-11-26; OpenCode, gpt-6-luna medium. The OpenCode version was not recorded in the map.
- **Sample and setup:** One seed session, three runs per cell, primer on versus off. Haiku was not run because an isolated configuration returned `provider.no-route`.
- **Measured:** Recall, open-task answer, tool steps, first-request input tokens, and false continuations for an unrelated prompt. The notes do not provide a cache-read breakdown.
- **Results:** Recall was 3/3 and the open-task answer 1/1 in both arms. Primer on used 0 tool steps versus 10 off, including 7 `memory_search` calls. It added 452 first-request tokens for a 1,918-character primer. Neither arm had false continuations in 3 unrelated-prompt runs.
- **Caveats:** One seed and n=3 per cell are too small to generalize; the primer stays opt-in and adds context to each new session.

## Native versus plugin benchmark

- **Date and environment:** 2026-11-22; OpenCode private-server benchmark. A gpt-6-luna base session was switched to `claude-agent/haiku` for the switch comparison; precise versions were not recorded in the map.
- **Sample and setup:** A 205k-token forked history with planted decisions and full file reads; two runs per cell, seven questions per run (five conversation decisions and two file-content details).
- **Measured:** First-request input tokens, compaction model input and elapsed time where applicable, and recall. First requests on the forks did not hit the OpenAI prompt cache; 205.6k tokens were uncached in that observation.
- **Results:** On switch, native replay used 341.6k first-request tokens with 7/7 recall versus 21.6k and 5/7 with the plugin. For same-model manual compaction, native summarization used 205.9k model input tokens and 12-14 seconds, then 11.2k first-request tokens and 5/7 recall. Memento used zero model tokens for compaction and 3.9 seconds, then 14.5k first-request tokens and 5-6/7 recall with additional `memory_search` steps. After 12 minutes idle, native resume used 206.5k uncached tokens and recalled 7/7 versus automatic memento compaction at 11.9k first-request tokens and 5/7 recall.
- **Caveats:** Each cell had only two runs. All arms recalled the five decisions, but digest-based arms missed file-content details that full verbatim history retained. Different request totals do not by themselves establish dollar savings.
