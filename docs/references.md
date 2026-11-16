# References

The clean-room rule: we match reference apps by behaviour and capability only, and never copy their source, assets, icons, wording, trademarks or pixel layouts. We study only public documentation and permissively licensed open source, and every reference is recorded in this table before work starts (ADR-008).

| Date | Source (URL) | What was studied | Licence / terms | Used for |
|---|---|---|---|---|
| 2026-11-16 | Obsidian public help docs, https://help.obsidian.md | Public help pages on internal links and front matter | Public help documentation; check site terms | File-format compatibility only |
| 2026-11-16 | CommonMark spec, https://spec.commonmark.org | Markdown syntax rules | CC-BY-SA | Parsing and rendering Markdown files |
| 2026-11-16 | YAML 1.2 spec, https://yaml.org/spec/1.2.2/ | YAML syntax and typing for front matter and `_schema.yaml` | Open specification | Front matter parsing |
| 2026-11-16 | RFC 5545 iCalendar, https://www.rfc-editor.org/rfc/rfc5545 | RRULE recurrence rules | IETF document, standard terms | Recurring task dates |
| 2026-11-16 | SQLite FTS5 docs, https://www.sqlite.org/fts5.html | Full-text index syntax and behaviour | Public domain | Index and full-text search |
| 2026-11-16 | Tauri 2 docs, https://v2.tauri.app | App shell, bundling and OS integration APIs | Public documentation; check project licence | Desktop shell |
| 2026-11-16 | Model Context Protocol spec, https://modelcontextprotocol.io | MCP transports and tool model | Open specification; check repository licence | MCP server (ADR-007) |
| 2026-11-16 | rmcp crate source (https://github.com/modelcontextprotocol/rust-sdk) | Server, tool macro and transport APIs (v3.10) | See crate licence | MCP server implementation |
| 2026-11-16 | MCP Inspector (https://github.com/modelcontextprotocol/inspector) | CLI mode for verifying tools/list, tools/call, resources/list | See project licence | Phase 3 verification only (not shipped) |
| 2026-11-16 | Notion help: export your content, https://www.notion.com/help/export-your-content | Public description of the Markdown & CSV export (zip, nested zips, per-page Markdown, per-database CSV) | Public help documentation; check site terms | Notion importer (Phase 7); file naming and layout were also confirmed against hand-built fixtures only |
| 2026-11-16 | Todoist help: import or export a project as CSV, https://www.todoist.com/help/articles/import-or-export-a-project-as-a-csv-file | Public description of the CSV columns (TYPE, CONTENT, DESCRIPTION, PRIORITY, INDENT, AUTHOR, RESPONSIBLE, DATE, DATE_LANG, TIMEZONE) and section/task rows | Public help documentation; check site terms | Todoist importer (Phase 7). The help page could not be fetched while implementing, so the PRIORITY direction (4 = highest) rests on the Todoist REST API reference below |
| 2026-11-16 | Todoist developer docs, https://developer.todoist.com/rest/v2/ | Task priority field: 1 normal to 4 very urgent (shown as p1 in the app) | Public developer documentation | Priority mapping in the Todoist importer |
| 2026-11-16 | Obsidian help: how Obsidian stores data, https://help.obsidian.md/data-storage | Vault layout; `.obsidian/` holds settings, `.trash/` the local trash | Public help documentation; check site terms | Obsidian importer (what to skip) |
