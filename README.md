# Selenite

Selenite is a local-first app that combines a note vault, a block workspace with databases, and a task manager on one Rust engine. Your data lives in plain Markdown files and ordinary folders that you can open in any text editor; the search and link index is a rebuildable cache, and sync uses storage you already own.

Status: early development (Phase 0 scaffold)

## Layout

- `crates/core`: shared domain types and the `APP_NAME` constant
- `crates/vault`: reading and writing Markdown pages, front matter and links
- `crates/storage`: the `StorageProvider` trait and its providers (local folder, cloud drives)
- `crates/index`: the disposable SQLite and FTS5 index
- `crates/sync`: change detection and reconciliation between devices
- `crates/reminders`: reminder planning and OS notification scheduling
- `crates/mcp`: the MCP server for AI agents
- `crates/cli`: command-line tools such as `rebuild-index`
- `apps/desktop`: the Tauri 2 desktop app (Rust shell and webview UI)
- `apps/server`: the headless server (planned for a later phase)

## Build

```sh
cargo build
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Desktop app:

```sh
cd apps/desktop/ui && pnpm install && cd ..
cargo tauri dev
```

## Documentation

- `docs/decisions.md`: architecture decision records
- `docs/parity/`: capability tracking for the Vault, Workspace and Tasks modes
- `docs/references.md`: sources studied and how they were used
- `docs/open-questions.md`: decisions still pending

## Licence

Licence: not yet chosen (see docs/open-questions.md).

Inspired by the capability sets of popular note, wiki and task apps; built clean-room (see docs/decisions.md ADR-008).
