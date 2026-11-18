# OpenCode adapter

This plugin stores session messages in memento and recalls them when continuing work.
Run `bun install` in this directory, then add the plugin to `opencode.jsonc`:

```jsonc
{
    "plugins": [{
        "package": "/absolute/path/to/memento-pool/adapters/opencode",
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
| `tailUserTurns` | `3` | Recent user turns retained after a model switch |
| `tailCharBudget` | `40000` | Target character budget for the pre-switch tail |
| `digestBudget` | `12000` | Digest character budget |
| `enabled` | `true` | Enable the plugin |
| `jev` | `true` | Enable Jev classification and relevance judgments |
| `jevModel` | `opencode/jev-1.13` | Jev model as `provider/id` or a model object |
| `harness` | `opencode` | Harness identifier |

On each completed step and execution the adapter ingests transcript messages.
After a model switch it adds a per-model frozen digest to system instructions
while preserving a bounded recent tail. The `memory_search` tool retrieves stored memories, and
`/memento-continue <parentSessionID>` links a session to an earlier session.

## Development

```sh
bun install
bun test
bunx tsc --noEmit
```
