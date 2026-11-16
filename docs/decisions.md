# Architecture decision records

Each record gives its status, date, context, the decision and its consequences; a locked decision changes only through a new ADR that supersedes it.

## ADR-001: One engine, three modes

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Note-taking, block databases and task management are often built as separate products with separate data models, forcing migration when a user changes how they work.

**Decision:** One engine, three modes. A space picks a mode (Vault, Workspace or Tasks) at creation. Modes are UI shells and templates over the same data. A space can switch mode without migrating data. Mode names are configurable strings.

**Consequences:**
- Feature work lands once in the engine and appears in every mode.
- Mode-specific UI must not own data models; a mode is a view plus defaults.
- Mode names live in configuration, so renaming a mode does not touch stored data.

## ADR-002: Files are the source of truth

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Users need to own their data and keep it readable outside the app, so the vault must survive the app being removed.

**Decision:** Files are the source of truth. Every page is a Markdown file with YAML front matter. A database is a folder containing `_schema.yaml` plus one Markdown file per row. A task is a row in a Tasks database. Attachments are ordinary files. The vault must stay readable in any text editor and openable in common Markdown vault apps.

**Consequences:**
- Every change the app makes must round-trip through plain files; no hidden state lives only in the app.
- File formats are a public contract, so any change needs a migration path.
- Renames and moves must update links inside files, not only in the index.

## ADR-003: The index is a disposable cache

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Links, backlinks, tags, properties, tasks and search need fast queries, but storing them as the truth would duplicate the files.

**Decision:** The index is a disposable cache. Local SQLite (FTS5) holds links, backlinks, tags, properties, tasks and the full-text index. The index is never synced. It is fully rebuildable from files by `rebuild-index`.

**Consequences:**
- Deleting the index loses no user data; the rebuild path must be tested in CI.
- Sync transfers files only, and each device builds its own index.
- Index schema changes need no data migration, only a rebuild.

## ADR-004: Bring-your-own storage

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Users want sync without a vendor holding their data, and the app must not require a hosted service.

**Decision:** Bring-your-own storage behind one `StorageProvider` trait. v1 providers are LocalFolder, GoogleDrive and OneDrive. Access uses OAuth with PKCE on the user's own account, with least-privilege scopes (app-created files or app folder only). Tokens are stored in the OS keychain, never in the vault or logs.

**Consequences:**
- Each cloud provider needs an OAuth app registration owned by the project (see docs/open-questions.md).
- Provider code stays behind the trait; sync logic does not branch on provider.
- Credentials must be redacted from logs and never written into vault files.

## ADR-005: Seven-day reminder window

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Reminders must fire on time even when the app is offline and no server exists.

**Decision:** On every connect, sync or app start, compute all reminder occurrences for the next 7 days from the index and register them as OS-level local notifications so they fire offline with no server. Re-plan on every change.

**Consequences:**
- Reminders beyond the seven-day window are scheduled only when the window moves forward; this is accepted.
- OS notification limits must be respected by the reminders crate.
- Every task change triggers a re-plan, so the scheduler must be idempotent.

## ADR-006: Stack

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** The engine needs one language for correctness and portability, and the UI needs a mature webview with a capable editor.

**Decision:** Rust stable Cargo workspace for all domain logic, storage, sync, indexing, scheduling and MCP. Tauri 2 app shell (desktop first, mobile later). The UI in the webview may use CodeMirror 6 or ProseMirror/TipTap. The UI stays thin, with no business logic outside Rust.

**Consequences:**
- Business rules are tested in Rust without starting the UI.
- Editor choice is confined to the UI layer and can change without touching the engine.
- Mobile reuses the Rust core, so shell differences are the main new work.

## ADR-007: AI agents are first-class operators

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Agents need the same capabilities as people, with a clear audit trail of what they changed.

**Decision:** AI agents are first-class operators. An MCP server built with the official Rust SDK `rmcp` offers stdio and Streamable HTTP transports. Everything the UI can do, MCP can do except delete (archive only). Every MCP write is audit-logged.

**Consequences:**
- The UI and MCP share one command layer; no operation exists only in the UI.
- Archive-only deletion means agents never hold a destructive capability.
- The audit log is kept locally and must be readable by the user.

## ADR-008: Clean-room rule

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** Users compare the product to established apps, but copying their source, assets or wording would create legal and licensing risk.

**Decision:** Match reference apps by behaviour and capability only. Never copy their source, assets, icons, wording, trademarks or pixel layouts. Study only public docs and permissively licensed open source. Record every reference in docs/references.md.

**Consequences:**
- Reviewers check new UI copy and icons against this rule before merge.
- Names of other products appear only in docs/references.md and the README inspiration line.
- Every new reference needs a row in docs/references.md before the work starts.

## ADR-009: Renaming is cheap

- Status: Accepted (locked)
- Date: 2026-11-16

**Context:** The product name is a working name and may change before release.

**Decision:** The product name appears in exactly one constant, `APP_NAME` (crates/core), plus the bundle config (apps/desktop/src-tauri/tauri.conf.json).

**Consequences:**
- A rename is a change to two files, plus any display assets.
- Crate package names are not covered by the constant and must be changed separately if they contain the name.
- Code and docs should refer to the product only through `APP_NAME` to keep this true.
