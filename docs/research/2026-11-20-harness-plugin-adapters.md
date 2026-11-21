# Harness Plugin Adapters Research (2026-11-20)

Research into adding native plugin adapters under `adapters/` for **Claude Code**, **Codex**, and **agy**, alongside the existing OpenCode plugin (`adapters/opencode/`).

Verified against locally installed CLIs (`claude` 2.1.328, `codex-cli` 0.170.0, `agy` 1.5.13), official documentation, and the current `memento-pool` codebase.

## Summary

- **OpenCode vs. the other harnesses:** OpenCode's plugin (`@opencode/plugin`) is an **in-process TypeScript runtime plugin** that intercepts and rewrites outgoing LLM requests (`context` hook), replaces auto-compaction (`compaction` hook), subscribes to live events, and registers native tools/commands. By contrast, **Claude Code, Codex, and agy** plugins are primarily **declarative distribution bundles** (manifest + shell lifecycle hooks + MCP server config + skills + rules/commands/agents).
- **Recommended architecture:** Ship thin, separate per-harness plugin bundles (`adapters/claude-code/`, `adapters/codex/`, `adapters/agy/`) that delegate all runtime logic to the existing `memento` and `memento-mcp` Rust binaries instead of duplicating transcript parsing or HTTP clients in JS/TS.
- **Why plugins still add real value over `memento install-hooks` + `install-skills`:**
    1. Bundles lifecycle hooks, `memento-mcp`, and `skills/` into one versioned unit with native install/enable/disable/uninstall UX, avoiding fragile JSON/TOML merging into user settings files.
    2. Automatically wires `memento-mcp` (`memory_search`, `memory_digest`, `memory_continue`, `memory_note`), directly closing the fresh-session discoverability gap noted in `docs/MAP.md` (where Claude Code and Codex searched their tool list for a memento tool, found none, and never opened the `memento-recall` skill).
- **Core limitation:** Neither Codex nor `agy` can rewrite outgoing LLM message arrays or replace auto-compaction summaries on mid-session model switches. Claude Code **Mods** (in-process JS/TS function hooks) are the only candidate worth probing if deeper Claude Code integration is desired.

## Current Baseline in `memento-pool`

| Capability | OpenCode (`adapters/opencode/`) | Claude Code / Codex / agy (`crates/memento-cli` + `memento-mcp`) |
| --- | --- | --- |
| Turn ingestion | In-process `session.execution.succeeded` / `session.step.ended` | `Stop` hook -> detached `memento import <harness>` with cursor file |
| Continuation claim & inject | First-sight claim in `context` hook | `SessionStart` hook -> `additionalContext` (Claude/Codex) or `injectSteps` (agy) |
| Subagent parent memory | In-process `context` hook (`subagentMemory`) | `SubagentStart` hook (Claude Code & Codex; unavailable in agy) |
| Model-switch context rewrite | In-process `context` hook (`[digest] + [tail]`) + `compaction` hook | Not possible via CLI hooks |
| Recall tool (`memory_search`) | Native plugin tool (+ Jev relevance filter) | `memento-mcp` exists, not wired automatically by `memento setup` |
| Slash commands | Native `/memento-continue`, `/memento-handoff` | Skills (`memento-*`) only |
| Deletion cleanup | `session.deleted` event + 24h startup reconciliation | Periodic detached `memento gc` on `SessionStart` |
| Install flow | Embedded bundle written to `~/.local/share/memento/opencode-plugin/` | Edits `.claude/settings.json`, `.codex/hooks.json`, `.agents/hooks.json`, and copies `skills/` |

## Harness Capability Matrix

| Feature | OpenCode `2.3.2` | Claude Code `2.1.328` | Codex CLI `0.170.0` | agy `1.5.13` |
| --- | --- | --- | --- | --- |
| Native plugin system | In-process JS/TS package | Declarative bundle + in-process Mods | Declarative bundle (Agent Plugins) | Declarative bundle |
| Manifest path | `package.json` | `.claude-plugin/plugin.json` | `plugin.json` (root) or `.codex-plugin/plugin.json` | `plugin.json` or `.antigravity-plugin/plugin.json` |
| Bundles lifecycle hooks | In-process API | Yes (`hooks/hooks.json`) | Needs live check (see Codex caveats) | Yes (`hooks.json` at plugin root) |
| Bundles MCP server | Native tool API | Yes (`.mcp.json` or inline `mcpServers`) | Yes (`.mcp.json` / `mcp.json`) | Yes (`mcp_config.json` at plugin root) |
| Bundles skills | Separate dir | Yes (`skills/*/SKILL.md`) | Yes (`skills/*/SKILL.md`) | Yes (`skills/*/SKILL.md`) |
| Bundles commands / rules | Native `ctx.command` | Yes (`commands/`, `agents/`) | `skills/` only | Yes (`rules/AGENTS.md`, `agents/`) |
| Plugin user options | `ctx.options` | Yes (`userConfig` + `claude plugin configure`) | `config.toml` | Prompted MCP env vars |
| In-process JS/TS runtime | Yes (Bun) | Yes (Mods via `hooks/hooks.json` `"modules"`) | No (Rust binary) | No (sync command hooks only) |
| Rewrite outgoing LLM context | Yes (`context` hook) | No in hooks; unverified in Mods | No (`PreModelRequest` is observe-only) | No (`PreInvocation` only appends steps) |
| Replace auto-compaction | Yes (`compaction` hook) | No (`PreCompact` can block; `SessionStart[compact]` re-injects) | No (`SessionStart[compact]` re-injects) | No (no compaction hook) |
| Model-switch visibility | `context` + `session.model.selected` | `PreModelSwitch` / `PostModelSwitch` | `PreModelRequest` carries `model` | `PreInvocation` carries `modelName` |
| Subagent start hook | Yes (`context` on child) | Yes (`SubagentStart`) | Yes (`SubagentStart`) | No (fires `SessionStart` without `agent_id`) |
| Session deletion signal | `session.deleted` | None (`memento gc`) | None (`SessionEnd` reason always `other`) | None (`memento gc`) |
| Plugin validator CLI | `bun test` | `claude plugin validate [--strict]` | None observed in `0.170.0` | `agy plugin validate --strict` |

## Harness Details

### 1. Claude Code (`2.1.328`)

#### Standard Declarative Plugin (recommended first step)

```text
adapters/claude-code/
├── .claude-plugin/
│   └── plugin.json        # name, version, description, userConfig
├── hooks/
│   └── hooks.json         # SessionStart, Stop, optional SubagentStart -> `memento hook claude-code`
├── .mcp.json              # stdio server: `memento-mcp --harness claude-code`
├── skills/                # memento-recall, memento-handoff, memento-cleanup, memento-connect-harness
└── commands/              # optional slash commands (/memento-continue, /memento-handoff)
```

- **Verified CLI surface:**
    - `claude --plugin-dir <path>` loads a local plugin directory or `.zip` for a single session (ideal for tests and local dev).
    - `claude plugin install <plugin[@marketplace]> --scope user|project|local --config key=value`
    - `claude plugin configure <plugin>` sets `userConfig` options declared in `plugin.json`.
    - `claude plugin validate --strict <path>` validates manifest, hooks, MCP, skills, agents, and commands.
- **Hooks:** `hooks/hooks.json` uses the same top-level `{"hooks": {...}}` schema as `.claude/settings.json`, with `${CLAUDE_PLUGIN_ROOT}` for portable paths.
- **MCP:** `.mcp.json` (or inline `mcpServers` in `plugin.json`) automatically registers `memento-mcp --harness claude-code`.

#### Claude Code Mods (in-process JS/TS function hooks)

Claude Code `2.1.287+` (verified present in local `2.1.328` via `claude plugin test`) supports **Mods**: plugins whose `hooks/hooks.json` declares `"modules": ["./register.ts"]` instead of (or alongside) shell command hooks.

```json
{
    "modules": ["./register.ts"]
}
```

```ts
export const register = (on, options) => {
    on("tool.call", { tool: "Bash" }, async ($, e, next) => {
        return next(e);
    });
};
```

- **How it works:** Runs JS/TS in-process as onion-style middleware `($, e, next)` with engine APIs `$.http`, `$.fs`, `$.process`, `$.settings`, `$.env`, `$.model`, and `$.ui` (status bands, toasts, custom panels). Handlers can observe, rewrite payloads via `next({...})`, or short-circuit/deny.
- **What it might unlock for memento:** Direct HTTP calls to `mementod` via `$.http` without spawning subprocesses, custom `/memento-continue` / `/memento-handoff` commands, status-bar memory indicators via `$.ui`, and reacting to model-switch/compaction events.
- **Why not jump straight to Mods:**
    1. Unverified whether `2.1.328` Mods can rewrite outgoing LLM `messages` arrays on model switch or replace native compaction summaries (OpenCode's core reason for needing a JS/TS plugin).
    2. Unverified whether Mods behave identically under headless `claude -p`.
    3. Tighter version coupling to Claude Code's internal runtime than declarative hooks + Rust CLI.
- **Verification note:** Claude Code generates local `.d.ts` definitions for the installed CLI version when Mods load; a quick `/tmp` probe with `claude --plugin-dir` will reveal the exact event set in `2.1.328`.

### 2. Codex CLI (`0.170.0`)

Codex implements the **Agent Plugins 1.0.0** schema (`https://agent-plugins.org/schemas/1.0.0/plugin.schema.json`) with OpenAI-specific fields under `extensions.com.openai` (and `.codex-plugin/plugin.json` as a compatibility fallback):

```text
adapters/codex/
├── plugin.json            # Agent Plugins schema + extensions.com.openai
├── .mcp.json              # stdio server: `memento-mcp --harness codex`
├── hooks/
│   └── hooks.json         # SUBJECT TO LIVE PROBE ON 0.170.0 (see warning below)
└── skills/                # memento-recall, memento-handoff, memento-cleanup, memento-connect-harness
```

- **Verified CLI surface:**
    - `codex plugin marketplace add <local-or-git>`
    - `codex plugin add <plugin>@<marketplace>`
    - `codex plugin list [--available] [--json]`
    - `codex plugin remove <plugin>`
    - Project-level marketplace discovery via `$REPO_ROOT/.agents/plugins/marketplace.json` + `[plugins."memento@local"] enabled = true` in `.codex/config.toml`.
- **No in-process runtime:** Codex is a Rust binary. `PreModelRequest` is observation-only and cannot modify the outgoing request.
- **Critical `0.170.0` caveat (`plugin_hooks`):**
    - `codex features list` on local `0.170.0` shows `plugins: stable / true`, `plugin_sharing: stable / true`, and **`plugin_hooks: removed / false`**.
    - Public Codex plugin directory submissions restrict or disallow bundled lifecycle hooks, and root `plugin.json` vs. `.codex-plugin/plugin.json` has had hook-loader conflicts.
    - Even where plugin hooks load, Codex requires explicit hook trust via `/hooks` (or `--dangerously-bypass-hook-trust` in automation).
    - **Must probe live on `0.170.0`** before assuming a Codex plugin can replace `~/.codex/hooks.json` / `.codex/hooks.json`. Fallback: Codex plugin ships skills + `.mcp.json`, while `memento install-hooks codex` continues managing `hooks.json`.

### 3. Antigravity / `agy` (`1.5.13`)

Antigravity has a clean declarative plugin model shared across `agy` CLI, Antigravity App, Antigravity IDE, and IDE extensions:

```text
adapters/agy/
├── plugin.json            # $schema: https://antigravity.google/schemas/v1/plugin.json
├── hooks.json             # MUST be at plugin root (not hooks/hooks.json)
├── mcp_config.json        # stdio server: `memento-mcp --harness agy`
├── skills/                # memento-recall, memento-handoff, memento-cleanup, memento-connect-harness
└── rules/                 # optional concise AGENTS.md pointer if needed
```

- **Verified CLI surface & discovery:**
    - `agy plugin validate --strict <dir>` validates manifest, hooks, MCP, skills, and rules.
    - `agy plugin install <dir>` / `uninstall` / `enable` / `disable` / `list`.
    - Auto-discovery in `.agents/plugins/<name>/` (project) and `~/.gemini/config/plugins/<name>/` (global), or explicit registration via `.agents/plugins.json` / `~/.gemini/config/plugins.json`.
    - Supports `.antigravity-plugin/plugin.json` as an alternative manifest path in multi-provider repos, though component files (`hooks.json`, `mcp_config.json`, `skills/`, `rules/`) still live at the plugin root.
- **Hooks inside plugin:**
    - Must be placed directly at `<plugin-root>/hooks.json` (native plugins do **not** read `hooks/hooks.json`).
    - Uses named top-level hook keys (`{"memento": {"SessionStart": [...], "Stop": [...]}}`), camelCase stdin JSON, and `injectSteps` stdout JSON.
    - Receives `PLUGIN_ROOT` and `PLUGIN_DATA` env vars; hook process `cwd` is the plugin directory while `stdin.cwd` is the user's workspace (already handled by `memento hook agy`).
    - Synchronous command hooks only; `PreInvocation` receives `modelName` but can only append `injectSteps` or terminate.
- **MCP inside plugin:** `mcp_config.json` at plugin root; tools are namespaced `<plugin-name>_<server-name>`.
- **Sidecars (`sidecars/*/sidecar.json`):** Antigravity App only, not available in standalone `agy` CLI — do not rely on them for CLI ingestion.
- **Importing Claude plugins:** `agy plugin import claude` exists, but native `agy` hook schemas and `--harness agy` differ from Claude Code, so a dedicated `adapters/agy/` package is cleaner.

## Key Architectural Decisions & Gotchas

### 1. Separate Per-Harness Adapter Directories (Not One Shared Plugin Folder)

Keep separate packages: `adapters/claude-code/`, `adapters/codex/`, `adapters/agy/`, plus existing `adapters/opencode/`.

- Putting Claude Code and Codex in one directory collides on both `hooks/hooks.json` (different JSON schema and different `memento hook <harness>` command) and `.mcp.json` (different `--harness` flag).
- Strict validators (`agy plugin validate --strict`, `claude plugin validate --strict`) complain about foreign manifests/files.
- Keep `skills/memento-*` at the repository root as the single source of truth and copy/embed them into each plugin at build or install time.

### 2. `memento-mcp` Startup `cwd` Bug-in-Waiting for Global Plugins

In `crates/memento-mcp/src/main.rs`, `Server::new` resolves `project = project_key(&std::env::current_dir())` **once at startup** and defaults `session` to `<harness>-notes`.

- When a plugin is installed **globally** (e.g., `~/.gemini/config/plugins/memento`), a harness may spawn the stdio MCP server with `cwd` set to the plugin folder or `$HOME`, or share one server across workspaces — causing `memory_search` and `memory_digest` to target the wrong project key.
- Before shipping bundled MCP in plugins:
    1. Test what `cwd` and path variables (`${workspaceFolder}`, `${CLAUDE_PROJECT_DIR}`, etc.) each harness passes to plugin-bundled stdio MCP servers.
    2. Consider adding an optional `project` (or `cwd`) parameter to `memento-mcp` tools and/or supporting MCP `roots/list` so project resolution stays accurate even from global plugins.

### 3. Configuring `subagent-memory` and Daemon URL/Token in Static Plugin Hooks

Currently `memento install-hooks` bakes `--subagent-memory digest` into each harness's settings JSON. With static plugin `hooks.json` files:

- Let `memento hook <harness>` read per-harness defaults (e.g. `subagent_memory`, daemon `url`) from `${XDG_CONFIG_HOME:-~/.config}/memento/config.json` (or existing `install.json`) when CLI flags and env vars (`MEMENTO_SUBAGENT_MEMORY`, `MEMENTO_URL`, `MEMENTO_TOKEN`) are unset.
- For Claude Code specifically, `plugin.json` `userConfig` can also expose these options through `claude plugin configure`.

### 4. Distribution: `memento setup` vs. Native Plugin Installers

- `memento` already embeds the OpenCode bundle (`crates/memento-cli/build.rs` + `assets.rs`) and writes it under `${XDG_DATA_HOME:-~/.local/share}/memento/`.
- The same pattern can embed the small declarative files for `claude-code`, `codex`, and `agy` so `memento setup` can install them cleanly (resolving the installed `memento` / `memento-mcp` binary paths if not on `PATH`), while keeping `adapters/<harness>/` directly usable with `claude --plugin-dir`, `agy plugin install`, and `codex plugin marketplace add`.

## Pre-Implementation Probe Checklist

Run these short read-only/sandbox probes in `/tmp` before writing implementation code:

1. **Codex `0.170.0` plugin hooks probe:** Create a minimal local plugin + marketplace in `/tmp` with `hooks/hooks.json` (testing both root `plugin.json` + `extensions.com.openai.hooks` and `.codex-plugin/plugin.json`) and verify whether `codex exec` fires `SessionStart` / `Stop` despite `plugin_hooks: removed / false`.
2. **MCP `cwd` & `roots/list` probe (Claude Code, Codex, agy):** Spawn a tiny logging stdio MCP server from a user-scoped plugin in each harness and record `process.cwd()`, env vars, and whether the client exposes MCP `roots/list`.
3. **Claude Code `2.1.328` Mod probe (optional, only if targeting OpenCode-like mid-session model switching in Claude Code):** Load a minimal `"modules": ["./register.ts"]` plugin via `claude --plugin-dir`, inspect the generated `.d.ts` event list, and test whether it runs in `claude -p` and whether any event can rewrite outgoing `messages` or replace compaction.
4. **Validation pass:** Verify target manifests pass `claude plugin validate --strict` and `agy plugin validate --strict`.

## Probe Results and Decisions (2026-11-21)

Ran the checklist above with Claude Code 2.1.329, Codex 0.170.0, and agy 1.5.13.

| Probe | Claude Code | Codex | agy |
| --- | --- | --- | --- |
| Plugin MCP server cwd | project | plugin cache folder | plugin folder |
| MCP roots | yes, `file://<project>` | no | yes, `file://<workspace>` |
| Other project signal | `${CLAUDE_PROJECT_DIR}` substituted in MCP args | `_meta["x-codex-turn-metadata"].workspaces` on every `tools/call` | hook stdin `cwd` / `workspacePaths` |
| Plugin hooks fire headless | yes | no (all manifest variants, trust bypassed) | yes |
| `userConfig` in hooks | exec-form `args` only | n/a | n/a |

Decisions:

1. `memento-mcp` resolves the project per call (commit `33bcd83d`): tool `project` argument, `--project`/`MEMENTO_PROJECT_DIR`, client roots, Codex workspace metadata, cwd.
2. Integration lives in the adapters, installed with each harness's plugin manager; `memento setup`/`install-hooks` are not extended for plugins. No per-user config file is needed: Claude Code exposes subagent memory through `userConfig`, agy has no subagent hook, and Codex hooks stay outside the plugin.
3. Codex adapter ships MCP and skills only; `memento install-hooks codex` stays required until Codex runs plugin hooks.
4. Claude Code Mods are not adopted: `turn.step` exposes `turnId`, `index`, `model`, `effort`, `messageCount` only and accepts only `model`/`effort` changes; `session.compact` can only skip.
5. Live end-to-end recall passed for all three plugins (see `docs/MAP.md`).
