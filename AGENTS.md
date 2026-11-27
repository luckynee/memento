# memento

Session-continuation memory for coding-agent harnesses (OpenCode, Claude Code, Codex, agy). A Rust daemon (`mementod`) stores filtered session segments in Qdrant with hybrid search; the `memento` CLI, `memento-mcp`, and per-harness adapters ingest sessions and inject digests.

## Read first

- `docs/MAP.md`: code map and verified runtime facts. Read before exploring; update it with verified, durable findings.
- `docs/DESIGN.md`: architecture, HTTP API, segmentation, digest and adapter contracts. Read before changing behavior.
- `docs/BENCHMARKS.md`: measured results and their caveats. Do not quote numbers that are not recorded there or in the map.

## Layout

- `crates/memento-core`: wire protocol, segmentation, lineage, rerank, digest rendering (shared by everything).
- `crates/mementod`: HTTP daemon, Qdrant store, embedder.
- `crates/memento-cli`: `memento` CLI (setup, hooks, importers, daemon management, update, doctor).
- `crates/memento-mcp`: stdio MCP server exposing memory tools.
- `adapters/opencode`: OpenCode V2 plugin (Bun, TypeScript).
- `adapters/claude-code`, `adapters/codex`, `adapters/agy`: native plugin bundles.
- `skills/`: canonical skills; adapter copies are generated.
- `deploy/`: Docker image and compose files. `install.sh`: release installer.

## Checks

Run the relevant checks before committing:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cd adapters/opencode && bun test && bun run typecheck
scripts/sync-adapter-skills.sh --check
```

Live Qdrant tests are gated on `MEMENTO_TEST_QDRANT_URL`. Run them against a throwaway container only:

```sh
docker run -d --rm --name memento-test-qdrant -p 16334:6334 qdrant/qdrant:v1.20.2
MEMENTO_TEST_QDRANT_URL=http://127.0.0.1:16334 cargo test -p mementod
docker stop memento-test-qdrant
```

## Conventions

- Four-space indentation, no tabs. No code comments; use descriptive names and put explanations in commit messages.
- Conventional Commits (`feat:`, `fix:`, `perf:`, `chore:`, `docs:`, `test:`), one logical change per commit.
- Edit skills in `skills/`, then run `scripts/sync-adapter-skills.sh` to update the adapter copies.
- Changes to `memento-core` protocol types affect the daemon, CLI, MCP server, and every adapter; keep them backward compatible.
- Digest output must stay byte-stable for identical stored data (prompt caching depends on it).
- Never change `POINT_NAMESPACE` in `crates/memento-core/src/ids.rs`; it changes every stored point id.
- Never commit secrets, model caches, Qdrant storage, or `.opencode/` (local, per-developer OpenCode settings).

## Safety when testing

- Never point tests, benchmarks, or experiments at a developer's managed daemon (`127.0.0.1:7377`, containers `memento-mementod-1` and `memento-qdrant-1`) or its data.
- Digest and session-scoped reads can update session activity, so benchmark them only on copies or throwaway daemons.
- For setup or harness tests, use scratch `XDG_CONFIG_HOME`/`XDG_STATE_HOME` and non-default ports. For private OpenCode servers, isolate `XDG_CONFIG_HOME` only so stored provider credentials still work; never read or print credentials.

## Releasing

Bump `[workspace.package] version` in `Cargo.toml` (then `cargo update -w`) and the `version` in `adapters/agy/plugin.json`, `adapters/codex/plugin.json`, and `adapters/claude-code/.claude-plugin/plugin.json`. Commit as `chore: release vX.Y.Z`, tag `vX.Y.Z`, push `main` and the tag; `.github/workflows/release.yml` publishes tarballs, `install.sh`, and the GHCR image.
