# OpenCode adapter

This plugin stores session messages in memento and recalls them when continuing work.
The recommended installation is `memento setup`, which installs and configures the
embedded plugin bundle without requiring Bun:

```sh
memento setup
```

For development checkouts, run `bun install` in this directory and use
`memento setup --opencode-plugin /path/to/memento/adapters/opencode`.
Alternatively, add the checkout plugin to `opencode.jsonc` manually:

```jsonc
{
    "plugins": [{
        "package": "/absolute/path/to/memento/adapters/opencode",
        "options": {
            "url": "http://127.0.0.1:7377",
            "tailUserTurns": 3,
            "tailCharBudget": 40000,
            "digestBudget": 12000,
            "jev": true
        }
    }]
}
```

| Option | Default | Description |
| --- | --- | --- |
| `url` | `MEMENTO_URL` or `http://127.0.0.1:7377` | Daemon URL |
| `token` | `MEMENTO_TOKEN` | Optional bearer token |
| `autostart` | `false` | Start a native daemon on connection failure; native `memento setup` enables this |
| `mementoBin` | unset | Absolute path to the memento executable, written by native setup |
| `tailUserTurns` | `3` | Recent user turns retained after a model switch |
| `tailCharBudget` | `40000` | Target character budget for the pre-switch tail |
| `digestBudget` | `12000` | Digest character budget |
| `compaction` | `"native"` | `"memento"` replaces same-model compaction with the memento digest plus a recent tail and current-turn context at zero model cost; falls back to native on errors |
| `resume` | disabled | `true` or `{ idleMinutes, minContextTokens }` compacts before a resumed prompt after more than 60 idle minutes and at least 100000 estimated context tokens by default; enable with `memento setup --harness opencode --resume on --compaction memento`; `compaction: "memento"` avoids model summarisation cost |
| `subagentMemory` | `"digest"` | Default subagent mode: `"off"`, `"digest"`, or `"filtered"`; enabled modes inject frozen root memory, and `"filtered"` omits the parent's newest user request and acknowledgement notes shorter than 40 characters |
| `subagentMemoryAgents` | `{}` | Map agent IDs to `"off"`, `"digest"`, or `"filtered"` to override the default mode; invalid entries are ignored |
| `subagentDigestBudget` | `12000` | Positive integer character budget for the subagent digest |
| `enabled` | `true` | Enable the plugin |
| `jev` | `true` | Enable Jev classification and relevance judgments |
| `jevModel` | `opencode/jev-1.13-free` | OpenCode model as `provider/id` or a model object; required verbatim model string when `jevUrl` is set |
| `jevUrl` | unset | Full System One endpoint URL; otherwise use OpenCode's built-in decisions API |
| `jevApiKeyEnv` | unset | Name of an environment variable holding the endpoint key |
| `jevApiKeyFile` | unset | File holding the endpoint key, with `~` expanded and whitespace trimmed |
| `jevCooldownMs` | `600000` | Milliseconds to skip Jev after a failure; non-negative integer |
| `harness` | `opencode` | Harness identifier |

For a custom System One endpoint, add options to the existing plugin entry:

```jsonc
{
    "jevUrl": "https://router.example.com/v1/systemone",
    "jevModel": "oc/jev-1.13-free",
    "jevApiKeyFile": "~/.config/router/systemone-key"
}
```

Set either `jevApiKeyFile` or `jevApiKeyEnv`, not both. With neither, the
request is unauthenticated. A missing `jevModel` disables Jev for a custom
endpoint. The endpoint receives classified session text and search queries
and candidate memories; use a trusted HTTPS endpoint for remote services.

On each completed step and execution the adapter ingests transcript messages.
A backlog (new install on an existing session, reset cursor, fork) is sent in batches
of at most 64 items or 2 MB per `/v1/ingest` request, whichever comes first; the
cursor is saved after each successful batch, so a timeout or restart resumes from
the failed batch. Ingest requests use a 60-second timeout (other requests keep 10
seconds), the per-session serializer allows one drain at a time, and each batch is
recorded in the activity log with counts only.
Native autostart waits at most 15 seconds for foreground requests, respects shorter
existing deadlines, and retries once. Background ingest does not wait; its cursor
remains unchanged on failure and it retries once after successful startup. Failed
starts have a 60-second cooldown. Attempts and outcomes are recorded in the
activity log without content.
After a model switch it adds a per-model frozen digest to the outgoing context
while preserving a bounded recent tail. The `memory_search` tool retrieves stored memories, and
`/memento-continue <harness>/<session>` links a session to an earlier session,
including one from another harness. A bare session ID means `opencode`.
`/memento-handoff [<harness>]` flushes the current session and prepares the
next session in the project to continue it. When a harness is specified, only
that harness can claim the handoff. A handoff replaces the pending handoff in
the same target slot and expires after 24 hours.

Cross-harness project identity uses OpenCode's canonical checkout path
(`ctx.location.project.canonical`), falling back to the project ID when the
canonical path is unavailable or empty. Linked worktrees share the canonical
main checkout key. Existing memories stored under project IDs are not migrated.
Project and global searches span harnesses.

## Development

```sh
bun install
bun test
bunx tsc --noEmit
```
