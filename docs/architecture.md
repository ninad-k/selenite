# Architecture

## Principles

1. **Files are the truth.** A vault is a folder of Markdown files with YAML front matter. Everything
   else is derived.
2. **The index is a cache.** `.selenite/index.sqlite` can be deleted at any time; `rebuild-index`
   recreates it byte-for-byte equivalent in results.
3. **Thin UI.** The Tauri webview renders and forwards intents; all rules live in Rust crates.
4. **One engine, three modes.** Vault, Workspace and Tasks are presentation shells over the same files.

## Crate graph

```
            core  (APP_NAME, Mode, VaultPath, domain types)
           /  |  \
       vault storage  reminders
           \   |   /
            index            sync (storage + index, Phase 5)
              |
   cli   desktop   mcp   server
```

| Crate | Responsibility | Depends on |
|-------|----------------|------------|
| `core` | Domain model, `VaultPath` (validated, `/`-separated, cannot escape the vault), modes, product name | — |
| `vault` | Parse/serialise Markdown + front matter; extract wikilinks, tags, headings; layout rules (daily notes, config) | core |
| `storage` | `StorageProvider` trait; `LocalFolder`; later `GoogleDrive`, `OneDrive`; atomic writes, revisions | core |
| `index` | SQLite + FTS5: pages, links, backlinks, tags, properties, tasks, search; rebuild from a provider | core, vault, storage |
| `sync` | Manifest (path, hash, etag, mtime), two-way sync, conflict copies, offline queue | core, storage |
| `reminders` | 7-day planner over indexed tasks; OS notification adapters | core |
| `mcp` | MCP tools/resources over the engine; stdio + Streamable HTTP; audit log | core, vault, storage, index |
| `cli` | `selenite` binary | all engine crates |
| `apps/desktop` | Tauri 2 shell + TypeScript UI (CodeMirror 6 editor) | engine crates |
| `apps/server` | Headless sync + reminders + MCP over HTTP | engine crates |

An `Engine` facade (in `index`, Phase 1) ties a provider, the parser and the index together so the
CLI, desktop app and MCP server share one code path for every operation.

## On-disk layout of a vault

```
<vault>/
  .selenite/
    config.yaml          # space mode, mode names, daily-note folder/format (synced)
    index.sqlite         # disposable cache (never synced, git-ignored)
  .obsidian/             # created empty-ish so the folder opens as a vault in Obsidian
  Welcome.md
  Daily/2026-11-16.md    # daily notes
  <Database>/            # Phase 4: _schema.yaml + one .md per row
    _schema.yaml
    Row A.md
```

Hidden folders (any segment starting with `.`) are never indexed as pages.

## Page format

```markdown
---
title: Optional
tags: [project, area/sub]
aliases: [Other name]
---
Body with [[Wikilink]], [[Target|alias]], [[Target#Heading]], ![[Embed]] and #tags.
```

Round-trip rule: the parser keeps the raw front matter text and the raw body. Serialising an
untouched document reproduces the original bytes exactly; editing a property rewrites only the
front matter block.

## Storage contract

`StorageProvider` is synchronous (callers on async runtimes use `spawn_blocking`). Operations:
`list`, `stat`, `read`, `write(path, bytes, expected_revision)`, `rename`, `trash`. `write` is
atomic and fails with `Conflict` if `expected_revision` no longer matches; sync turns conflicts
into conflict copies instead of overwriting. There is no hard delete in the trait: `trash` moves
files to `.selenite/trash/`.

## Index lifecycle

- Opened with `INDEX_VERSION` stored in `PRAGMA user_version`. Mismatch => drop and rebuild.
- Incremental update per changed file (content hash compared with the stored hash).
- `rebuild-index` deletes and repopulates everything from the provider listing.
- Link resolution: `[[Name]]` resolves to the page whose stem or alias matches (case-insensitive),
  preferring the shortest path; unresolved links are kept so they resolve when the page appears.

## Security notes

- `VaultPath` validation blocks path traversal for every input (CLI, UI, MCP).
- Tauri capabilities are minimal; the UI cannot touch the filesystem directly.
- Logs carry paths and counts only, never note content or tokens.
- OAuth tokens (Phase 5) go to the OS keychain only.
