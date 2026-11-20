# memento-pool design

Status: v1 design, 2026-11-18. Facts below were verified against official docs on that date; versions are pinned.

## Problem

Switching models inside a long harness session (for example Opus at 500k tokens to an OpenAI model) replays the whole transcript to the new model. Provider prompt caches are keyed per model, so the first request after a switch is a full uncached prefill, every later turn pays cached-read cost on the whole prefix, and models reason worse over long, mostly stale context.

memento-pool stores a filtered, segmented copy of each session in Qdrant and lets the new model work from:

1. a small, frozen working-state digest (always injected),
2. a verbatim recent tail of the transcript,
3. on-demand recall through a `memory_search` tool.

It is retrieval over conversation history scoped by metadata. The digest exists because retrieval alone cannot surface facts the model does not know to ask about.

## Components

```
OpenCode plugin (TypeScript, adapters/opencode)      memento (Rust workspace)
  event subscribe -> map to neutral turns ------->   mementod (axum HTTP, 127.0.0.1:7377)
  Jev classifies assistant notes (kind hint)            segmenter, fastembed dense vectors
  context hook: model differs from origin?              Qdrant: dense + server-side bm25, RRF
    -> [digest] + [recent tail] + [post-switch]       memento CLI (HTTP client of mementod)
  tool memory_search -> daemon -> Jev relevance         scope, lineage, forget, stats, digest
  command memento-continue -> lineage + digest
```

- Core (Rust) is harness-agnostic and model-free. Harness adapters map native transcripts to the neutral ingest schema.
- Jev (System 1) is used only inside the OpenCode adapter through `ctx.experimental.generate.decisions` with model `{ providerID: "opencode", id: "jev-1.13-free" }` (free tier, verified 2026-11-18; paid `jev-1.13` needs OpenCode Models credits). Jev is a decision model: `predicate`, `choice`, and `score` questions; it cannot embed or generate text.
- Deployment: Docker Compose with `qdrant/qdrant:v1.20.2` and a `mementod` image. The CLI runs on the host.

## Pinned stack

| Item | Version | Notes |
| --- | --- | --- |
| Qdrant server | `qdrant/qdrant:v1.20.2` | HTTP 6333, gRPC 6334, storage `/qdrant/storage`, health `/healthz` `/readyz`; image has no curl/wget |
| `qdrant-client` | 1.20.0 | gRPC only, connect to 6334 |
| `fastembed` | 7.7.1 | `EmbeddingModel::BGESmallENV15`, 384 dims, cosine. Pins `ort 2.0.0-rc.15` (Rust >= 1.92, glibc only, x86-64-v3, needs libc++ at runtime). Cache via `FASTEMBED_CACHE_DIR` |
| Lexical | Qdrant server-side `qdrant/bm25` | Self-hosted since 1.15.3, via `Document` / `DocumentBuilder`, sparse vector with `Modifier::Idf` |
| Fusion | Qdrant RRF (`Fusion::Rrf`) | recency rerank done in Rust, see Search |
| OpenCode | v2.3.2, `@opencode/plugin` 2.x | Promise plugin API |

## Identity and scope

- `harness`: adapter id, for example `opencode`.
- `project`: canonical project key (OpenCode `ctx.location.project.id`).
- `session`: harness session id.
- Lineage: a session may declare a `parent` session (continuation). Lineage of S is S plus all ancestors.
- Search scopes: `session`, `lineage`, `project`, `global`. The CLI defaults to `project`, or `lineage` when a session is supplied.
- Retention mode per session: `continuation` (default) or `keep`. Explicit forgetting uses `memento forget --session|--lineage|--whole-project`.

## Neutral ingest schema

```json
{
  "harness": "opencode",
  "project": "proj_123",
  "session": "ses_abc",
  "items": [
    {
      "source_id": "msg_1",
      "ts": "2026-11-18T10:00:00Z",
      "role": "user",
      "text": "...",
      "kind_hint": null,
      "tool": null
    },
    {
      "source_id": "msg_2:tool:call_7",
      "ts": "2026-11-18T10:00:05Z",
      "role": "tool",
      "text": "",
      "kind_hint": null,
      "tool": { "name": "edit", "input": { "filePath": "src/a.rs" }, "output": "...", "ok": true }
    }
  ]
}
```

`role` is one of `user`, `assistant`, `tool`. Adapters drop reasoning content (provider-specific, often encrypted).

## Segmentation policy (Rust, unit-tested)

Raw transcripts are not indexed. Tool output dominates coding sessions and file contents in history go stale.

| Input | Kind | Stored text |
| --- | --- | --- |
| user text | `user_request` | full text, chunked |
| assistant text | `kind_hint` if valid (`decision`, `constraint`, `task`), else `assistant_note` | full text, chunked |
| tool in edit set (`edit`, `write`, `patch`, `apply_patch`, `multiedit`) | `file_change` | `<tool> <paths>` plus rendered body truncated to 2000 chars |
| tool in shell set (`bash`, `shell`) | `command` | command line plus output tail: last 1000 chars when `ok == false`, last 400 chars when `ok == true` |
| tool in read set (`read`, `grep`, `glob`, `list`, `webfetch`, `websearch`) | `file_ref` | `<tool> <path or pattern>` only, no output |
| todo tools (`todowrite`) | `task` | rendered todo list |
| subagent tools (`task`, `subagent`) | `assistant_note` | `<tool>` plus output truncated to 2000 chars (subagent reports are dense summaries) |
| any other tool (MCP, LSP, unknown) | dropped by default | configurable through `SegmentPolicy` |

- Chunking: max 1000 chars, 150 char overlap, split on paragraph, then line, then whitespace boundaries. BGE-small-en-v1.5 truncates input at 512 tokens and code tokenizes densely, so chunks stay well under that limit. Every segment kind is chunked, including `file_change` and `command` text.
- Path is taken from input keys `filePath`, `path`, `file`, `pattern` (first present); `webfetch` uses `url`, `websearch` uses `query`. For patch-style input (`patch`, `patchText`, `diff`, or `input` string) paths are parsed from `*** Add File: `, `*** Update File: `, `*** Delete File: `, `*** Move to: ` and `+++ b/` lines. The deduplicated paths are stored in first-seen order in `paths`, joined with `, ` in file-change text, and the first path is stored in `path`.
- A shell command that is a single simple invocation (no pipes, redirects, `;`, `&&`, `||`, backticks or `$(`) of `cat`, `head`, `tail`, `nl`, `less`, `more`, `bat`, or `sed -n ...p` with file operands is a file read: it becomes a `file_ref` with the operands as paths, and its output is not stored. Failed commands always keep a status line (`Exit code: N`, `Exit code N`, `Process exited with code N`, or `Exit status: failed`).
- A file-change body uses the first string in `patchText`, `patch`, `diff`, `input` order; otherwise, if both `oldString` and `newString` are strings, it renders `--- old`, the old string, `+++ new`, and the new string on separate lines. Otherwise it uses a `content` string or compact JSON as a fallback. The body is truncated before chunking.
- Point id: UUIDv5 over `harness/session/source_id/chunk_index`, so re-ingest is idempotent.
- Empty segments are skipped.

## Noise and prompt injection

Harness scaffolding is not session memory, and stored text is untrusted when it is shown to a model again.

- OpenCode v2.3.2 (verified on 40 local sessions): user `text` has no injected blocks; agent prompts live in system instructions (not ingested); skill bodies are `skill` tool results; harness notices are `synthetic` records. The adapter skips `system` and `synthetic` records.
- Adapters set `injected: true` on harness-generated items; the segmenter skips them.
- `SegmentPolicy.scaffold_strip_tags` removes `<system-reminder>`, `<skill_content>`, `<environment_context>`, `<user_instructions>`, `<command-message>`, `<command-name>`, `<local-command-stdout|stderr|caveat>`, `<subagent>`, and memento's own `<memento-*>` wrappers from user and assistant text (an unclosed tag strips to the end); `scaffold_unwrap_tags` keeps the inner text of `<command-args>`. Text that is empty after stripping is skipped.
- `ignored_tools` (`skill`, `memory_search`, `question`, `execute`) and `self_command_prefixes` (`memento `, `mementod `) prevent ingesting skill bodies and memento's own output.
- The digest contains user requests, assistant-authored decisions, constraints and notes, the latest todo list, changed and inspected file paths, and a few recent commands with exit status and a short (160 char) output tail, failures first. Other tool output (file contents, subagent reports) never reaches it; file reads are stored as path pointers only. Search results may include tool output because they arrive as tool output.
- Continuation framing (CLI hooks and OpenCode adapter) states that the digest omits full command output and file contents, tells the model to answer only the current user message because past requests are already-handled history, to search memory before concluding something was not recorded, and to read or grep workspace files for code details. The CLI hook prints a runnable `memento search "<query>" --harness ... --project ... --session ...` with the real, shell-quoted values.
- Every `<memento-` or `</memento-` sequence in rendered text is escaped to `&lt;`, so stored text cannot close the wrapper. Digest, continuation, and search output are framed as reference data, not instructions, with current instructions taking precedence.
- Residual risk: assistant-authored text that quotes an attack is still shown (escaped and framed). Optional future step: a Jev `predicate` screen at ingest for text addressed to an AI model.

## Qdrant data model

Collection `memento_segments`:

- named dense vector `dense`: 384, cosine.
- named sparse vector `bm25`: `Modifier::Idf`, filled server-side with `Document { model: "qdrant/bm25" }`.
- payload: `harness`, `project`, `session`, `kind`, `text`, `source_id`, `chunk_index`, `ts` (RFC 3339), `tool` (optional), `path` (optional), `paths` (keyword array). Old points without `paths` fall back to their `path`.
- keyword indexes: `project` (`is_tenant = true`), `harness`, `session`, `kind`, `path`, `paths`; datetime index on `ts`.

Collection `memento_sessions` (registry): payload-only points if the Rust client supports a collection without vectors, otherwise a 1-dim dummy dense vector. Point id: UUIDv5 over `harness/session`.

- payload: `harness`, `project`, `session`, `parent` and `parent_harness` (optional), `retention`, `created`, `title`, activity and deletion timestamps, and `deleted_reason` (optional).
- keyword indexes: `harness`, `project`, `session`, `parent`.
- Linking rejects cycles and self-parenting (409).

Lineage terms:

- ancestors(S): S, its parent, the parent's parent, up to the root (cycle-safe, max depth 64).
- descendants(S): S and every session whose ancestor chain contains S.
- Search scope `lineage` uses ancestors(S). `forget` scope `lineage` uses descendants(S), so forgetting a continuation never deletes the parent history that siblings still use.

Search:

1. Qdrant query: prefetch dense (limit 50) and bm25 (limit 50), both under the scope filter and optional `kinds` filter, fused with RRF at the top level with an explicit `limit` of 50. No formula query: RRF scores are sums of `1/(k + rank)` and Qdrant's docs warn that an unweighted decay term dominates them.
2. Rerank in Rust (pure, unit-tested): `normalized = rrf_score / max_rrf_score`, `final = normalized * question_factor * absence_factor + recency_weight * exp(-ln2 * age / half_life) + exact_match_bonus` where `question_factor` is 0.6 for a `user_request` whose trimmed text ends in `?` or begins with a question word, and 1 otherwise; `absence_factor` is 0.4 when every non-empty sentence or line states absence, and 1 otherwise. Age is measured against the newest `ts` in the candidate set (not wall clock, so results are reproducible), defaults `recency_weight = 0.15`, `half_life = 24h`. The default exact-match bonus of 0.5 applies only when the query has identifier-like whitespace-separated tokens and every such token occurs as a case-sensitive substring of the candidate text or first path. Surrounding punctuation is trimmed; identifiers contain `_`, `.`, `/`, `::`, or `-` between alphanumerics, mixed letters and digits, or a lowercase-to-uppercase transition. Ties break on `(ts desc, source_id, chunk_index)`. Return top `k` (default 8, max 50).

## Digest

`GET /v1/digest` builds a deterministic working-state document for a session lineage (ancestors) within a char budget (default 12000). Segments are loaded fully by scroll and sorted by `(ts, source_id, chunk_index)` before rendering; chunks of one source are re-joined (overlap removed) and rendered as one entry, never split mid-entry. Absence-only assistant notes, decisions and constraints are omitted; mixed entries remain. Each section has its own share of the budget and unused share flows to the next section:

| Section | Share | Selection | Rendered order |
| --- | --- | --- | --- |
| Header (harness, project, lineage) | fixed | | |
| Constraints and decisions | 27% | newest entries that fit | chronological |
| Current tasks | 8% | latest `task` entry only | as stored |
| Recent notes | 8% | newest `assistant_note` entries that fit | chronological |
| Past user requests (already handled) | 32% | newest entries that fit; an entry longer than 600 chars is truncated deterministically | chronological |
| Files changed | 7% | distinct paths across all `file_change` `paths`, newest change first | newest first, preserving path order within a change |
| Recent commands | 11% | up to 5 `command` entries, failures first then newest; command line (160 chars), `(exit: N)` or `(failed)` when known, 160-char output tail | newest first |
| Files inspected | 7% | distinct `file_ref` paths not already under Files changed | newest first |

Failed commands always carry a status line in stored text: the harness's own header (`Exit code: N`, `Process exited with code N`) or `Exit status: failed`.

Identical stored data yields byte-identical output, which keeps the injected prefix cacheable.

## Daemon HTTP API (mementod, JSON)

All routes under `http://127.0.0.1:7377`. Optional bearer token via `MEMENTO_TOKEN`; when set, every route except `/health` requires `Authorization: Bearer <token>`.

| Method and path | Body or query | Response |
| --- | --- | --- |
| `GET /health` | | `{ "ok": true, "qdrant": true }` |
| `PUT /v1/sessions` | `{ harness, project, session, parent?, parent_harness?, require_parent?, retention?, title? }`; merge semantics: absent fields keep stored values, `created` is set once | session record |
| `GET /v1/sessions` | `?harness?&project?&include_deleted?` | `{ sessions: [...] }` |
| `POST /v1/sessions/release` | `{ harness, project, session, reason? }` | `{ released, removed: [{ harness, session }] }` |
| `POST /v1/maintenance/sweep` | `{ project?, dry_run? }` | stale and removed session keys |
| `GET /v1/sessions/{harness}/{session}/lineage` | | `{ lineage: ["ses_child", "ses_parent", ...] }` |
| `POST /v1/ingest` | neutral ingest schema | `{ segments: n, skipped: n }` |
| `POST /v1/search` | `{ harness, project, session, query, scope?, k?, kinds? }` (empty harness and session allowed for project/global) | `{ hits: [{ id, score, kind, text, harness, session, ts, path? }] }` |
| `GET /v1/digest` | `?harness&project&session&budget` | `{ text, segments }` |
| `POST /v1/forget` | `{ harness, project?, session?, scope: "session" or "lineage" or "project", dry_run? }` | `{ deleted: n, would_delete: n, sessions: [{ harness, session }] }`; dry runs report matching segments and sessions without deleting or collecting |
| `GET /v1/stats` | `?harness?&project?&session?` | counts by kind |

Errors: `{ "error": { "code": "...", "message": "..." } }` with 4xx/5xx.

## CLI (memento)

`memento [--url URL] <command>`; `MEMENTO_URL` default `http://127.0.0.1:7377`.

- `health`
- `stats [--harness H] [--project P] [--session S]` (`{ total, by_kind }`; current project by default, all harnesses when omitted)
- `sessions list --harness H [--project P]`
- `sessions link --harness H --project P --child S --parent P2 [--parent-harness H2] [--title T] [--retention continuation|keep]`
- `sessions keep --harness H --session S [--project P] [--off]` (checks the session exists and is not deleted, then sets `keep` or restores `continuation`; project defaults to the current project)
- `sessions lineage --harness H --session S`
- `search <query> [--harness H] [--project P] [--session S] [--scope session|lineage|project|global] [--k 8] [--kind KIND]...` (current project by default; session and lineage require harness and session)
- `digest [--harness H --session S] [--project P] [--budget N]` (current project and its most recently active non-deleted session by default; selected session reported on stderr)
- `forget --harness H [--project P] (--session S | --lineage S | --whole-project) [--yes] [--dry-run]` (`--whole-project` requires `--project`; without `--yes` it asks on a TTY and refuses otherwise; dry runs never prompt and report matching segments and sessions)
- `gc [--project DIR] [--dry-run] [--json]` releases sessions whose harness data is gone.
- `ingest --file turns.json|-` (neutral schema, for harnesses without a plugin)
- `install-hooks claude-code|codex|agy [--project DIR | --user] [--dry-run]`
- `install-skills [--harness opencode|claude-code|codex|agy|all] [--project DIR | --user] --source DIR [--dry-run]`
- Global: `--url`, `--token`, `--json`, `--timeout-secs`.

## OpenCode adapter

Location: `adapters/opencode/` (Promise plugin, `@opencode/plugin`). Verified contracts:

- For OpenCode project id `global`, ownership additionally requires matching session and plugin `location.directory` values after trimming one trailing slash, and the project key is the plugin directory (live verification pending).
- Ingestion triggers: event `session.execution.succeeded` (`data: { sessionID }`) and `session.step.ended` (`data: { sessionID, assistantMessageID, finish }`, catches progress in turns that later fail or are aborted); then `ctx.session.context({ sessionID })` (messages after the last compaction) and ingest messages newer than the last ingested message id kept in `ctx.storage`. Assistant messages are re-sent while still growing (point ids are idempotent). On first sight of a session, ingest the full active context (backfill). Ingestion is serialized per session.
- Forks are linked to their parent and only post-fork messages are ingested; lineage also includes parent segments recorded after the fork point. A claimed continuation takes precedence over the fork parent.
- Flush before snapshot: when a switch is detected, the adapter awaits any in-flight ingestion and runs one synchronous ingest of the current context before fetching the digest, so failed, aborted, or just-finished turns are included.
- Transcript shapes: user `{ id, type: "user", time: { created }, text }`; assistant `{ id, type: "assistant", model, content: (text | reasoning | tool)[] }`; tool part `{ type: "tool", id, name, state }` with `state.input`, `state.content`, `state.error`.
- Jev at ingest: for each assistant text item, one `choice` question with choices `decision`, `constraint`, `task`, `note`; result becomes `kind_hint` (`note` maps to null). Failures fall back to null.
- Model switch detection: the `context` hook compares `event.model` (provider + id, variant ignored) with the model of the previous primary request for the session (stored). `session.model.selected` is only a hint; the hook is the source of truth.
- Snapshots: state per session is `{ origin, lastModel, snapshots: { "<provider>/<id>": { cutMessageID, digest } } }`. The origin of a session the adapter has no state for is the model of the most recent assistant record in `ctx.session.context` (so forks and sessions first seen mid-history keep the model that produced their history); only without such a record does it fall back to the selected or requested model. Before the first switch the session runs untouched on its origin model. On a request whose model differs from `lastModel`: if a snapshot for that model exists it is reused unchanged (keeps that model's prompt cache valid when you bounce back to it); otherwise the adapter flushes ingestion, fetches the digest once, and stores a new snapshot whose cut is the id of the last `user` message at that moment. Switching to a model whose window is smaller than the history makes OpenCode auto-compact before the `context` hook runs; the adapter's `compaction` hook detects that the compaction model differs from `lastModel` (or, without state, the last assistant model in history), flushes ingestion and sets `result = { summary: <framed digest>, metadata: { source: "memento" } }`, which skips the summarisation request. The target snapshot is stored as `{ cutMessageID: null, digest, compacted: true }` so the following context rewrite does not inject the digest again. Same-model compactions and any memento error fall back to native compaction. After the first switch every model, including the origin, is served from its snapshot, because the origin's cache has likely expired and its history now contains other models' turns.
- Rewrite (pure function, deterministic, byte-stable for identical inputs): the frozen digest is prepended as a text part to the first `user` message of the tail because some providers (such as `claude-agent`) drop extra system parts; if no `user` message remains, it falls back to an `event.system` text part. `event.messages` keeps the verbatim tail before the cut plus everything from the cut onward. The tail starts at the Nth-last `user` message before the cut (default 3) and then moves forward one `user` boundary at a time until the pre-cut tail is within `tailCharBudget` (default 40000 chars). If even the single last pre-cut turn exceeds the budget, tool-result texts inside the pre-cut tail are truncated deterministically (head 1000 + tail 500 chars). Cuts only happen at `user` boundaries so tool-call/result pairs stay intact. If the cut message id is no longer present in `event.messages` (OpenCode compacted after the switch), the rewrite keeps messages as given and only adds the digest.
- Persisted history is never touched; the hook changes only the outgoing request.
- Tool `memory_search({ query, scope? })`: calls `/v1/search` (k = 12). Jev `predicate` relevance runs in parallel only for prose kinds (`user_request`, `assistant_note`, `decision`, `constraint`, `task`); `file_change`, `command` and `file_ref` hits pass through on score because they are short and lexical. Keep hits with probability >= 0.5, return max 6 ordered by score. On any Jev error or timeout (3 s), fall back to the top 6 by score. Results arrive as tool output at the end of the context, which keeps the prefix cacheable.
- Command `memento-continue <session>`: links current session to parent via `PUT /v1/sessions`, then puts the digest as `ctx.session.instructions.entry.put({ key: "memento-continuation", ... })`.
- Options (`ctx.options`): `url`, `token`, `harness` (`opencode`), `tailUserTurns` (3), `tailCharBudget` (40000), `digestBudget` (12000), `subagentMemory` (`off`, optionally `digest`), `subagentDigestBudget` (6000), `jev` (true), `jevModel` (`opencode/jev-1.13-free`), `enabled` (true), `reconcile` (true).

## Deployment

- `deploy/compose.yaml`: `qdrant` (`qdrant/qdrant:v1.20.2`, named volume for `/qdrant/storage`, no host port by default) and `mementod` (port `127.0.0.1:7377`, `depends_on` qdrant healthy).
- Qdrant healthcheck without curl: `bash -c ':> /dev/tcp/127.0.0.1/6333'` (image is Debian based; verify bash presence during implementation).
- `mementod` image: multi-stage, glibc Debian builder and runtime (ORT prebuilt binaries are glibc only and need libc++). The embedding model is downloaded at build time by `mementod warmup` into `/opt/memento/models` (`FASTEMBED_CACHE_DIR`), so containers start offline.

## Cross-harness (v1.1)

Claude Code and Codex store JSONL transcripts and expose command hooks that receive JSON on stdin with `session_id`, `transcript_path`, `cwd`, and `hook_event_name`; `SessionStart` accepts `hookSpecificOutput.additionalContext`. agy 1.5.12 also supports lifecycle hooks and exports JSONL transcripts.

Harness ids: `opencode`, `claude-code`, `codex`, `agy`.

### Identity

- `project` is the canonical absolute path of the main checkout: the parent of `git rev-parse --path-format=absolute --git-common-dir` (so linked worktrees map to their main repository), or the canonical cwd outside git. The OpenCode adapter uses `ctx.location.project.canonical` (OpenCode's saved canonical checkout directory) and falls back to `ctx.location.project.id`. `memento project-key [DIR]` prints the key. Existing data keyed by OpenCode project ids is not migrated.
- A session is identified by the pair `(harness, session)`. Session records gain `parent_harness` (defaults to the session's own harness), so a Claude Code session can continue an OpenCode session.
- Lineage, ancestors, and descendants are computed over `(harness, session)` pairs (cycle-safe across harnesses). Qdrant filters for session and lineage scope are a `should` of `must(harness, session)` pairs.

### Scopes

- `session`, `lineage`: pair-based as above, and also filtered by the request's `project` (search and forget), so a session id cannot reach another project.
- `project`: every harness, filtered by `project`.
- `global`: everything.

### Pending continuations

`PUT /v1/continuations { project, parent_harness, parent_session, target_harness? }` stores one pending continuation per `(project, target_harness)` slot; an absent target is a separate any-harness slot. A put replaces only its slot. `POST /v1/continuations/claim { project, harness, session }` first returns an existing unexpired claim by that caller, then tries the caller's targeted slot followed by the any-harness slot. Expired slots are deleted; a slot claimed by another caller is skipped. A successful claim records `claimed_by` and `claimed_at`, links the session (`parent_harness`, `parent`), and returns `{ claimed: bool, parent_harness?, parent?, digest? }` with the lineage digest. Claims are idempotent per caller for 24 h; unclaimed slots expire 24 h after creation, and claimed slots expire 24 h after claiming. Legacy project-only-ID records remain claimable until expiry or replacement. Project-scope forget removes all continuation slots for the project. Links, continuations and claims are rejected with 409 `cross_project` when parent and child projects differ, and with 409 `too_deep` when the lineage would exceed 64. CLI: `memento continue --from <harness>/<session> [--project DIR] [--to HARNESS]`. Every adapter claims on its first event for a new session.

When OpenCode claims during a context hook, the claiming request also carries the framed continuation in its first user message, or in system text if no user message exists.

### Harness lifecycle

| Harness | Track | Inject context | Import turns | Deletion signal | Subagent rule |
| --- | --- | --- | --- | --- | --- |
| OpenCode | Plugin session events | Plugin claim and model-switch rewrite | Plugin ingest | `session.deleted`; startup and daily reconciliation | Exclude sessions with `parentID`; searches resolve to the root non-subagent ancestor |
| Claude Code | `SessionStart` hook | Claimed digest in `additionalContext` | `Stop` transcript import | `memento gc` | Skip inputs with nonempty `agent_id`; subagent hooks are not installed |
| Codex | `SessionStart` hook | Claimed digest in `additionalContext` | `Stop` transcript import | `memento gc`; `SessionEnd` is not a delete signal | Skip inputs with nonempty `agent_id`; subagent hooks are not installed |
| agy | `SessionStart` hook | Claimed digest in `injectSteps` | `Stop` transcript import | `memento gc` | Skip inputs with nonempty `agent_id`; per-subagent hook behavior is unverified |
| Generic | `session_start` hook | Returned `context` | `turn_end` transcript or items | `session_deleted` or supported-harness gc | Skip nonempty `agent_id` or `is_subagent: true` |

OpenCode excludes sessions with `parentID` from registration, ingestion, claims, switch rewriting, and compaction handling. Their `memory_search` resolves to the root non-subagent ancestor in the same project, using bounded cached lookups; continuation and handoff commands are unavailable. The experimental `subagentMemory: "digest"` option injects a frozen root digest into subagent context requests if OpenCode invokes the hook for them, which remains unverified. Forks remain independent sessions. At startup and every 24 hours, unless `reconcile: false`, the adapter checks registered OpenCode sessions and releases confirmed deleted sessions (`SessionNotFoundError`) and previously registered subagents. Claude Code and Codex emit `SubagentStart` and `SubagentStop` with `agent_id` and `agent_type`, but these hooks are not installed. agy documents subagents without subagent hook fields; whether `SessionStart` fires for them is unverified.

#### Generic hook contract

Invoke `memento hook generic --harness NAME` with JSON on stdin:

```json
{
  "event": "session_start|turn_end|session_end|session_deleted",
  "session_id": "session-id",
  "cwd": "/absolute/project/path",
  "transcript_path": "/optional/transcript.jsonl",
  "transcript_format": "claude-code|codex|agy",
  "items": [],
  "parent": { "harness": "opencode", "session": "parent-id" },
  "agent_id": "optional-subagent-id",
  "is_subagent": false
}
```

Only `event`, `session_id`, and `cwd` are required; `items` contains neutral `IngestItem` objects. `session_start` registers the session, optionally links its parent, claims a continuation, and prints `{"context":"<framed digest>"}` or `{"context":null}`. `turn_end` imports a transcript when `transcript_path` is present, requiring `transcript_format` and taking precedence over `items`; otherwise it ingests the items. `session_deleted` releases with reason `deleted`; `session_end` is a no-op. Unknown events print `{}` and warn on stderr. A nonempty `agent_id` or true `is_subagent` prints `{}` and does nothing.

#### Retention lifecycle

`POST /v1/sessions/release` marks a matching session deleted under the operation write lock and registry lock, then collects eligible deleted sessions throughout the project to a fixpoint. Its optional reason is stored as `deleted_reason` (`deleted`, `stale`, or `subagent`). A missing or project-mismatched session returns `released: false`. A deleted session is retained for dependents until it is not `keep`, has no uncollected child in any harness, and is not the parent of an unexpired unclaimed continuation. Collection deletes its segments leaf-first and slots referencing it as parent or claimant, and preserves a tombstone with `collected_at`. Collected tombstones older than seven days are pruned during collection. Explicit `forget` runs collection afterwards, allowing deleted ancestors to cascade.

Deleted records reject resurrection: ingest stores nothing and session updates return the existing record. A continuation put against a deleted parent returns 409 `parent_deleted`; strict links with `require_parent: true` return 409 `parent_unavailable` if the parent is missing or deleted. Deleted but uncollected ancestors remain visible in descendant lineage search and digest, but not project or global search. Collected tombstones never appear in search or digest. Session listing excludes deleted records unless `include_deleted=true`.

The daemon records activity on ingest, session updates, continuation claims, search, and digest, throttled to one write per 15 minutes. Startup backfills missing timestamps. A periodic sweep controlled by `MEMENTO_STALE_DAYS` (default 7, zero disables) and `MEMENTO_STALE_SWEEP_SECS` (default 3600) marks inactive sessions stale only when their uncollected descendants are also inactive. For testing, a valid non-negative integer `MEMENTO_STALE_SECS` overrides days; zero disables expiry, and an invalid value falls back to days. Unexpired unclaimed continuation slots protect their parent and ancestors; `keep` sessions are never marked. The collector then removes eligible segments. `POST /v1/maintenance/sweep` accepts optional `project` and `dry_run` and reports stale and removed session keys.

`memento gc` uses the canonical project key for its directory, skips fresh pending imports, and reports alive, deleted and released, retained, and unknown sessions. Each `SessionStart` hook also starts detached gc at most once per 24 hours per project; `MEMENTO_AUTO_GC=0` disables it. OpenCode's API reports missing sessions with `HTTP 404` or `SessionNotFoundError`; other failures, including an unreachable server, are unknown. `MEMENTO_OPENCODE_BIN` overrides the executable. Claude Code silently deletes transcripts after `cleanupPeriodDays` (default 30); gc checks cursor paths or searches `~/.claude/projects` (or `CLAUDE_CONFIG_DIR/projects`), treating a missing projects directory as unknown. Codex checks both `~/.codex/sessions` and `archived_sessions` (under `CODEX_HOME` when set); archive is not deletion, and its `SessionEnd` reason is only `other` even on archive or delete. agy is deleted when both `~/.gemini/antigravity-cli/conversations/<id>.db` and `~/.gemini/antigravity-cli/brain/<id>/` are gone. Successful releases prune cursor entries under the cursor lock; dry runs do not mutate anything.

### Transcript importers (CLI)

`memento import claude-code|codex|agy --transcript PATH [--session ID] [--project DIR]` parses the JSONL into neutral items and posts them. Incremental: a cursor file in `${XDG_STATE_HOME:-~/.local/state}/memento/cursors.json` keyed by canonical transcript path stores the byte offset of the last fully parsed line; only new complete lines are sent. A tool call without a result holds the cursor until its result arrives or a later real user turn appears, after which it is treated as abandoned. Codex command outputs are decoded from plain text (`Process exited with code N`, `Exit code: N`) or JSON wrappers (`output`, `metadata.exit_code`). Jev classification (when available) runs on scaffolding-stripped text and skips items that are empty after stripping. Reasoning (`thinking`, `reasoning`) is dropped. Claude Code records with `isMeta: true`, `isSidechain` subagent transcripts, and harness command/caveat wrappers are marked `injected` or stripped by the policy. Codex `session_meta`, `turn_context`, `<environment_context>` and `<user_instructions>` user messages are marked `injected`. agy records include `USER_INPUT`, `SYSTEM_MESSAGE` (hook injections, skipped), `PLANNER_RESPONSE`, and `GENERIC`.

### Hooks (CLI)

`memento hook claude-code|codex|agy` reads the hook JSON from stdin:
- `SessionStart`: claim a pending continuation for `project-key(cwd)`; if claimed, print the `additionalContext` JSON with the framed digest (`<memento-continuation>` wrapper, reference data, not instructions).
- `Stop`: the installed synchronous hook atomically writes a timestamped pending marker in `${XDG_STATE_HOME:-~/.local/state}/memento/pending/<harness>-<sanitized-session>`, spawns a detached incremental importer of `transcript_path`, and returns `{}`. The importer removes the marker after success or failure and appends output to `${XDG_STATE_HOME:-~/.local/state}/memento/import.log` when available. `MEMENTO_HOOK_SYNC=1` imports synchronously without a marker; a spawn failure also falls back to synchronous import. Async hooks are dropped by `claude -p` and `codex exec` in headless runs (verified 2026-11-19).
- `memento continue` waits for the source session's pending marker before calling the daemon, polling every 100 ms for at most 30 s and warning on timeout; markers at least 60 s old are removed and ignored. Overlapping imports share one marker, so the first importer to finish can remove it while another is still pending. `SessionStart` claims do not wait.
- `SessionEnd`: fast no-op and not installed (Claude Code gives all `SessionEnd` hooks a shared 1.5 s budget, Codex 1-3 s; verified 2026-11-19).
- `SessionStart`: overall deadline 8 s, then prints nothing.
- Optional Jev classification of imported text: when `opencode` is on PATH and `MEMENTO_JEV` is not `0`, the CLI calls `opencode api post /api/experimental/generate/decisions` with `opencode/jev-1.13-free` (user text: `decision|constraint|request`; assistant text: `decision|constraint|task|note`; confidence >= 0.6), at most 12 items per import, 5 s each, with a 10 minute cooldown file after a failure. Never on `SessionStart`.
- Hook failures exit 0 with a message on stderr; detached import failures are logged when possible. Synchronous fallback and the explicit sync escape hatch can block the harness.
- Output contract: Claude Code and Codex `SessionStart` print `hookSpecificOutput` JSON or nothing; `Stop` prints `{}` (Codex rejects plain text on `Stop`); `SessionEnd` prints nothing.
- Codex specifics (developers.openai.com/codex/hooks, verified 2026-11-18): `~/.codex/hooks.json` (or `[hooks]` in `config.toml`), matcher groups `{ "matcher"?, "hooks": [{ "type": "command", "command", "timeout"?, "async"?, "additionalContextLimit"? }] }`; `SessionEnd` timeout is 1 s by default and at most 3 s, so imports run from `Stop`; `SessionStart` sources `startup|resume|clear|compact|fork` (on `compact` the digest is re-injected); non-managed hooks need trust via `/hooks`, or `--dangerously-bypass-hook-trust` for scripted runs; default `additionalContext` spill threshold about 2500 tokens (`additionalContextLimit` raises it).
- agy 1.5.12 uses project `.agents/hooks.json` or global `~/.gemini/config/hooks.json`; `SessionStart` and `Stop` fire in `agy -p` and interactive sessions. Input uses camelCase fields including `conversationId`, `cwd`, `transcriptPath`, `workspacePaths`, and `hookEventName`. A claim outputs `{"injectSteps":[{"systemMessage":{"systemMessage":"..."}}]}`. `transcriptPath` can be a `.system_generated/` directory or `logs/transcript_full.jsonl`.
- `memento install-hooks claude-code|codex|agy [--project DIR | --user] [--dry-run]` prints or merges hook entries under an exclusive lock, preserving existing configuration and file permissions, removing memento `SessionEnd` entries and `async` from memento `Stop` entries from older installs, and writing a `.bak` copy with the original mode.

### MCP server

`memento-mcp --harness NAME` (crate `memento-mcp`, `rmcp` 3.x) optionally serves stdio MCP tools for agy and other MCP-capable harnesses: `memory_search { query, scope?, project?, session? }`, `memory_digest { project?, harness?, session? }`, `memory_continue { from_harness, from_session }` (claims nothing, returns the parent digest), and `memory_note { kind: decision|constraint|task|note, text, session? }` which stores an explicit note (default session `<harness>-notes`, project from the server's cwd).

### Hardening (2026-11-19)

- Ingest: `IngestResponse.duplicates` counts prose segments skipped as near-duplicates of an existing segment in the same project and the same lineage (the session and its ancestors; an unlinked session repeating a fact is stored) (dense cosine >= 0.95 and 3-gram word-shingle Jaccard >= 0.8; `MEMENTO_DUPLICATE_COSINE_THRESHOLD`, `MEMENTO_DUPLICATE_JACCARD_THRESHOLD`). This curbs memory echo when a model restates the digest.
- Re-ingesting a source with fewer chunks deletes its higher-index chunks.
- User items honor `kind_hint` `decision` and `constraint` only; user text never becomes `task`.
- Per-session locks for ingest; a short registry lock for registry writes and claims; embedding and upserts run outside global locks.
- OpenCode adapter: no snapshot after a failed pre-switch flush; bounded in-memory caches; claim completion persisted only after the continuation instruction is written; mid-history `system` messages are hoisted into system parts when rewriting (Anthropic-protocol providers reject them).
- OpenCode adapter ignores sessions outside the plugin instance's project.
- OpenCode code-mode `execute` stays ignored: verified on 33 local calls that it only wraps catalog and MCP tools, never shell or edits.
- Deployment: the daemon listens on `0.0.0.0` inside the container but the host port is `127.0.0.1` only; the Compose network contains only `qdrant` and `mementod`. Set `MEMENTO_TOKEN` if you attach other containers to that network.

## Non-goals for v1

- Proactive per-turn recall injection (needs deterministic per-message injection storage to stay cache-safe).
- General-purpose summarization or custom compaction beyond the model-switch digest.
