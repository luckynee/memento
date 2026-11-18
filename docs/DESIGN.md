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
| Fusion | Qdrant RRF (`Fusion::Rrf`) | recency rerank done in Rust, see Search |
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
| tool in edit set (`edit`, `write`, `patch`, `apply_patch`, `multiedit`) | `file_change` | `<tool> <paths>` plus input/diff truncated to 2000 chars |
| tool in shell set (`bash`, `shell`) | `command` | command line plus output tail: last 1000 chars when `ok == false`, last 400 chars when `ok == true` |
| tool in read set (`read`, `grep`, `glob`, `list`, `webfetch`, `websearch`) | `file_ref` | `<tool> <path or pattern>` only, no output |
| todo tools (`todowrite`) | `task` | rendered todo list |
| subagent tools (`task`, `subagent`) | `assistant_note` | `<tool>` plus output truncated to 2000 chars (subagent reports are dense summaries) |
| any other tool (MCP, LSP, unknown) | dropped by default | configurable through `SegmentPolicy` |

- Chunking: max 1000 chars, 150 char overlap, split on paragraph, then line, then whitespace boundaries. BGE-small-en-v1.5 truncates input at 512 tokens and code tokenizes densely, so chunks stay well under that limit. Every segment kind is chunked, including `file_change` and `command` text.
- Path is taken from input keys `filePath`, `path`, `file`, `pattern` (first present); `webfetch` uses `url`, `websearch` uses `query`. For patch-style input (`patch`, `patchText`, `diff`, or `input` string) paths are parsed from `*** Add File: `, `*** Update File: `, `*** Delete File: `, `*** Move to: ` and `+++ b/` lines; multiple paths are joined with `, ` in text and the first path is stored in `path`.
- Point id: UUIDv5 over `harness/session/source_id/chunk_index`, so re-ingest is idempotent.
- Empty segments are skipped.

## Qdrant data model

Collection `memento_segments`:

- named dense vector `dense`: 384, cosine.
- named sparse vector `bm25`: `Modifier::Idf`, filled server-side with `Document { model: "qdrant/bm25" }`.
- payload: `harness`, `project`, `session`, `kind`, `text`, `source_id`, `chunk_index`, `ts` (RFC 3339), `tool` (optional), `path` (optional).
- keyword indexes: `project` (`is_tenant = true`), `harness`, `session`, `kind`, `path`; datetime index on `ts`.

Collection `memento_sessions` (registry): payload-only points if the Rust client supports a collection without vectors, otherwise a 1-dim dummy dense vector. Point id: UUIDv5 over `harness/session`.

- payload: `harness`, `project`, `session`, `parent` (optional), `retention`, `created`, `title` (optional).
- keyword indexes: `harness`, `project`, `session`, `parent`.
- Linking rejects cycles and self-parenting (409).

Lineage terms:

- ancestors(S): S, its parent, the parent's parent, up to the root (cycle-safe, max depth 64).
- descendants(S): S and every session whose ancestor chain contains S.
- Search scope `lineage` uses ancestors(S). `forget` scope `lineage` uses descendants(S), so forgetting a continuation never deletes the parent history that siblings still use.

Search:

1. Qdrant query: prefetch dense (limit 50) and bm25 (limit 50), both under the scope filter and optional `kinds` filter, fused with RRF at the top level with an explicit `limit` of 50. No formula query: RRF scores are sums of `1/(k + rank)` and Qdrant's docs warn that an unweighted decay term dominates them.
2. Rerank in Rust (pure, unit-tested): `normalized = rrf_score / max_rrf_score`, `final = normalized + recency_weight * exp(-ln2 * age / half_life)` where age is measured against the newest `ts` in the candidate set (not wall clock, so results are reproducible), defaults `recency_weight = 0.15`, `half_life = 24h`. Ties break on `(ts desc, source_id, chunk_index)`. Return top `k` (default 8, max 50).

## Digest

`GET /v1/digest` builds a deterministic working-state document for a session lineage (ancestors) within a char budget (default 12000). Segments are loaded fully by scroll and sorted by `(ts, source_id, chunk_index)` before rendering; chunks of one source are re-joined (overlap removed) and rendered as one entry, never split mid-entry. Each section has its own share of the budget and unused share flows to the next section:

| Section | Share | Selection | Rendered order |
| --- | --- | --- | --- |
| Header (harness, project, lineage) | fixed | | |
| Constraints and decisions | 35% | newest entries that fit | chronological |
| Current tasks | 10% | latest `task` entry only | as stored |
| User requests | 35% | newest entries that fit; an entry longer than 1500 chars is truncated deterministically | chronological |
| Files changed | 10% | distinct paths from `file_change`, newest first | newest first |
| Recent notes | 10% | newest `assistant_note` entries that fit | chronological |

Identical stored data yields byte-identical output, which keeps the injected prefix cacheable.

## Daemon HTTP API (mementod, JSON)

All routes under `http://127.0.0.1:7377`. Optional bearer token via `MEMENTO_TOKEN`; when set, every route except `/health` requires `Authorization: Bearer <token>`.

| Method and path | Body or query | Response |
| --- | --- | --- |
| `GET /health` | | `{ "ok": true, "qdrant": true }` |
| `PUT /v1/sessions` | `{ harness, project, session, parent?, retention?, title? }`; merge semantics: absent fields keep stored values, `created` is set once | session record |
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

- `health`
- `stats --harness H [--project P] [--session S]` (`{ total, by_kind }`)
- `sessions list --harness H [--project P]`
- `sessions link --harness H --project P --child S --parent P2 [--title T] [--retention continuation|keep]`
- `sessions lineage --harness H --session S`
- `search <query> --harness H --project P --session S [--scope lineage] [--k 8] [--kind KIND]...`
- `digest --harness H --project P --session S [--budget N]`
- `forget --harness H [--project P] (--session S | --lineage S | --whole-project) [--yes]` (`--whole-project` requires `--project`; without `--yes` it asks on a TTY and refuses otherwise)
- `ingest --file turns.json|-` (neutral schema, for harnesses without a plugin)
- Global: `--url`, `--token`, `--json`, `--timeout-secs`.

## OpenCode adapter

Location: `adapters/opencode/` (Promise plugin, `@opencode/plugin`). Verified contracts:

- Ingestion triggers: event `session.execution.succeeded` (`data: { sessionID }`) and `session.step.ended` (`data: { sessionID, assistantMessageID, finish }`, catches progress in turns that later fail or are aborted); then `ctx.session.context({ sessionID })` (messages after the last compaction) and ingest messages newer than the last ingested message id kept in `ctx.storage`. Assistant messages are re-sent while still growing (point ids are idempotent). On first sight of a session, ingest the full active context (backfill). Ingestion is serialized per session.
- Flush before snapshot: when a switch is detected, the adapter awaits any in-flight ingestion and runs one synchronous ingest of the current context before fetching the digest, so failed, aborted, or just-finished turns are included.
- Transcript shapes: user `{ id, type: "user", time: { created }, text }`; assistant `{ id, type: "assistant", model, content: (text | reasoning | tool)[] }`; tool part `{ type: "tool", id, name, state }` with `state.input`, `state.content`, `state.error`.
- Jev at ingest: for each assistant text item, one `choice` question with choices `decision`, `constraint`, `task`, `note`; result becomes `kind_hint` (`note` maps to null). Failures fall back to null.
- Model switch detection: the `context` hook compares `event.model` (provider + id, variant ignored) with the model of the previous primary request for the session (stored). `session.model.selected` is only a hint; the hook is the source of truth.
- Snapshots: state per session is `{ origin, lastModel, snapshots: { "<provider>/<id>": { cutMessageID, digest } } }`. Before the first switch the session runs untouched on its origin model. On a request whose model differs from `lastModel`: if a snapshot for that model exists it is reused unchanged (keeps that model's prompt cache valid when you bounce back to it); otherwise the adapter flushes ingestion, fetches the digest once, and stores a new snapshot whose cut is the id of the last `user` message at that moment. After the first switch every model, including the origin, is served from its snapshot, because the origin's cache has likely expired and its history now contains other models' turns.
- Rewrite (pure function, deterministic, byte-stable for identical inputs): the frozen digest is appended as an extra `event.system` text part (avoids consecutive `user` messages); `event.messages` keeps the verbatim tail before the cut plus everything from the cut onward. The tail starts at the Nth-last `user` message before the cut (default 3) and then moves forward one `user` boundary at a time until the pre-cut tail is within `tailCharBudget` (default 40000 chars). If even the single last pre-cut turn exceeds the budget, tool-result texts inside the pre-cut tail are truncated deterministically (head 1000 + tail 500 chars). Cuts only happen at `user` boundaries so tool-call/result pairs stay intact. If the cut message id is no longer present in `event.messages` (OpenCode compacted after the switch), the rewrite keeps messages as given and only adds the digest.
- Persisted history is never touched; the hook changes only the outgoing request.
- Tool `memory_search({ query, scope? })`: calls `/v1/search` (k = 12). Jev `predicate` relevance runs in parallel only for prose kinds (`user_request`, `assistant_note`, `decision`, `constraint`, `task`); `file_change`, `command` and `file_ref` hits pass through on score because they are short and lexical. Keep hits with probability >= 0.5, return max 6 ordered by score. On any Jev error or timeout (3 s), fall back to the top 6 by score. Results arrive as tool output at the end of the context, which keeps the prefix cacheable.
- Command `memento-continue <session>`: links current session to parent via `PUT /v1/sessions`, then puts the digest as `ctx.session.instructions.entry.put({ key: "memento-continuation", ... })`.
- Options (`ctx.options`): `url`, `token`, `harness` (`opencode`), `tailUserTurns` (3), `tailCharBudget` (40000), `digestBudget` (12000), `jev` (true), `jevModel` (`opencode/jev-1.13`), `enabled` (true).

## Deployment

- `deploy/compose.yaml`: `qdrant` (`qdrant/qdrant:v1.20.2`, named volume for `/qdrant/storage`, no host port by default) and `mementod` (port `127.0.0.1:7377`, `depends_on` qdrant healthy).
- Qdrant healthcheck without curl: `bash -c ':> /dev/tcp/127.0.0.1/6333'` (image is Debian based; verify bash presence during implementation).
- `mementod` image: multi-stage, glibc Debian builder and runtime (ORT prebuilt binaries are glibc only and need libc++). The embedding model is downloaded at build time by `mementod warmup` into `/opt/memento/models` (`FASTEMBED_CACHE_DIR`), so containers start offline.

## Non-goals for v1

- Proactive per-turn recall injection (needs deterministic per-message injection storage to stay cache-safe).
- Custom compaction via the `compaction` hook.
- Adapters for other harnesses (the neutral `ingest` CLI is the escape hatch).
