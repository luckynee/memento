# memento-pool

Session-continuation memory for coding-agent harnesses. Rust daemon + CLI backed by Qdrant, with a thin OpenCode plugin adapter.

Read `docs/MAP.md` before exploring. Read `docs/DESIGN.md` before changing behavior.

## Conventions

- Four-space indentation, no tabs. No code comments; use descriptive names.
- Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, `test:`), one logical change per commit.
- Rust: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass before committing Rust changes.
- Never commit secrets, model caches, or Qdrant storage.
