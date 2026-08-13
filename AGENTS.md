# Agent Instructions

- Commit after each completed change.
- Commit messages must explain why the change was made and summarize what it
  does at a high level.
- Keep all commit message lines at 72 columns or less.
- Do not sign commits.
- Before committing, run formatting for this repo.
- Before committing, ensure the crate builds and tests pass.
- Use `cargo fmt --all` for formatting.
- Use `cargo build`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`
  for normal validation.
- Be careful with expensive validation that invokes Nix builds. Run package
  rebuilds when the change affects Nix orchestration, activation behavior, or
  user-visible package workflows.
- If checks are intentionally limited, state exactly what was run and why.
- Do not leak information from private repos into this repo.
- Do not edit README or other Markdown documentation files unless the user
  explicitly asks for documentation changes.
- Maintain clear Rust APIs. Add doc comments where documentation improves
  clarity for public or non-obvious items.
- Avoid low-signal contrastive phrasing that restates what the user is not
  doing or does not want unless needed to prevent a concrete misunderstanding.
- When recommending an approach, state the recommended approach first. Put
  caveats after the recommendation.
- Prefer "Do Y because..." over phrasing that leads with what to avoid.
