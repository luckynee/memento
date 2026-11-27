# Memento for agy

Install the `memento` and `memento-mcp` binaries on `PATH` and start a memento daemon. Set `MEMENTO_URL` and, when needed, `MEMENTO_TOKEN` in the environment inherited by agy.

Install this plugin from a local checkout:

```sh
agy plugin install /path/to/memento/adapters/agy
```

GitHub plugin installation for agy has not been verified. Without a checkout, use `memento install-hooks agy` for lifecycle integration and optionally register `memento-mcp` for memory tools.

Alternatively, copy `adapters/agy/` to the project's `.agents/plugins/memento/` for project discovery.

Remove any hooks previously added by `memento install-hooks agy` from project or user settings to avoid duplicate SessionStart and Stop runs.
