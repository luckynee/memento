# Map

| Path | Purpose |
| --- | --- |
| `AGENTS.md` | Agent conventions |
| `docs/DESIGN.md` | Architecture, pinned versions, HTTP API, segmentation policy, adapter contracts |
| `crates/memento-core/src/protocol.rs` | Wire types shared by daemon, CLI, and adapters |
| `crates/memento-core/src/segment.rs` | `SegmentPolicy` (tool sets, truncation and chunk limits) and `segment_items` |
| `crates/memento-core/src/ids.rs` | `POINT_NAMESPACE`: changing it changes every stored point id |
| `crates/memento-core/src/lineage.rs` | `ancestors`, `descendants`, `would_create_cycle` |
| `crates/memento-core/src/rerank.rs` | RRF normalization + recency rerank (no wall clock) |
| `crates/memento-core/src/digest.rs` | `render_digest`, `merge_chunks`; byte-stable output |
| `crates/mementod/src/routes.rs` | All HTTP routes, bearer auth, error mapping |
| `crates/mementod/src/store.rs` | Qdrant schema, segment upsert (dense + bm25 Document), hybrid RRF candidates, session registry |
| `crates/mementod/src/embed.rs` | `Embedder` trait, `FastEmbedder` (spawn_blocking), `FakeEmbedder` for tests |
| `crates/mementod/tests/` | Live Qdrant tests, gated on `MEMENTO_TEST_QDRANT_URL` |
| `crates/memento-cli/src/main.rs` | `memento` CLI, HTTP client of mementod |
| `adapters/opencode/src/` | OpenCode V2 plugin: `client`, `transcript`, `ingest`, `switch` (pure rewrite), `switch-controller`, `tools`, `commands`, `options`, `index` |
| `adapters/opencode/test/` | `bun test` suites (mocked daemon and Jev) |
| `deploy/` | `Dockerfile` (rust:1.95-trixie builder, debian:trixie-slim + libc++1 runtime, model baked by `mementod warmup`), `compose.yaml` |

## Adapter storage keys

- `memento/ingest/<sessionID>`: ingest cursor
- `memento/registered/<sessionID>`: session registered with the daemon
- `memento/switch/<sessionID>`: `{ origin, lastModel, snapshots }`

## Verified runtime facts

- OpenCode v2.3.2 `context` hook: user and assistant messages carry `id` (`msg_...`); tool-result messages have no `id`. Verified with a probe plugin on a standalone server, 2026-11-18.
- `@opencode/plugin` 2.2.5 decision `predicate` questions take `{ type, name, instructions }` only (no `criteria`).
- Qdrant v1.20.2 self-hosted: vectorless collections accept payload-only points; sparse `bm25` with `modifier: idf` and server-side `qdrant/bm25` documents works for upsert and query. Image has bash, no curl/wget.
- Qdrant gRPC (qdrant-client 1.20.0): payload-only points need an explicitly empty named-vector map; `vectors: None` is rejected with `Expected some vectors`.
- Jev: `opencode/jev-1.13-free` works at cost 0 (`billing_source: free`); paid `opencode/jev-1.13` returns 402 without OpenCode Models credits (verified 2026-11-18). Adapter default is the free model.
- End-to-end verified 2026-11-18 on a standalone OpenCode server: ingest on step/execution events, switch rewrite (digest system part + tail from the cut), multi-switch snapshots, `memory_search` via Code Mode, CLI `forget`.
