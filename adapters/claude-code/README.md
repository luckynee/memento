# Memento for Claude Code

Install the `memento` binary on `PATH` and start a memento daemon. Set `MEMENTO_URL` and, when needed, `MEMENTO_TOKEN` in the environment inherited by Claude Code.

The plugin registers native Mod tools without an MCP server: `mcp__memento__memory_search`, `mcp__memento__memory_digest`, `mcp__memento__memory_continue`, and `mcp__memento__memory_note`. Claude Code's built-in Mod guard or organization policy can refuse Mods, preventing these tools from loading.

Add the GitHub repository as a marketplace and install the plugin:

```sh
claude plugin marketplace add luckynee/memento
claude plugin install memento@memento
```

From a checkout, add its path instead:

```sh
claude plugin marketplace add /path/to/memento
claude plugin install memento@memento
```

From the checkout root, `claude --plugin-dir adapters/claude-code` loads the plugin for one session.

Use `claude plugin configure memento` to choose `off`, `digest`, or `filtered` subagent memory. The default is `off`; `MEMENTO_SUBAGENT_MEMORY`, if set, overrides this setting.

Remove any hooks previously added by `memento install-hooks claude-code` from project or user settings to avoid duplicate SessionStart and Stop runs.
