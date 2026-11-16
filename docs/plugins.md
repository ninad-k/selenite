# Plugins (API v1)

Plugins are small JavaScript files that add commands to the desktop app. They are safe by
construction: off by default, sandboxed, permission-checked in Rust, rate-limited and audit-logged.

## Installing

A plugin is a folder inside the vault: `.selenite/plugins/<id>/` with a `manifest.json` and the script
it names. An example lives in `examples/plugins/word-count/`; copy it into the vault to try it.
Then open the command palette (Ctrl+P), run **Manage plugins**, review the permissions and press
**Enable**.

```json
{ "id": "word-count", "name": "Word count", "version": "1.0.0",
  "description": "Adds a command that shows the word count of the open page.",
  "permissions": ["read", "commands"], "main": "main.js" }
```

- `id`: lower-case letters, digits and single hyphens, at most 48 characters; must equal the folder name.
- `permissions`: any of `read`, `write`, `commands`. An unknown permission makes the manifest invalid.
- `main`: a `.js` file name in the same folder (default `main.js`), at most 512 KiB.

## Enabling

Plugins are disabled by default. Enabling is a user action in the Plugins dialog and is stored in
`.selenite/plugins/enabled.json` together with a hash of the manifest and script. If either file
changes afterwards (edited, replaced by sync, swapped by another program) the plugin stops running
and shows "changed" until you enable it again. `enabled.json` is never synced, so a remote cannot
enable a plugin on your device. No plugin method can enable or disable plugins.

## The API

Inside the sandbox a frozen global `selenite` is available. Every call returns a Promise.

| Call | Permission | Result |
|------|-----------|--------|
| `selenite.vault.search(q, limit?)` | `read` | `[{path, title, snippet}]` |
| `selenite.vault.read(path)` | `read` | `{path, text, revision}` |
| `selenite.vault.backlinks(path)` | `read` | `[{source, title, line, context}]` |
| `selenite.vault.createPage(title, markdown)` | `write` | `{path, revision}` (root folder, fails if it exists) |
| `selenite.vault.appendBlocks(path, markdown)` | `write` | `{path, revision}` |
| `selenite.ui.registerCommand(id, title, handler)` | `commands` | shows the command in the palette; `handler({activePath})` runs when chosen |
| `selenite.ui.notify(text)` | none | shows a short notice |

There is no method to delete, rename, move, change settings, enable plugins or use the network.
Only visible Markdown pages are reachable (not `.selenite/`). Markdown payloads are limited to 256 KiB.

## Protocol

The iframe talks to the host with `postMessage`: request `{v: 1, id, method, args}`, response
`{v: 1, id, ok, result | error}`; the host sends `{v: 1, type: "command", id, context}` to run a
command. `selenite` wraps this, so plugins normally never see it. Method names and arguments are
fixed (`crates/plugins/src/host.rs`, `METHODS`); unknown fields are rejected.

## Security model

1. **Sandbox.** Each enabled plugin runs in an `<iframe sandbox="allow-scripts">` without
   `allow-same-origin`: opaque origin, no access to the app's DOM, storage, cookies or Tauri IPC, no
   popups, forms or top navigation.
   The frame is not a `srcdoc`: Rust serves it from the `plugin` URI scheme
   (`http://plugin.localhost/<id>/` on Windows, `plugin://localhost/<id>/` elsewhere) and the UI only
   sets `iframe.src`. Only an enabled plugin whose files still match the hash in `enabled.json` is
   served; everything else is a 404. The response carries `Content-Security-Policy: default-src
   'none'; script-src 'unsafe-inline'; form-action 'none'; base-uri 'none'; navigate-to 'none'` and
   `Cross-Origin-Resource-Policy: same-origin`, so the plugin cannot fetch, open sockets or load
   images/frames.
2. **App CSP.** The main window has a strict CSP (`app.security.csp` in `tauri.conf.json`):
   `default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:
   https: http://vault.localhost vault:; font-src 'self' data:; connect-src 'self' ipc:
   http://ipc.localhost; frame-src http://plugin.localhost plugin:; object-src 'none'; base-uri
   'none'; form-action 'none'` (Tauri adds a hash for its own injected script). `unsafe-inline` for
   styles is needed for CodeMirror and inline `style` attributes. `devCsp` additionally allows the
   Vite dev server. The capability file grants `core:default` to the `main` window only and has no
   remote URLs.
3. **One narrow door.** The only channel is `postMessage` to the app window. The app maps the
   sending frame to its plugin id (a plugin never names itself) and calls the Tauri command
   `plugin_call(plugin_id, method, args)`. The iframe never receives raw IPC.
4. **Rust decides.** `plugin_call` loads the plugin, requires it to be enabled with unchanged files,
   looks the method up in the fixed table, checks the manifest permission, enforces the rate limit
   (120 calls and 20 writes per minute per plugin), validates the arguments and then calls the engine.
5. **Audit.** Every write call (`createPage`, `appendBlocks`), including refused and rate-limited
   ones, appends a line to `.selenite/audit/mcp-YYYY-MM.jsonl` with transport `plugin:<id>`:
   paths, revisions, outcome and an argument digest, never content.
6. **No destructive power.** Writes use the engine's conflict-checked, atomic save; nothing can be
   deleted or renamed, and settings are out of reach.

### Verified in the real app

Checked in the built desktop app (WebView2, `cargo tauri build --debug --no-bundle`, driven over
the DevTools protocol) with an enabled read-only probe plugin: it sees `__TAURI_INTERNALS__`,
`window.ipc` and `chrome.webview` objects (Tauri injects them into every frame), but
`parent.document` is blocked, `fetch` to the IPC endpoint and to the internet is blocked, a forged
`vault.createPage` is refused by Rust (`write` permission required), a direct
`__TAURI_INTERNALS__.invoke('create_page' | 'vault_info')` never gets a response (Tauri rejects the
opaque origin) and an allowed `vault.search` works. No file was created in the vault. The app itself
(editor, Workspace images from the `vault` scheme, Tasks, Ask, command palette, plugin commands)
works under the CSP with no violations reported in the console.

### Known limits

- A sandboxed frame can still navigate itself (`location = url`), which could carry data in a URL;
  CSP `navigate-to` is sent but not implemented by browsers. Plugins can only see what their permissions allow,
  so enable only plugins you trust, as the dialog says.
- A plugin can use CPU in its own frame (a busy loop); the rate limit protects the vault, not the UI.
- Tauri still injects its IPC objects into plugin frames. Isolation from IPC relies on the sandbox's
  opaque origin (Tauri rejects it), the capability set (`core:default`, window `main`, no remote
  URLs) and the main window CSP; re-verify after upgrading Tauri or WebView2. Only Windows
  (WebView2) was tested; the `plugin://` form on macOS/Linux is untested.
- `style-src 'unsafe-inline'` remains in the app CSP.
