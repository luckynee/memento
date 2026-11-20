# memento-pool

Session-continuation memory for coding-agent harnesses. When you switch models mid-session, the new model gets a small frozen digest of the session, a bounded verbatim tail, and a `memory_search` tool over a filtered, segmented copy of the session, instead of the full transcript.

- `mementod`: Rust daemon (axum) storing segments in Qdrant with hybrid search (fastembed BGE-small dense + Qdrant server-side BM25, RRF, recency rerank).
- `memento`: Rust CLI for the daemon.
- `adapters/opencode`: OpenCode V2 plugin (ingest, model-switch rewrite, `memory_search`, `/memento-continue`).

Design and verified contracts: [docs/DESIGN.md](docs/DESIGN.md). Code map: [docs/MAP.md](docs/MAP.md).

## Quick start

```sh
curl -fsSL https://github.com/luckynee/memento/releases/latest/download/install.sh | sh
```

The installer downloads `memento` and `memento-mcp`, verifies the SHA-256 checksum, and runs `memento setup` when a terminal is available. Setup guides harness configuration and can start the managed daemon. Docker with Compose is required for the daemon. The installer requires a published GitHub release.

Override install settings with `MEMENTO_VERSION` (for example `0.2.0` or `v0.2.0`), `MEMENTO_INSTALL_DIR` (default `~/.local/bin`), or `MEMENTO_NO_SETUP=1`.

### Install and update

Run `memento setup` to configure detected harnesses, install embedded skills and the embedded OpenCode plugin, and optionally start the managed daemon. The wizard previews changes before confirmation. `memento setup --start-daemon` starts the managed daemon without prompting; for non-interactive setup, also specify `--yes --harness ... --scope ...`.

Alternatively, configure harnesses manually with `memento install-hooks <harness>` and `memento install-skills`, then add the installed OpenCode plugin at `${XDG_DATA_HOME:-~/.local/share}/memento/opencode-plugin/` to `opencode.jsonc` (see [Other harnesses](#other-harnesses) and [adapters/opencode/README.md](adapters/opencode/README.md)). Your own tooling can use the generic hook, `memento import`/`memento ingest`, MCP, or the daemon HTTP API ([docs/DESIGN.md](docs/DESIGN.md)).

```sh
memento update
memento doctor
memento daemon up|down|status|logs [-f]|pull
```

`memento update --check` checks for an available update, `--version <x.y.z>` selects a release, and `--refresh` reapplies saved setup without replacing binaries. Updates replace binaries only for installer-managed installs; Cargo installs should use `cargo install --path crates/memento-cli` or the installer. A managed daemon is pulled and restarted during refresh. `memento doctor --offline` skips the latest-release check.

### From source

```sh
docker compose -f deploy/compose.yaml -p memento up -d --build
cargo install --path crates/memento-cli
cd adapters/opencode && bun install
```

Run `memento setup` from the checkout. Development checkouts can pass `--opencode-plugin /path/to/memento-pool/adapters/opencode` and `--skills-source /path/to/memento-pool/skills`; the equivalent environment overrides are `MEMENTO_OPENCODE_PLUGIN` and `MEMENTO_SKILLS_DIR`. JSONC files containing comments or trailing commas are left unchanged; setup prints the plugin snippet for manual insertion.

## Daily use

- Keep working; every finished step is ingested.
- Switch models in the same session; the adapter serves the new model from a per-model snapshot.
- New session continuing an old one: `/memento-continue <harness>/<session>` (bare id = opencode).
- Hand off to the next session in this project, any harness: `/memento-handoff [harness]` in OpenCode, or `memento continue --from <harness>/<session> [--to <harness>]` from a shell. The next session started in the project claims it on its first event or `SessionStart` hook.
- Inspect from the project directory: `memento search "topic"`, `memento stats`, `memento digest`, `memento sessions lineage --harness <h> --session <ses>`.
- Done: see [Cleanup](#cleanup) before forgetting or collecting memory.

The project key is the main checkout path (`memento project-key`), identical across harnesses.

## Other harnesses

| Harness | Writes memory | Reads memory | Setup |
| --- | --- | --- | --- |
| OpenCode | plugin (step/turn events) | digest after model switch, `memory_search`, continuation claim | `memento setup`, or add the plugin entry to `opencode.jsonc` ([snippet](adapters/opencode/README.md)) |
| Claude Code | `Stop` hook starts a detached transcript import | `SessionStart` hook injects a claimed continuation | `memento install-hooks claude-code --project DIR` (or `--user`) |
| Codex | `Stop` hook starts a detached rollout import | `SessionStart` hook (also re-injects after compaction) | `memento install-hooks codex --project DIR` (or `--user`); trust the project (`codex --trust` or `/hooks`) |
| agy | `Stop` hook imports the transcript | `SessionStart` hook injects a claimed continuation | `memento install-hooks agy --project DIR` (or `--user`) |
| Generic | `turn_end` imports a supported transcript or neutral items | `session_start` returns claimed context for the harness to inject | Wire JSON lifecycle events to `memento hook generic --harness NAME`; see [contract](docs/DESIGN.md#generic-hook-contract) |

Any harness can use the CLI directly: `memento import claude-code|codex|agy --transcript PATH` (incremental cursor), `memento search`, `memento digest`. MCP is optional for agy and other MCP-capable harnesses; for agy, configure `agy mcp add memento -- /path/to/memento-mcp --harness agy`.

OpenCode and Codex provide parent memory to subagents by default; Claude Code is opt-in with `memento install-hooks claude-code --subagent-memory digest`. Use `--subagent-memory off` when installing Codex hooks to disable it, or set the OpenCode `subagentMemory` option to `"off"`. Both also support `"filtered"`; agy does not support subagent memory.

### Cleanup

Preview deleted-session cleanup with `memento gc --project DIR --dry-run`, then run `memento gc --project DIR` when appropriate. Hooks also start gc at most daily per project; `MEMENTO_AUTO_GC=0` disables this. The daemon sweeps stale sessions by default. Deleted sessions can remain available to dependent continuations until collected.

Explicit forgetting is destructive. Preview it with `--dry-run` (reports the segment count and affected sessions without deleting), then confirm the scope before `memento forget --harness H --project PROJECT --session S`, `--lineage S` (S and its descendants), or `--whole-project` (all harnesses). The CLI asks for confirmation unless `--yes` is supplied.

### Skills

Preview or install the embedded skills; `--source` is optional:

```sh
memento install-skills --harness all --project DIR --dry-run
memento install-skills --harness all --project DIR
```

Use `--user` instead of `--project DIR` for user-wide installation. The installer skips foreign skills. Project locations are `.opencode/skills`, `.claude/skills`, and `.agents/skills` (shared by Codex and agy); user locations are `~/.config/opencode/skills`, `~/.claude/skills`, `~/.agents/skills`, and `~/.gemini/config/skills`, respectively.

## Jev

The adapter uses `opencode/jev-1.13-free` (free tier, cost 0) through `ctx.experimental.generate.decisions` to classify assistant notes and filter search hits. The paid `opencode/jev-1.13` bills OpenCode Models credits and returns HTTP 402 without balance. On any Jev failure the adapter falls back to score ordering and unclassified notes, and pauses Jev for `jevCooldownMs`. Disable it with `jev: false`.

## Development

```sh
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
MEMENTO_TEST_QDRANT_URL=http://127.0.0.1:16334 cargo test -p mementod
cd adapters/opencode && bun test && bunx tsc --noEmit
```

### Releasing

Bump the workspace version in the root `Cargo.toml`, tag it `vX.Y.Z`, and push the tag to trigger the release workflow.
