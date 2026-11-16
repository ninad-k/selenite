# Open questions

| # | Question | Owner | Status | Notes |
|---|---|---|---|---|
| 1 | Which licence should the project use? | Owner | Open | Options: MIT/Apache-2.0 dual, AGPL-3.0, MPL-2.0. The repository stays private until this is decided. |
| 2 | What is the final product name? | Owner | Open | Working name is Selenite. A rename means changing `APP_NAME` (crates/core) and the bundle config (apps/desktop/src-tauri/tauri.conf.json). |
| 3 | Which YAML crate should be used? | Engineering | Decided | serde_yaml is archived, so we use serde_norway (maintained fork). Revisit if it stalls. |
| 4 | Who registers the OAuth clients for Google Drive and OneDrive? | Owner | Open | The owner must create OAuth apps in their own Google Cloud and Microsoft Entra accounts. Needed before Phase 5. |
| 5 | How will iOS builds be produced? | Owner | Open | iOS builds need macOS and Xcode; the development machine is Windows. |
| 6 | Which code signing certificates will sign the Windows and macOS installers? | Owner | Open | Needed before any public installer is distributed. |

## Approved dependencies (owner, 2026-11-16)

Rust: thiserror, anyhow, serde, serde_json, serde_norway, rusqlite (bundled, FTS5), pulldown-cmark,
clap, tempfile, walkdir, blake3, chrono, tracing, tracing-subscriber, criterion, tauri 2, tauri-build 2,
rmcp (Phase 3). UI: vite, typescript, @tauri-apps/api, CodeMirror 6. Anything else needs approval.
