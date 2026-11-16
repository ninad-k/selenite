# MCP server

AI agents operate a vault through the Model Context Protocol (ADR-007). The server is built on the
official Rust SDK (`rmcp`) and runs over stdio:

```bash
selenite --vault /path/to/vault mcp
```

Logs go to stderr; stdout carries the protocol.

## Client configuration

Claude Code:

```bash
claude mcp add vault -- selenite --vault /path/to/vault mcp
```

Any client that takes a JSON server list:

```json
{ "mcpServers": { "vault": { "command": "selenite", "args": ["--vault", "/path/to/vault", "mcp"] } } }
```

## Tools

| Tool | Writes | Purpose |
|------|--------|---------|
| `search` | | Full-text search with `tag:`, `path:`, `title:`, `-term`, `"phrases"` |
| `get_page` | | Page text, revision, backlinks, outgoing links |
| `list_blocks` | | Top-level blocks of a page with index and `^anchor` |
| `create_page` | yes | New page from a title or path |
| `append_blocks` | yes | Append Markdown to a page |
| `update_block` | yes | Replace one block (by index or `^anchor`), byte-stable elsewhere |
| `list_tasks` | | `inbox`, `today`, `upcoming`, `completed`, or a filter expression |
| `create_task` | yes | Natural-language quick add or explicit fields |
| `update_task` | yes | Change title, priority, due, project, section, labels |
| `complete_task` / `reopen_task` | yes | Complete (recurring tasks advance) / reopen |
| `set_reminder` | yes | Replace a task's reminders (`-15m`, `2026-11-20T08:45`) |
| `get_agenda` | | Overdue plus N days, recurring tasks expanded |
| `query_database` | | Filter a Tasks database (table databases arrive in Phase 4) |
| `daily_note` | yes (if created) | Open or create a daily note |
| `list_tags`, `get_tree` | | Tags with counts, file tree |
| `archive_task`, `archive_page` | yes | Move to `.selenite/trash/` |

There is **no delete tool**: agents can archive, never destroy. Every write accepts an optional
`revision`; a stale revision returns a conflict instead of overwriting.

Resources: every page as `vault:///<path>`. Prompt: `daily_review`.

## Audit log

Each write call appends one JSON line to `.selenite/audit/mcp-YYYY-MM.jsonl`:
timestamp, tool, target paths, ok/error category, revisions before/after, and a blake3 digest of
the arguments. Page and task content is never written to the log (file names are paths, so a
title-derived file name is visible).

## Concurrency

The engine sits behind a mutex; each tool runs in `spawn_blocking` and holds the lock for the whole
call, so a tool's read-modify-write and its audit entry never interleave with another call.
