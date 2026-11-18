# memento-pool

Session-continuation memory for coding-agent harnesses. When you switch models mid-session, the new model gets a small frozen digest of the session, a bounded verbatim tail, and a `memory_search` tool over a filtered, segmented copy of the session, instead of the full transcript.

- `mementod`: Rust daemon (axum) storing segments in Qdrant with hybrid search (fastembed BGE-small dense + Qdrant server-side BM25, RRF, recency rerank).
- `memento`: Rust CLI for the daemon.
- `adapters/opencode`: OpenCode V2 plugin (ingest, model-switch rewrite, `memory_search`, `/memento-continue`).

Design and verified contracts: [docs/DESIGN.md](docs/DESIGN.md). Code map: [docs/MAP.md](docs/MAP.md).

## Quick start

```sh
docker compose -f deploy/compose.yaml -p memento up -d --build
curl -s http://127.0.0.1:7377/health
cargo install --path crates/memento-cli
cd adapters/opencode && bun install
```

Add the plugin to `~/.config/opencode/opencode.jsonc` (or a project `.opencode/opencode.jsonc`):

```jsonc
{
    "plugins": [
        {
            "package": "/absolute/path/to/memento-pool/adapters/opencode",
            "options": { "url": "http://127.0.0.1:7377" }
        }
    ]
}
```

## Daily use

- Keep working; every finished step is ingested.
- Switch models in the same session; the adapter serves the new model from a per-model snapshot.
- New session continuing an old one: `/memento-continue <parentSessionID>`.
- Inspect: `memento stats --harness opencode --project <id> --session <ses>` and `memento digest ...`.
- Done with a session: `memento forget --harness opencode --project <id> --session <ses> --yes` (or `--lineage` to drop a continuation chain below it).

## Jev

The adapter uses `opencode/jev-1.13` through `ctx.experimental.generate.decisions` to classify assistant notes and filter search hits. Jev is billed to OpenCode Models credits (pay-as-you-go); without credits calls fail with HTTP 402 and the adapter falls back to score ordering and unclassified notes. Disable it with the `jev: false` option.

## Development

```sh
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
MEMENTO_TEST_QDRANT_URL=http://127.0.0.1:16334 cargo test -p mementod
cd adapters/opencode && bun test && bunx tsc --noEmit
```
