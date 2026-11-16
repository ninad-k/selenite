# CLAUDE.md — working on this repo

Local-first notes / workspace / tasks engine in Rust with a Tauri 2 shell. The product name lives
only in `selenite_core::APP_NAME` and `apps/desktop/src-tauri/tauri.conf.json`.

## Commands

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings          # engine crates (default members)
cargo test                                          # engine crates
cargo test --workspace                              # includes desktop (needs apps/desktop/ui/dist)
cd apps/desktop/ui && pnpm install && pnpm build    # UI bundle
cd apps/desktop && cargo tauri dev                  # run the desktop app
cargo bench -p selenite-index                       # search benchmark
```

## Rules (non-negotiable)

- Locked decisions are in `docs/decisions.md` (ADR-001..009). Do not change them without the owner.
- Library crates: no `unwrap`/`expect`/`panic` (clippy denies them outside tests); errors via
  `thiserror`. `anyhow` only in binaries (`cli`, `server`, `desktop`).
- Every public item has a doc comment (`missing_docs` is on).
- Parsers/serialisers have round-trip tests: file -> model -> file is byte-stable for untouched content.
- All file writes go through `StorageProvider::write`, which is atomic (temp file + rename) and
  refuses to overwrite a file whose revision changed underneath it.
- The index (`.selenite/index.sqlite`) is a cache. Bump `INDEX_VERSION` with every schema change;
  a version mismatch rebuilds from files.
- Never log secrets, tokens or vault content (log paths and counts only).
- No business logic in the UI (TypeScript); it calls Tauri commands that call the engine.
- Clean-room: behaviour parity only; no copied code, assets, icons or wording. Log every external
  reference in `docs/references.md`.
- Parity checklists (`docs/parity/*.md`): update status in the same commit as the behaviour change.
- Conventional commits, one concern each. New dependencies need owner approval (see
  `docs/open-questions.md` for the approved list).

## Layout

See `docs/architecture.md`.

## Subagents

Complex engine work (parsers, index, sync, MCP, UI features) -> Sonnet. Docs, checklists,
boilerplate, mechanical edits -> Haiku. The lead reviews, integrates and runs the gates.
