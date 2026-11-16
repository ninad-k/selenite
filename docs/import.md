# Importing from other apps

`selenite import obsidian|notion|todoist <PATH> [--into FOLDER] [--dry-run] [--json]`

All importers write into a folder inside the vault (default `Imported/<Source> <YYYY-MM-DD>`), go through the engine's atomic writes, and never overwrite an existing file: on a name clash the new file gets ` (imported)` (then ` (imported) 2`, ...) appended. `--dry-run` writes nothing and prints what would happen. The report lists pages, rows, tasks and attachments written plus every skipped item with a reason. Imported content is treated purely as data and is never logged (only paths and counts). Exit code 0 on success, 1 on error.

## Obsidian

Point at the vault folder. Markdown files and attachments are copied byte for byte with the folder structure kept. `.obsidian/` (app settings, including the daily-notes folder setting) and `.trash/` are skipped. Front matter and wikilinks already work, so nothing is rewritten.

## Notion

In Notion, export with format "Markdown & CSV" (see the Notion help page "Export your content", https://www.notion.com/help/export-your-content). Pass the downloaded `.zip` (nested zips are unpacked) or an already extracted folder.

| Notion | Selenite |
|---|---|
| `Title <id>.md` | `Title.md` (ids removed from every file and folder name) |
| `Title <id>/` folder | `Title/` folder (sub-pages and attachments keep their place) |
| `Title <id>.csv` database | folder `Title/` with `_schema.yaml` (table database) and one `.md` per row; `_all.csv` duplicates are skipped |
| internal links `[Text](Page%20<id>.md)` | `[[Page]]`, or `[[Page\|Text]]` when the text differs (full path if two pages share a name) |
| links to a database CSV | plain link text |
| images/files | copied beside the pages, links rewritten (relative, `%20`-encoded) |
| "Property: value" block under a row page title | front matter (typed from the CSV row when there is one) |

Column types are inferred from the CSV: first column is the title; Yes/No becomes checkbox; all-numeric number; dates like `November 16, 2026` (optionally with time) date; `http(s)` links url; comma lists with a small repeated vocabulary multi-select; up to 20 short repeated values select; anything else text.

## Todoist

In Todoist, export a project as CSV (help: https://www.todoist.com/help/articles/import-or-export-a-project-as-a-csv-file) or pass a folder of such files. Each file becomes a Tasks database named after the file.

- `TYPE=task` rows become tasks; `section` rows set the section of the following tasks; `note` rows are appended to the previous task under "Notes".
- `INDENT` > 1 makes a sub-task whose `parent` is a wikilink to the previous task one level up.
- `PRIORITY` 4 (highest) to 1 maps to p1 to p4.
- `DATE` text such as "every 1st at 9am" or "tomorrow" is run through the quick-add parser relative to the import time; text it cannot parse is kept in the task body as "Original date: ...".
- `@label` tokens in `CONTENT` become labels. `DESCRIPTION` becomes the body.

## Not supported

Notion: databases' relations, rollups, formulas and views (exported as plain text values), date ranges and time zones (kept as text), comments, page icons/covers, `.html` exports, links to database CSV files. Obsidian: plugin data, canvas semantics (files copied as is), daily-note settings. Todoist: completed tasks (not in the export), reminders, assignees, comments other than `note` rows, the `meta` view rows.
