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
| `ask_vault` | | Natural-language question -> ranked citable passages (see below) |
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
| `query_database` | | Tasks database: `filter` (list_tasks syntax). Table database: `view` (saved view name) and/or `query` (`{filter, sorts, group_by}`); returns rows with computed values and `groups` |
| `list_databases` | | Databases in the vault: folder, kind (`table` or `tasks`), row count |
| `get_database_schema` | | A database's properties, views and schema revision |
| `create_database` | yes | New table database in a folder: `Name` title plus `properties` (`[{"name", "type", ...}]`) and a default Table view |
| `update_database_schema` | yes | Replace `properties` and/or `views` of a table database; validated, invalid schemas are rejected without writing |
| `create_row` | yes | New row (Markdown file) in a table database with `values`; computed properties are ignored |
| `set_row_property` | yes | Set one stored property of a row (`value: null` clears); the body is untouched |
| `daily_note` | yes (if created) | Open or create a daily note |
| `list_tags`, `get_tree` | | Tags with counts, file tree |
| `archive_task`, `archive_page` | yes | Move to `.selenite/trash/` (also how a row is removed) |

There is **no delete tool**: agents can archive, never destroy. Every write accepts an optional
`revision` where it makes sense (page and schema writes); a stale revision returns a conflict
instead of overwriting. Schema and row values are validated by the engine; a rejected write is a
tool error (`isError`) and changes nothing.

Resources: every page as `vault:///<path>`. Prompts: `daily_review`, `answer_with_citations`.

## Asking the vault (`ask_vault`)

`ask_vault {question, limit?}` is retrieval only: Selenite never calls a model. The tool finds the
passages that best match a plain-language question and the client (Claude) writes the answer.

```json
{ "question": "...",
  "citations": [ { "path": "Travel/Berlin Trip.md", "title": "Berlin Trip", "heading": "Hotel",
                   "start_line": 11, "end_line": 11, "excerpt": "...", "score": 0.93 } ],
  "instructions": "Answer using only these passages and cite them as [path#Lstart-Lend]" }
```

- Pages are split into passages when indexed: by heading section, then paragraphs merged up to
  about 800 characters; a code fence is never split. `start_line` / `end_line` are 1-based lines of
  the whole file (front matter included), so `get_page` text lines up with the citation.
- The question loses its stopwords; the rest are `OR`ed with prefix matching over an FTS5
  `passages` table (bm25, headings weighted above body). Results are re-ranked by term coverage,
  term proximity, title and heading matches, and a small boost for pages linked to or from the best
  page. Overlapping passages are dropped, at most 3 per page, ties broken by path then line, so the
  order is deterministic. `limit` defaults to 8 (max 25); `score` is higher-is-better.
- It is a read tool: no audit entry. The `answer_with_citations` prompt tells the model to call
  `ask_vault`, answer only from the passages and cite them as `[path#Lstart-Lend]`.
- The desktop app shows the same ranked passages in its Ask panel (Ctrl+Shift+A); it makes no model
  call.

## Audit log

Each write call appends one JSON line to `.selenite/audit/mcp-YYYY-MM.jsonl`:
timestamp, tool, target paths, ok/error category, revisions before/after, and a blake3 digest of
the arguments. Page and task content is never written to the log (file names are paths, so a
title-derived file name is visible).

## Concurrency

The engine sits behind a mutex; each tool runs in `spawn_blocking` and holds the lock for the whole
call, so a tool's read-modify-write and its audit entry never interleave with another call.
