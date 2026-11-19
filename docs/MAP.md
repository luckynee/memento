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
| `crates/mementod/src/store.rs` | Qdrant schema, segment upsert (dense + bm25 Document), hybrid RRF candidates, session registry, continuation slots per `(project, target_harness)` (`list_continuations` keeps legacy project-only ids) |
| `crates/mementod/src/embed.rs` | `Embedder` trait, `FastEmbedder` (spawn_blocking), `FakeEmbedder` for tests |
| `crates/mementod/tests/` | Live Qdrant tests, gated on `MEMENTO_TEST_QDRANT_URL` |
| `crates/memento-cli/src/main.rs` | `memento` CLI, HTTP client of mementod |
| `adapters/opencode/src/` | OpenCode V2 plugin: `client`, `transcript`, `ingest`, `session-ownership`, `switch` (pure rewrite), `switch-controller`, `tools`, `commands`, `options`, `index` |
| `adapters/opencode/test/` | `bun test` suites (mocked daemon and Jev) |
| `deploy/` | `Dockerfile` (rust:1.95-trixie builder, debian:trixie-slim + libc++1 runtime, model baked by `mementod warmup`), `compose.yaml` |

## Adapter storage keys

- `memento/ingest/<sessionID>`: ingest cursor
- `memento/registered/<sessionID>`: session registered with the daemon
- `memento/switch/<sessionID>`: `{ origin, lastModel, snapshots }`

## Cross-harness pieces

| Path | Purpose |
| --- | --- |
| `crates/memento-cli/src/cross_harness.rs` | Claude Code / Codex JSONL importers (incl. Codex code-mode `exec`), cursor file, `hook`, `continue`, `project-key`, `install-hooks` |
| `crates/memento-cli/tests/fixtures/` | Synthetic Claude and Codex transcripts |
| `crates/memento-mcp/src/main.rs` | stdio MCP server (`rmcp` 3.11): `memory_search`, `memory_digest`, `memory_continue`, `memory_note` |
| CLI state | `${XDG_STATE_HOME:-~/.local/state}/memento/cursors.json` (+ `cursors.lock`) |
| Session cleanup | `POST /v1/sessions/release` and `memento gc` collect deleted sessions while preserving dependent lineage |

## Verified runtime facts

- OpenCode v2.3.2 `context` hook: user and assistant messages carry `id` (`msg_...`); tool-result messages have no `id`. Verified with a probe plugin on a standalone server, 2026-11-18.
- `@opencode/plugin` 2.2.5 decision `predicate` questions take `{ type, name, instructions }` only (no `criteria`).
- Qdrant v1.20.2 self-hosted: vectorless collections accept payload-only points; sparse `bm25` with `modifier: idf` and server-side `qdrant/bm25` documents works for upsert and query. Image has bash, no curl/wget.
- Qdrant gRPC (qdrant-client 1.20.0): payload-only points need an explicitly empty named-vector map; `vectors: None` is rejected with `Expected some vectors`.
- Jev: `opencode/jev-1.13-free` works at cost 0 (`billing_source: free`); paid `opencode/jev-1.13` returns 402 without OpenCode Models credits (verified 2026-11-18). Adapter default is the free model.
- End-to-end verified 2026-11-18 on a standalone OpenCode server: ingest on step/execution events, switch rewrite (digest system part + tail from the cut), multi-switch snapshots, `memory_search` via Code Mode, CLI `forget`.
- Cross-harness end-to-end verified 2026-11-19 in a throwaway repo: OpenCode S1 -> (`memento continue`) Claude Code 2.1.319 `-p` (SessionStart claim + Stop import) -> Codex 0.169.0 `exec` (SessionStart claim + async Stop import) -> agy 1.5.11 via `memento-mcp` (search + note) -> new OpenCode session (first-sight claim + project-scope search with harness attribution). Lineage 4 hops across 3 harnesses; project-scope forget removed all harnesses' data.
- Codex 0.169 runs tools in code mode: every call is `custom_tool_call` `exec` with a JavaScript `input` calling `tools.exec_command`, `tools.apply_patch`, `tools.web__run`, MCP tools.
- Codex ignores project `.codex/hooks.json` until the project is trusted; `-c projects."<dir>".trust_level=...` does not count; `codex exec --trust` writes `[projects."<dir>"] trust_level = "trusted"` to `~/.codex/config.toml`. Scripted runs also need `--dangerously-bypass-hook-trust`.
- Claude Code `-p` loads project `.claude/settings.json` hooks without extra trust flags.
- agy `-p` takes the next token as its prompt: put flags before `-p "prompt"`.
- Big-context test 2026-11-19 (OpenCode, commandcode/gpt-6-luna to commandcode/claude-haiku-5-5 at 285,576 tokens): first switched request 22,780 tokens vs 287,355 for a vanilla switch, 4/4 recall. OpenCode v2.3.2 leaves `system` role messages in history after a switch; Anthropic-protocol providers reject them (HTTP 400), so the adapter hoists them into system parts when rewriting. `9router/cx/gpt-6-luna` auto-compacts near 200k. Recording: `/home/luckynee/video/memento-big-context-demo.*`.
- Re-verified 2026-11-19 after digest fixes, on real big transcripts: the Codex -> Claude Code continuation now answers the `check.py` error code from the digest's `Recent commands` (previously missed); a forked 285k OpenCode session switched to Haiku sends about 20-24k tokens (fork origin fix).
- Codex 0.169 code-mode `exec` outputs are `[{"type":"input_text","text":"Script completed\nWall time ...\nOutput:\n"}, {...command output...}]` with no exit code; no `Script failed` variant observed in local rollouts.
- Interactive TUIs: Codex asks to trust new project hooks once (`Hooks need review`); Claude Code and agy ask to trust the folder once. agy's start banner and Codex `/status` show account emails, so recordings must redact or avoid them.
- Demo recording on this machine (i3 + picom, X11): OBS Xcomposite capture of a kitty window records black; `ffmpeg -f x11grab -window_id <wid>` works. Demo kitty is floated and made sticky via `i3-msg` so it stays mapped.
- OpenCode switching a 285k session to a 200k-window model (`claude-agent/claude-haiku-4-5`) auto-compacts before the context hook (verified 2026-11-19: 182,179-token summarisation call). With the adapter's compaction hook the compaction is a memento digest at zero model cost; first requests about 19-20k, 4/4 recall.
- Fixed 2026-11-19 (`4dd9a532`, verified live): the shared OpenCode service delivers every session's events to each loaded plugin instance; the adapter now ignores sessions whose `projectID` differs from `ctx.location.project.id`.
- `claude-agent` provider (Claude Agent SDK) drops extra system parts added in the `context` hook but keeps the message tail; the switch digest therefore rides in the first tail user message (`f9ee8124`; Haiku 5.5 recall 11/17 -> 17/17).
- Headless `claude -p` (2.1.328) and `codex exec` (0.170.0) drop `"async": true` Stop hooks (0 segments imported); interactive sessions complete them. Stop now spawns a detached importer with a pending marker that `memento continue` waits on (`be88d6bf`).
- OpenCode forks copy messages with new ids but keep the original `time.created`; `session.get` exposes `fork.sessionID`. The adapter links forks to the parent and ingests only post-fork messages (`22378916`).
- `chunk_text` used to emit one-character sliding duplicates after a paragraph shorter than the overlap (131 segments for one reply); fixed in `19eb2935`.
- Messy-data proof 2026-11-19 in `/tmp/memento-messy` (reversed decisions, corrected near-duplicate values, 5 file edits, chain OpenCode -> Claude Code -p -> Codex exec -> agy MCP -> new OpenCode on claude-agent Haiku): latest values recalled across all hops; one of three final-hop runs answered an older quoted request instead of the current one.
- Demo tooling 2026-11-19: `opencode api POST /api/session/<id>/fork -d '{}'` clones a big session per take; `POST /api/session` takes `agent`, `model {id, providerID, variant}`, `location {directory}`. OpenCode TUI `Home`/`PageDown`/`End` scroll the transcript; TUI tabs are held by the background service (editing `tui/tabs.json` does not stick). The 2026-11-19 preview is `/home/luckynee/video/memento-pool-preview.mp4`.
- `adapters/opencode/src/jev.ts` `stripScaffolding` mirrors the Rust `strip_scaffolding` default tag set in `crates/memento-core/src/segment.rs`; keep both in sync.
