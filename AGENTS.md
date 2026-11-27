# memento

Session-continuation memory for coding-agent harnesses. Rust daemon + CLI backed by Qdrant, with a thin OpenCode plugin adapter.

Read `docs/MAP.md` before exploring. Read `docs/DESIGN.md` before changing behavior.

## Conventions

- Four-space indentation, no tabs. No code comments; use descriptive names.
- Conventional Commits (`feat:`, `fix:`, `chore:`, `docs:`, `test:`), one logical change per commit.
- Rust: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass before committing Rust changes.
- Never commit secrets, model caches, or Qdrant storage.


# Opencode Rule ( ignore if the harness is not opencode )
- use coder with GPT-6.1 Terra OpenAI none
- do not call reviewer sub-agent on small iteration
- don't forget to commits
- use agy --dangerously-skip-permissions with Gemini 4 Neon (High) for second opinion or whenever you feel doubtfull

# Harness setting for testing
- opencode , GPT-6 luna OpenAI medium and Haiku 5.5Claude Agent medium
- agy Gemini 4 Hydrogen (medium)
- claude Haiku 5.5 medium
- codex GPT-6 luna medium
