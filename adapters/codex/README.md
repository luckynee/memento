# Memento for Codex

Install the `memento` and `memento-mcp` binaries on `PATH` and start a memento daemon. Set `MEMENTO_URL` and, when needed, `MEMENTO_TOKEN` in the environment inherited by Codex.

Add this repository as a marketplace and install the plugin:

```sh
codex plugin marketplace add /path/to/memento-pool
codex plugin add memento@memento-pool
```

Codex 0.170 does not run plugin lifecycle hooks. Until Codex supports them, run `memento install-hooks codex` separately for lifecycle integration.

MCP project resolution relies on Codex per-call workspace metadata handled by `memento-mcp`.
