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
- New session continuing an old one: `/memento-continue <harness>/<session>` (bare id = opencode).
- Hand off to the next session in this project, any harness: `/memento-handoff [harness]` in OpenCode, or `memento continue --from <harness>/<session> [--to <harness>]` from a shell. The next session started in the project claims it on its first event or `SessionStart` hook.
- Inspect: `memento stats --harness <h> --project <dir> --session <ses>`, `memento digest ...`, `memento sessions lineage --harness <h> --session <ses>`.
- Done: `memento forget --harness <h> --project <dir> --session <ses> --yes`, `--lineage <ses>` (that session and its continuations, across harnesses), or `--whole-project` (every harness).

The project key is the main checkout path (`memento project-key`), identical across harnesses.

## Other harnesses

| Harness | Writes memory | Reads memory | Setup |
| --- | --- | --- | --- |
| OpenCode | plugin (step/turn events) | digest after model switch, `memory_search`, continuation claim | plugin entry above |
| Claude Code | `Stop`/`SessionEnd` hook imports the transcript | `SessionStart` hook injects a claimed continuation | `memento install-hooks claude-code --project DIR` (or `--user`) |
| Codex | async `Stop` hook imports the rollout | `SessionStart` hook (also re-injects after compaction) | `memento install-hooks codex --project DIR` (or `--user`); trust the project (`codex --trust` or `/hooks`) |
| agy | `memory_note` MCP tool | `memory_search`, `memory_digest`, `memory_continue` MCP tools | `agy mcp add memento -- /path/to/memento-mcp --harness agy` |

Any harness can also run the CLI directly: `memento import claude-code|codex --transcript PATH` (incremental cursor), `memento search`, `memento digest`.

Known limitation: when a model restates facts from the digest, the restatement is stored again ("memory echo"); duplicates rank together and are harmless for recall but grow the store.

## Jev

The adapter uses `opencode/jev-1.13-free` (free tier, cost 0) through `ctx.experimental.generate.decisions` to classify assistant notes and filter search hits. The paid `opencode/jev-1.13` bills OpenCode Models credits and returns HTTP 402 without balance. On any Jev failure the adapter falls back to score ordering and unclassified notes, and pauses Jev for `jevCooldownMs`. Disable it with `jev: false`.

## Development

```sh
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
MEMENTO_TEST_QDRANT_URL=http://127.0.0.1:16334 cargo test -p mementod
cd adapters/opencode && bun test && bunx tsc --noEmit
```
