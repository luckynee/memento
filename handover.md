# Handover: bugs found during the 2026-11-19 demo and proof work

Scope: everything found while recording the preview videos and proving memento on messy, multi-hop, cross-harness sessions. Each item says what was observed, the evidence, and the status. Commits are on `main` (from `4dd9a532` to `b98549ac`). Test fixtures used: `/tmp/ledgerd` (302k-token OpenCode session) and `/tmp/memento-messy` (reversed decisions, 5-hop chain); both live in `/tmp` and disappear on reboot.

## Fixed bugs

| # | Bug | How it showed up | Fix | Live verification |
| --- | --- | --- | --- | --- |
| 1 | OpenCode adapter ingested sessions from other projects | The plugin loaded for `/tmp/ledgerd` stored the orchestrator session that runs in `memento-pool` (348 segments) | `4dd9a532`: ownership gate compares the session's `projectID` with the plugin's project | Orchestrator session stored 0 segments in the test project afterwards |
| 2 | `claude-agent` provider never saw the switch digest | Haiku 5.5 answered "the earlier part of the conversation isn't in my context"; the recorded Claude request had 0 occurrences of `memento-digest` | `f9ee8124`: digest is prepended to the first user message of the tail (provider drops extra system parts) | Recall after switch 11/17 -> 17/17 |
| 3 | Headless `claude -p` and `codex exec` saved nothing | `"async": true` Stop hooks are dropped when the process exits; sessions had 0 segments, so the next hop lost the decision | `be88d6bf`: synchronous Stop spawns a detached importer with a pending marker; `memento continue` waits on it | Every hop saved; `continue` waited 0.9 s for an in-flight import |
| 4 | Forks duplicated the parent's memory | Every fork re-ingested the copied history (67-103 segments each), flooding project search | `22378916`: forks link to their parent and ingest only messages created after the fork | Fork stored 2 segments and was linked to its parent |
| 5 | Chunker produced near-identical segments | One reply became 131 segments after a paragraph shorter than the 150-char overlap | `19eb2935`: overlap capped at half a chunk and chunk ends must advance | Same kind of reply stores 2 segments; regression test fails on the old code |
| 6 | Models searched with one many-topic query and missed facts | An 8-topic query returned only 6 hits overall; the agy note was missed | `b92952fb`: tool descriptions say one topic per call | Next run searched per topic and found the note |
| 7 | Only one pending handoff per project | A second `memento continue --to X` overwrote the first, so launches had to be serialized | `ce31a471`: one slot per `(project, target_harness)` | Claude Code and Codex claimed in parallel, 17/17 each |
| 8 | First OpenCode request after a claim had no continuation | Claim ran inside the first request's `context` hook and was stored as a session instruction, which only applies from the next request; GPT-6 Luna said "I don't have those details" while `memento/claimed=true` | `b98549ac`: the claiming request also gets the framed continuation in its first user message (no duplicate on later requests) | Grid retake: OpenCode answered correctly on its first request |
| 9 | `memento search` needed `--harness --project --session` even for project scope | A model using the recall skill needed several shell calls | `fd20fc6f`: project defaults to the cwd's project key, scope to `project` without `--session`; daemon accepts project/global search without identity | `memento search "topic"` works from the project directory |

Related features added while fixing these: deletion release with dependent-aware cascade and `memento gc` (`1cf3ddd7`, `b5140262`), activity tracking and the stale sweep (`049e02b6`), generic and agy hooks plus subagent exclusion and auto-gc (`3f2f1a84`), OpenCode subagent skip and missed-deletion reconcile (`91815936`), skills and docs (`09bfc6c3`).

## Open bugs and gaps

| # | Issue | Evidence | Suggested fix |
| --- | --- | --- | --- |
| A | Git repos with no commits share OpenCode project id `global`, so the adapter's ownership gate cannot tell them apart | Probe plugin in `/tmp/probe-del` (no commit) got `project.id = "global"`; a repo with one commit got a hash id | When `project.id === "global"`, also compare the session's `location.directory` (or the project key) before treating it as owned |
| B | Search ranks an earlier question above the note that answers it | `memento search "QA sign-off owner"` returned a previous test session's question first; the agy note came second | Down-weight `user_request` segments that are questions (ends with `?`, starts with who/what/which) or boost `decision`/`assistant_note` in the rerank |
| C | "Not recorded" answers are stored and later look like conflicting facts | A final hop said "deploy window not recorded"; the next run reported a conflict with the real note | Skip or tag assistant notes that only state absence; or have Jev classify them as `note` with low weight |
| D | A model sometimes answers an older quoted question instead of the current one | 1 of 3 final-hop runs answered a quoted request from the digest | Tighten the continuation framing ("answer only the current user message"); consider not quoting full past user requests in the digest |
| E | Fork lineage includes parent segments recorded after the fork point | By design of lineage search; documented in `docs/DESIGN.md` | Filter ancestor segments by fork time when the link came from a fork |
| F | Overlapping imports share one pending marker; the first to finish removes it | Accepted trade-off in `be88d6bf` | Use a counter or per-import marker files if `continue` ever races |
| G | 7-day stale expiry not observed live | Covered by tests with injected time and live Qdrant; the real 7-day wait was not run | Run once with a short threshold on a throwaway daemon (`MEMENTO_STALE_DAYS` only accepts whole days, so add a test-only seconds override or wait) |
| H | Whether agy subagents fire their own SessionStart/Stop is unknown | agy documents subagents but no subagent hook fields | Probe with a hooks.json logger while agy runs a subagent; skip by `conversationId` lineage if needed |
| I | No simple CLI to mark a session `keep` | Only via the API or `sessions link --retention keep`, which also relinks | Add `memento sessions keep --harness H --session S [--off]` |
| J | `memento forget` has no `--dry-run` | Reported by the docs worker; the cleanup skill uses stats/lineage as a preview | Add `--dry-run` that returns the count and session list |
| K | Windows detached import is compiled but untested | Deprioritized by the user | Leave until Windows matters |

## Harness facts that caused or explain bugs

- OpenCode `session.deleted` carries only `data.sessionID`, reaches every loaded plugin instance, and has no replay; `session.get` afterwards throws `SessionNotFoundError`.
- `opencode api GET /api/session/<id>` exits 1 both for a deleted session (`HTTP 404`) and an unreachable server; parse the output.
- Evicting a loaded location needs `-H 'x-opencode-directory: DIR'` on `DELETE /api/debug/location`; plugin paths must be directories.
- Codex `SessionEnd` can fire on archive/delete but its reason is always `other`; `codex archive` moves the rollout to `~/.codex/archived_sessions/`.
- Claude Code deletes transcripts after `cleanupPeriodDays` (default 30) and logs each tool call twice in its transcript (count unique `tool_use` ids in evals).
- agy `transcriptPath` is sometimes the `.system_generated/` directory and sometimes `logs/transcript_full.jsonl`.

## Feedback

- Publish the API. Third parties can already use the HTTP API, the generic hook, and MCP, but the endpoints are documented only inside `docs/DESIGN.md`. A `docs/API.md` with request/response examples, error codes, auth, and lifecycle rules (plus an OpenAPI file and a tiny example client) would make "bring your own harness" real.
- Default auth. The daemon is localhost-only, but `MEMENTO_TOKEN` is optional. Consider generating a token at install time and having `install-hooks` write it, before anyone exposes the port.
- Digest quality. In file-heavy sessions the digest's "User requests" are mostly read instructions and "Current tasks" comes first; the facts users ask about sit in notes and decisions. Ordering decisions and constraints first, and trimming long user requests, would help weaker models (see open issue D).
- Auto-gc cost. `memento gc` runs `opencode api` once per registered OpenCode session. Fine today; with hundreds of sessions, batch it (one session list call) or rely on the adapter's reconcile for OpenCode.
- Tracking everything. Per-project installs only track that project. For "track every session" install with `--user` for Claude Code, Codex and agy, and add the OpenCode plugin globally; say so in the README quick start.
- Tests against real harnesses. The live proofs (messy chain, grid, deletion) were scripted by hand in `/tmp`. Turning `/tmp/memento-messy` and the scorer (`/tmp/memento-proof/score.py`) into a repeatable `scripts/live-proof.sh` would catch regressions like bugs 2, 3 and 8, which unit tests missed because they only appear with real providers.
- Environment gotchas for future demos: OpenCode's `formatter: true` reformats shell scripts written through it (spaces become tabs); `tmux send-keys -l ";"` is swallowed as a command separator (send `\;`); `ffmpeg -f x11grab -window_id` must use the top-level kitty window (pick the one with the expected geometry).
