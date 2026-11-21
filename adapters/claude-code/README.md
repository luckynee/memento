# Memento for Claude Code

Install the `memento` and `memento-mcp` binaries on `PATH` and start a memento daemon. Set `MEMENTO_URL` and, when needed, `MEMENTO_TOKEN` in the environment inherited by Claude Code.

From the repository root, load the plugin for one session:

```sh
claude --plugin-dir adapters/claude-code
```

Alternatively, add the repository as a marketplace and install the plugin:

```sh
claude plugin marketplace add /path/to/memento-pool
claude plugin install memento@memento-pool
```

Use `claude plugin configure memento` to choose `off`, `digest`, or `filtered` subagent memory. The default is `off`; `MEMENTO_SUBAGENT_MEMORY`, if set, overrides this setting.

Remove any hooks previously added by `memento install-hooks claude-code` from project or user settings to avoid duplicate SessionStart and Stop runs.
