# memento-pool

Long coding sessions can replay a huge transcript when you switch models, while starting a new session can leave useful context behind. Provider prompt caches are per model, so switching can also mean an expensive uncached prefill.

memento-pool gives coding agents a compact working-state digest, a bounded recent verbatim tail, and `memory_search` over a filtered copy of the session. It works across OpenCode, Claude Code, Codex, and agy.

https://github.com/user-attachments/assets/7d5a1290-f95e-4998-a06a-b6ffac6d8676

## How it works

1. A harness adapter ingests session activity and filters it into searchable segments.
2. `mementod` stores segments in Qdrant. Hybrid search combines fastembed BGE-small dense vectors and Qdrant BM25 using RRF, then reranks for recency.
3. The adapter supplies a digest and recent context when continuing work; the agent can search memory for details it needs.

The main components are:

- `mementod`: the Rust daemon and Qdrant-backed memory service.
- `memento`: the Rust CLI for setup, maintenance, and memory operations.
- `memento-mcp`: an optional MCP server exposing memory tools.
- Adapters: the OpenCode V2 plugin and native Claude Code, Codex, and agy integrations.

## Requirements

- Docker with Compose for the managed daemon.
- A published GitHub release for the installer.
- A supported CLI target: x86_64 or aarch64 Linux (musl), or x86_64 or aarch64 macOS.

## Install

```sh
curl -fsSL https://github.com/luckynee/memento/releases/latest/download/install.sh | sh
```

The installer downloads `memento` and `memento-mcp`, verifies their archive's SHA-256 checksum, and runs `memento setup` when a terminal is available.

Set `MEMENTO_VERSION` to select a release (for example `0.2.0` or `v0.2.0`), `MEMENTO_INSTALL_DIR` to change the binary directory (default `~/.local/bin`), or `MEMENTO_NO_SETUP=1` to skip setup.

## Setup

Run `memento setup` to detect harnesses, install embedded skills and the OpenCode plugin, and optionally start the managed daemon. The wizard defaults to **This project**; choose **User-wide** to configure your user instead. It previews changes and asks for confirmation.

For non-interactive setup, specify `--yes`, `--harness`, and `--scope`; add `--start-daemon` to request starting the managed daemon:

```sh
memento setup --yes --harness opencode --scope project --start-daemon
```

For OpenCode, setup also accepts:

- `--resume on|off`: enable or disable resume compaction. By default, it triggers after more than 60 minutes idle when context is at least approximately 100,000 tokens, keeping the first post-idle request smaller.
- `--compaction memento|native`: choose memento's digest-based same-model compaction to avoid a model summarisation request, or use native compaction.

`memento doctor` reports the installed per-harness settings.

## Everyday use

- Keep working: configured adapters ingest session activity automatically.
- Switch models in an OpenCode session: the adapter supplies a per-model snapshot instead of replaying the full transcript.
- Continue an old session in OpenCode with `/memento-continue <harness>/<session>`; a bare session ID means OpenCode.
- Hand off to the next session with `/memento-handoff [harness]` in OpenCode, or `memento continue --from <harness>/<session> [--to <harness>]` in a shell. The next session in the project claims it on its first event or `SessionStart` hook.
- Inspect memory from the project directory with `memento search "topic"`, `memento stats`, `memento digest`, or `memento sessions lineage --harness <h> --session <ses>`.

The project key is the main checkout path, shared across harnesses and linked worktrees. Run `memento project-key` to see it.

## Harness support

| Harness | Writes memory | Reads memory | Setup |
| --- | --- | --- | --- |
| OpenCode | Plugin step/turn events | Switch digest, `memory_search`, continuation claim | `memento setup`, or add the plugin entry to `opencode.jsonc` ([snippet](adapters/opencode/README.md)) |
| Claude Code | `Stop` hook starts detached transcript import | `SessionStart` injects a claimed continuation | `memento install-hooks claude-code --project DIR` (or `--user`) |
| Codex | `Stop` hook starts detached rollout import | `SessionStart` injects a claim and re-injects after compaction | `memento install-hooks codex --project DIR` (or `--user`); trust the project with `codex --trust` or `/hooks` |
| agy | `Stop` hook imports the transcript | `SessionStart` injects a claimed continuation | `memento install-hooks agy --project DIR` (or `--user`) |
| Generic | `turn_end` imports a supported transcript or neutral items | `session_start` returns claimed context for injection | Wire JSON events to `memento hook generic --harness NAME`; see the [contract](docs/DESIGN.md#generic-hook-contract) |

Native plugins provide memory tools such as `memory_search` and `memory_note`: [Claude Code](adapters/claude-code/README.md) uses hooks and native Mod tools without MCP; [agy](adapters/agy/README.md) uses hooks and MCP; [Codex](adapters/codex/README.md) uses MCP. Use either the native plugin hooks or `install-hooks` for a harness, not both. Codex 0.170 does not run plugin hooks, so its plugin still needs `memento install-hooks codex`.

OpenCode and Codex give subagents parent memory by default. Claude Code is opt-in with `memento install-hooks claude-code --subagent-memory digest`; disable Codex's default with `--subagent-memory off` or set OpenCode's `subagentMemory` to `"off"`. Both OpenCode and Codex also support `"filtered"`; agy does not support subagent memory.

For manual setup, use `memento install-hooks <harness>` and `memento install-skills`, then register the OpenCode plugin at `${XDG_DATA_HOME:-~/.local/share}/memento/opencode-plugin/` in `opencode.jsonc`. Other tooling can use `memento import claude-code|codex|agy --transcript PATH` (incremental cursor), `memento ingest`, `memento search`, `memento digest`, the generic hook, MCP, or the [daemon HTTP API](docs/DESIGN.md#daemon-http-api-mementod-json). MCP is optional for agy and other MCP-capable harnesses:

```sh
agy mcp add memento -- /path/to/memento-mcp --harness agy
```

## Maintenance

```sh
memento update
memento doctor
memento daemon up|down|status|logs [-f]|pull
```

- `memento update --check` checks for an update; `--version <x.y.z>` selects a release; `--refresh` reapplies saved setup without replacing binaries. Refresh also pulls and restarts a managed daemon.
- Binary replacement is for installer-managed installs. For Cargo installs, use `cargo install --path crates/memento-cli` or the installer.
- `memento doctor --offline` skips the latest-release check.
- `memento daemon` manages the daemon through Docker Compose; `logs -f` follows its logs.

## Removing memento

`memento remove` previews removal and lets you choose harness integrations from a checklist, or select them with `--harness`. Use `--dry-run` to preview without changes. Normal removal keeps memory data, binaries, and shared assets that other integrations may need.

`memento remove --purge` additionally destroys the managed daemon's Qdrant data, Compose project and files, and embedded plugin. It requires a saved managed daemon and typing `purge` in a terminal; non-interactive purge requires `--yes`.

## Cleaning up memory

Preview deleted-session cleanup with `memento gc --project DIR --dry-run`, then run `memento gc --project DIR`. Hooks also start gc at most daily per project; `MEMENTO_AUTO_GC=0` disables that automatic gc. The daemon separately sweeps stale sessions by default. Deleted sessions can remain available to dependent continuations until collected.

Explicit forgetting is destructive. Use `--dry-run` to see the segment count and affected sessions first. The CLI asks for confirmation unless `--yes` is supplied:

- `memento forget --harness H --project PROJECT --session S`: forget one session.
- Replace `--session S` with `--lineage S` to forget S and its descendants.
- Replace it with `--whole-project` to forget all harnesses in the project.

## Skills

Preview and install the embedded skills with:

```sh
memento install-skills --harness all --project DIR --dry-run
memento install-skills --harness all --project DIR
```

Use `--user` instead of `--project DIR` for user-wide installation, or `--source DIR` to override the embedded skills. The installer skips foreign skills.

| Harness | Project location | User location |
| --- | --- | --- |
| OpenCode | `.opencode/skills` | `~/.config/opencode/skills` |
| Claude Code | `.claude/skills` | `~/.claude/skills` |
| Codex | `.agents/skills` | `~/.agents/skills` |
| agy | `.agents/skills` | `~/.gemini/config/skills` |

## Jev

The OpenCode adapter uses the free `opencode/jev-1.13-free` through `ctx.experimental.generate.decisions` to classify assistant notes and filter search hits. The paid `opencode/jev-1.13` uses OpenCode Models credits and returns HTTP 402 without balance. On failure, the adapter falls back to unclassified notes and score ordering, and pauses Jev for `jevCooldownMs`. Set `jev: false` to disable it.

## Development

Build from a checkout:

```sh
docker compose -f deploy/compose.yaml -p memento up -d --build
cargo install --path crates/memento-cli
cd adapters/opencode && bun install
```

Run `memento setup` from the checkout. Override embedded development assets with `--opencode-plugin /path/to/memento-pool/adapters/opencode` and `--skills-source /path/to/memento-pool/skills`, or with `MEMENTO_OPENCODE_PLUGIN` and `MEMENTO_SKILLS_DIR`. Setup preserves supported JSONC comments and trailing commas; if it cannot safely parse a config, it leaves the file unchanged and prints a plugin snippet for manual insertion.

Checks:

```sh
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
MEMENTO_TEST_QDRANT_URL=http://127.0.0.1:16334 cargo test -p mementod
cd adapters/opencode && bun test && bunx tsc --noEmit
```

### Releasing

1. Bump `[workspace.package] version` in the root `Cargo.toml` and run `cargo update -w`.
2. Bump `version` in `adapters/agy/plugin.json`, `adapters/codex/plugin.json`, and `adapters/claude-code/.claude-plugin/plugin.json`.
3. Commit as `chore: release vX.Y.Z`, tag `vX.Y.Z`, and push main and the tag.

`.github/workflows/release.yml` publishes release tarballs, `install.sh`, and the multi-architecture GHCR daemon image.

## Documentation

- [Design and verified contracts](docs/DESIGN.md)
- [Code map](docs/MAP.md)
- Adapter guides: [OpenCode](adapters/opencode/README.md), [Claude Code](adapters/claude-code/README.md), [Codex](adapters/codex/README.md), [agy](adapters/agy/README.md)
- [Deployment guide](deploy/README.md)

## License

MIT, see [LICENSE](LICENSE).
