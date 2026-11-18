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
- Jev (System 1) is used only inside the OpenCode adapter through `ctx.experimental.generate.decisions` with model `{ providerID: "opencode", id: "jev-1.13" }`. Jev is a decision model: `predicate`, `choice`, and `score` questions; it cannot embed or generate text.
- Deployment: Docker Compose with `qdrant/qdrant:v1.20.2` and a `mementod` image. The CLI runs on the host.

## Pinned stack

| Item | Version | Notes |
| --- | --- | --- |
| Qdrant server | `qdrant/qdrant:v1.20.2` | HTTP 6333, gRPC 6334, storage `/qdrant/storage`, health `/healthz` `/readyz`; image has no curl/wget |
| `qdrant-client` | 1.20.0 | gRPC only, connect to 6334 |
| `fastembed` | 7.7.1 | `EmbeddingModel::BGESmallENV15`, 384 dims, cosine. Pins `ort 2.0.0-rc.15` (Rust >= 1.92, glibc only, x86-64-v3, needs libc++ at runtime). Cache via `FASTEMBED_CACHE_DIR` |
| Lexical | Qdrant server-side `qdrant/bm25` | Self-hosted since 1.15.3, via `Document` / `DocumentBuilder`, sparse vector with `Modifier::Idf` |
| Recency | Qdrant formula query (>= 1.14) | `FormulaBuilder`, `Expression::exp_decay` on datetime payload `ts` |
| OpenCode | v2.3.2, `@opencode/plugin` 2.x | Promise plugin API |

## Identity and scope

- `harness`: adapter id, for example `opencode`.
- `project`: canonical project key (OpenCode `ctx.location.project.id`).
- `session`: harness session id.
- Lineage: a session may declare a `parent` session (continuation). Lineage of S is S plus all ancestors.
- Search scopes: `session`, `lineage` (default), `project`, `global`.
- Retention mode per session: `continuation` (default, user deletes when done) or `keep`. Forgetting is explicit: `memento forget --session|--lineage|--project`.

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
| tool in edit set (`edit`, `write`, `patch`, `apply_patch`, `multiedit`) | `file_change` | `<tool> <path>` plus input/diff truncated to 2000 chars |
| tool in shell set (`bash`, `shell`) | `command` | command line; when `ok == false`, plus last 1000 chars of output |
| tool in read set (`read`, `grep`, `glob`, `list`, `webfetch`, `websearch`) | `file_ref` | `<tool> <path or pattern>` only, no output |
| todo tools (`todowrite`) | `task` | rendered todo list |
| any other tool | dropped | |

- Chunking: max 1500 chars, 200 char overlap, split on paragraph then line boundaries.
- Path is taken from input keys `filePath`, `path`, `file`, `pattern` (first present).
- Point id: UUIDv5 over `harness/session/source_id/chunk_index`, so re-ingest is idempotent.
- Empty segments are skipped.

## Qdrant data model

Collection `memento_segments`:

- named dense vector `dense`: 384, cosine.
- named sparse vector `bm25`: `Modifier::Idf`, filled server-side with `Document { model: "qdrant/bm25" }`.
- payload: `harness`, `project`, `session`, `kind`, `text`, `source_id`, `chunk_index`, `ts` (RFC 3339), `tool` (optional), `path` (optional).
- keyword indexes: `project` (`is_tenant = true`), `harness`, `session`, `kind`, `path`; datetime index on `ts`.

Collection `memento_sessions` (registry, 1-dim dummy vector or payload-only points):

- payload: `harness`, `project`, `session`, `parent` (optional), `retention`, `created`, `title` (optional).

Search: prefetch dense (limit 50) and bm25 (limit 50) under the scope filter, RRF fusion, then formula rescore `score + 0.1 * exp_decay(ts, scale 7 days)`, limit `k` (default 8).

## Digest

`GET /v1/digest` builds a deterministic working-state document for a session lineage within a char budget (default 12000):

1. Header: harness, project, session lineage.
2. `constraint` and `decision` segments, oldest first.
3. Latest `task` segment.
4. `user_request` segments, newest first until budget.
5. Distinct `file_change` paths, newest first.

Deterministic output for the same stored data keeps the injected prefix byte-stable, which preserves prompt caching on the new model.

## Daemon HTTP API (mementod, JSON)

All routes under `http://127.0.0.1:7377`. Optional bearer token via `MEMENTO_TOKEN`; when set, every route except `/health` requires `Authorization: Bearer <token>`.

| Method and path | Body or query | Response |
| --- | --- | --- |
| `GET /health` | | `{ "ok": true, "qdrant": true }` |
| `PUT /v1/sessions` | `{ harness, project, session, parent?, retention?, title? }` | session record |
| `GET /v1/sessions` | `?harness&project` | `{ sessions: [...] }` |
| `GET /v1/sessions/{harness}/{session}/lineage` | | `{ lineage: ["ses_child", "ses_parent", ...] }` |
| `POST /v1/ingest` | neutral ingest schema | `{ segments: n, skipped: n }` |
| `POST /v1/search` | `{ harness, project, session, query, scope?, k?, kinds? }` | `{ hits: [{ id, score, kind, text, session, ts, path? }] }` |
| `GET /v1/digest` | `?harness&project&session&budget` | `{ text, segments }` |
| `POST /v1/forget` | `{ harness, project?, session?, scope: "session" or "lineage" or "project" }` | `{ deleted: n }` |
| `GET /v1/stats` | `?harness&project&session` | counts by kind |

Errors: `{ "error": { "code": "...", "message": "..." } }` with 4xx/5xx.

## CLI (memento)

`memento [--url URL] <command>`; `MEMENTO_URL` default `http://127.0.0.1:7377`.

- `health`, `stats`, `sessions list`, `sessions link --child S --parent P`
- `search <query> --harness H --session S [--scope lineage] [--k 8] [--json]`
- `digest --harness H --session S [--budget N]`
- `forget --harness H (--session S | --lineage S | --project P) [--yes]`
- `ingest --file turns.json` (neutral schema, for harnesses without a plugin)

## OpenCode adapter

Location: `adapters/opencode/` (Promise plugin, `@opencode/plugin`). Verified contracts:

- Ingestion trigger: event `session.execution.succeeded` (`data: { sessionID }`); then `ctx.session.context({ sessionID })` (messages after the last compaction) and ingest messages newer than the last ingested message id kept in `ctx.storage`. On first sight of a session, ingest the full active context (backfill).
- Transcript shapes: user `{ id, type: "user", time: { created }, text }`; assistant `{ id, type: "assistant", model, content: (text | reasoning | tool)[] }`; tool part `{ type: "tool", id, name, state }` with `state.input`, `state.content`, `state.error`.
- Jev at ingest: for each assistant text item, one `choice` question with choices `decision`, `constraint`, `task`, `note`; result becomes `kind_hint` (`note` maps to null). Failures fall back to null.
- Model switch: event `session.model.selected` (`data: { sessionID, model, previous? }`) and the `context` hook `event.model`. The origin model is the first model seen for the session (stored). When `event.model` differs from origin in provider or id, the hook rewrites `event.messages`: keep system messages, insert one user message containing the frozen digest (computed once at switch time, stored), keep the verbatim tail starting at the Nth-last `user` message before the switch point (default 3 user turns) and everything after the switch point. Cuts only happen at `user` message boundaries so tool-call/result pairs stay intact. Switching back to the origin model restores full history (the hook only changes the outgoing request; persisted history is untouched).
- Tool `memory_search({ query, scope? })`: calls `/v1/search` (k = 12), then Jev `predicate` relevance per hit in parallel, returns hits with probability >= 0.5 (max 6). Results arrive as tool output at the end of the context, which keeps the prefix cacheable.
- Command `memento-continue <session>`: links current session to parent via `PUT /v1/sessions`, then puts the digest as `ctx.session.instructions.entry.put({ key: "memento-continuation", ... })`.
- Options (`ctx.options`): `url`, `token`, `tailUserTurns`, `digestBudget`, `jev` (bool), `jevModel`.

## Non-goals for v1

- Proactive per-turn recall injection (needs deterministic per-message injection storage to stay cache-safe).
- Custom compaction via the `compaction` hook.
- Adapters for other harnesses (the neutral `ingest` CLI is the escape hatch).
