// Plugin host UI: runs enabled plugins in sandboxed iframes, relays their postMessage requests to
// the `plugin_call` command, and provides the command palette (Ctrl+P) and the plugin manager.
//
// Security model (see docs/plugins.md): the iframe is `sandbox="allow-scripts"` without
// `allow-same-origin`, so it has an opaque origin and no access to this page, its storage or the
// Tauri IPC bridge. Its document carries a CSP of `default-src 'none'`, so it cannot load
// anything or open connections. It can only postMessage requests to this window, and this file
// forwards them to Rust, which checks the manifest permissions. This file never decides what a
// plugin may do and never trusts a plugin-supplied identity: the plugin id is taken from the
// iframe the message came from. All text goes in via textContent.

import "./panels.css";
import { api, errorText, type PluginInfo, type PluginPermission } from "./api";
import { button, h, modal } from "./dom";

export interface BuiltinCommand {
  id: string;
  title: string;
  run: () => void;
}

export interface PluginHooks {
  /** Built-in commands for the palette. */
  builtins: () => BuiltinCommand[];
  /** Path of the page open in the editor, passed to plugin commands as context. */
  activePath: () => string | null;
}

interface Running {
  id: string;
  name: string;
  frame: HTMLIFrameElement;
  commands: Map<string, string>;
}

const PROTOCOL_VERSION = 1;
const running = new Map<string, Running>();
let hooks: PluginHooks | null = null;
let listening = false;
let generation = 0;

/** Runs inside the iframe before the plugin; defines the `selenite` global over postMessage. */
const BOOTSTRAP = [
  '"use strict";',
  "(function () {",
  "  var pending = new Map();",
  "  var handlers = Object.create(null);",
  "  var seq = 0;",
  "  function rpc(method, args) {",
  "    return new Promise(function (resolve, reject) {",
  "      var id = ++seq;",
  "      pending.set(id, { resolve: resolve, reject: reject });",
  `      parent.postMessage({ v: ${PROTOCOL_VERSION}, id: id, method: method, args: args || {} }, "*");`,
  "    });",
  "  }",
  '  window.addEventListener("message", function (ev) {',
  "    if (ev.source !== parent) return;",
  `    var m = ev.data;`,
  `    if (!m || m.v !== ${PROTOCOL_VERSION}) return;`,
  '    if (m.type === "command") {',
  "      var fn = handlers[m.id];",
  "      if (typeof fn === 'function') {",
  "        Promise.resolve().then(function () { return fn(m.context || {}); }).catch(function (e) {",
  "          var msg = String((e && e.message) || e).slice(0, 200);",
  "          rpc('ui.notify', { text: 'Command failed: ' + msg }).catch(function () {});",
  "        });",
  "      }",
  "      return;",
  "    }",
  "    var p = pending.get(m.id);",
  "    if (!p) return;",
  "    pending.delete(m.id);",
  "    if (m.ok) p.resolve(m.result); else p.reject(new Error(String(m.error)));",
  "  });",
  "  function args(o) { var out = {}; for (var k in o) { if (o[k] !== undefined) out[k] = o[k]; } return out; }",
  "  var api = {",
  `    version: ${PROTOCOL_VERSION},`,
  "    vault: {",
  "      search: function (q, limit) { return rpc('vault.search', args({ q: q, limit: limit })).then(function (r) { return r.results; }); },",
  "      read: function (path) { return rpc('vault.read', { path: path }); },",
  "      backlinks: function (path) { return rpc('vault.backlinks', { path: path }).then(function (r) { return r.backlinks; }); },",
  "      createPage: function (title, markdown) { return rpc('vault.createPage', { title: title, markdown: markdown }); },",
  "      appendBlocks: function (path, markdown) { return rpc('vault.appendBlocks', { path: path, markdown: markdown }); }",
  "    },",
  "    ui: {",
  "      registerCommand: function (id, title, handler) {",
  "        if (typeof handler !== 'function') return Promise.reject(new Error('registerCommand needs a handler function'));",
  "        handlers[id] = handler;",
  "        return rpc('ui.registerCommand', { id: id, title: title });",
  "      },",
  "      notify: function (text) { return rpc('ui.notify', { text: String(text) }); }",
  "    }",
  "  };",
  "  Object.freeze(api.vault); Object.freeze(api.ui); Object.freeze(api);",
  "  Object.defineProperty(window, 'selenite', { value: api, writable: false, configurable: false });",
  "})();",
].join("\n");

const FRAME_CSP = "default-src 'none'; script-src 'unsafe-inline'; form-action 'none'; base-uri 'none'";

/** Keep text from ending its <script> element or opening an HTML comment. */
function scriptSafe(js: string): string {
  return js.replace(/<\/(script)/gi, "<\\/$1").replace(/<!--/g, "<\\!--");
}

function buildSrcdoc(source: string): string {
  return (
    '<!doctype html><html><head><meta charset="utf-8">' +
    `<meta http-equiv="Content-Security-Policy" content="${FRAME_CSP}"></head><body>` +
    `<script>${scriptSafe(BOOTSTRAP)}</script><script>${scriptSafe(source)}</script></body></html>`
  );
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

// ---------------------------------------------------------------- toast

function toast(text: string): void {
  const el = h("div", { class: "toast", text });
  el.setAttribute("role", "status");
  document.body.append(el);
  window.setTimeout(() => el.remove(), 5000);
}

// ---------------------------------------------------------------- running plugins

function find(source: MessageEventSource | null): Running | undefined {
  if (!source) return undefined;
  for (const r of running.values()) if (r.frame.contentWindow === source) return r;
  return undefined;
}

async function onMessage(ev: MessageEvent): Promise<void> {
  const r = find(ev.source);
  if (!r) return; // not one of our plugin frames: ignore
  const m: unknown = ev.data;
  if (!isRecord(m) || typeof m.id !== "number") return;
  const id = m.id;
  const reply = (ok: boolean, payload: unknown): void => {
    if (running.get(r.id) !== r) return;
    r.frame.contentWindow?.postMessage(
      ok ? { v: PROTOCOL_VERSION, id, ok, result: payload } : { v: PROTOCOL_VERSION, id, ok, error: payload },
      "*",
    );
  };
  if (m.v !== PROTOCOL_VERSION || typeof m.method !== "string") {
    reply(false, "unsupported protocol version or malformed request");
    return;
  }
  try {
    const result = await api.pluginCall(r.id, m.method, isRecord(m.args) ? m.args : {});
    if (running.get(r.id) === r && isRecord(result)) {
      if (m.method === "ui.registerCommand" && typeof result.id === "string" && typeof result.title === "string") {
        r.commands.set(result.id, result.title);
      } else if (m.method === "ui.notify" && typeof result.text === "string") {
        toast(`${r.name}: ${result.text}`);
      }
    }
    reply(true, result);
  } catch (err) {
    reply(false, errorText(err));
  }
}

async function startPlugin(info: PluginInfo, gen: number): Promise<void> {
  const src = await api.pluginLoad(info.id);
  if (gen !== generation) return; // the vault changed while loading
  const frame = document.createElement("iframe");
  frame.setAttribute("sandbox", "allow-scripts");
  frame.setAttribute("title", `Plugin ${src.name}`);
  frame.setAttribute("aria-hidden", "true");
  frame.tabIndex = -1;
  frame.className = "plugin-frame";
  frame.srcdoc = buildSrcdoc(src.source);
  running.set(src.id, { id: src.id, name: src.name, frame, commands: new Map() });
  document.body.append(frame);
}

/** Stop every plugin and forget its commands (vault closed or switched). */
export function stopPlugins(): void {
  generation++;
  for (const r of running.values()) r.frame.remove();
  running.clear();
}

/** (Re)start the enabled plugins of the open vault. */
export async function reloadPlugins(): Promise<PluginInfo[]> {
  stopPlugins();
  const gen = generation;
  let list: PluginInfo[] = [];
  try {
    list = await api.pluginList();
  } catch {
    return [];
  }
  for (const p of list) {
    if (!p.enabled || p.error) continue;
    try {
      await startPlugin(p, gen);
    } catch (err) {
      toast(`Plugin ${p.name} did not start: ${errorText(err)}`);
    }
  }
  return list;
}

export function initPlugins(h2: PluginHooks): void {
  hooks = h2;
  if (!listening) {
    listening = true;
    window.addEventListener("message", (ev) => void onMessage(ev));
  }
}

// ---------------------------------------------------------------- command palette

interface Entry {
  key: string;
  title: string;
  run: () => void;
}

function entries(): Entry[] {
  const out: Entry[] = (hooks?.builtins() ?? []).map((c) => ({ key: `app:${c.id}`, title: c.title, run: c.run }));
  for (const r of running.values()) {
    for (const [cid, title] of r.commands) {
      out.push({
        key: `plugin:${r.id}:${cid}`,
        title: `${r.name}: ${title}`,
        run: () =>
          r.frame.contentWindow?.postMessage(
            { v: PROTOCOL_VERSION, type: "command", id: cid, context: { activePath: hooks?.activePath() ?? null } },
            "*",
          ),
      });
    }
  }
  return out;
}

export function openPalette(): void {
  const m = modal("Command palette");
  const input = h("input", { type: "text", label: "Command" });
  input.autocomplete = "off";
  input.spellcheck = false;
  input.placeholder = "Type a command";
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-expanded", "true");
  input.setAttribute("aria-controls", "palette-results");
  const list = h("ul", { class: "results palette-results", role: "listbox", label: "Commands" });
  list.id = "palette-results";
  m.body.append(input, list);
  let shown: Entry[] = [];
  let active = 0;

  const render = (): void => {
    list.replaceChildren();
    shown.forEach((e, i) => {
      const li = h("li", { class: "result", text: e.title, role: "option" });
      li.id = `palette-${i}`;
      li.setAttribute("aria-selected", String(i === active));
      li.addEventListener("mousedown", (ev) => ev.preventDefault());
      li.addEventListener("click", () => choose(i));
      list.append(li);
    });
    if (shown.length > 0) input.setAttribute("aria-activedescendant", `palette-${active}`);
    else input.removeAttribute("aria-activedescendant");
    document.getElementById(`palette-${active}`)?.scrollIntoView({ block: "nearest" });
  };
  const filter = (): void => {
    const q = input.value.trim().toLowerCase();
    shown = entries().filter((e) => e.title.toLowerCase().includes(q));
    active = 0;
    render();
  };
  const choose = (i: number): void => {
    const e = shown[i];
    if (!e) return;
    m.close();
    e.run();
  };
  input.addEventListener("input", filter);
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" && shown.length > 0) {
      e.preventDefault();
      active = (active + 1) % shown.length;
      render();
    } else if (e.key === "ArrowUp" && shown.length > 0) {
      e.preventDefault();
      active = (active - 1 + shown.length) % shown.length;
      render();
    } else if (e.key === "Enter") {
      e.preventDefault();
      choose(active);
    }
  });
  filter();
  input.focus();
}

// ---------------------------------------------------------------- plugin manager

const PERMISSION_TEXT: Record<PluginPermission, string> = {
  read: "read your pages, search them and see their backlinks",
  write: "create pages and add text to existing pages (it cannot delete or rename anything)",
  commands: "add commands to the command palette",
};

function permissionSummary(perms: PluginPermission[]): string {
  return perms.length === 0 ? "none (it can only show notifications)" : perms.join(", ");
}

export function openPluginManager(): void {
  const m = modal("Plugins");
  m.body.classList.add("plugin-manager");

  const render = async (): Promise<void> => {
    let list: PluginInfo[];
    try {
      list = await reloadPlugins();
    } catch (err) {
      m.body.replaceChildren(h("p", { class: "error", text: errorText(err) }));
      return;
    }
    m.body.replaceChildren(
      h("p", {
        class: "small muted",
        text:
          "Plugins are small scripts stored in .selenite/plugins/<id>/ inside your vault. They are " +
          "off until you enable them, run in a sandbox with no network access, and can only do what " +
          "their permissions say. Only enable plugins you trust.",
      }),
    );
    if (list.length === 0) {
      m.body.append(h("p", { text: "No plugins found in this vault." }));
      return;
    }
    const ul = h("ul", { class: "plugin-list" });
    for (const p of list) ul.append(pluginItem(p, render));
    m.body.append(ul);
  };
  void render();
}

function pluginItem(p: PluginInfo, rerender: () => Promise<void>): HTMLLIElement {
  const state = p.error
    ? `Cannot be used: ${p.error}`
    : p.enabled
      ? "Enabled"
      : p.changed
        ? "Changed since you enabled it. It is not running until you enable it again."
        : "Disabled";
  const li = h(
    "li",
    { class: "plugin-item" },
    h("div", { class: "plugin-head" }, h("strong", { text: p.name }), h("span", { class: "muted small", text: `v${p.version}` })),
  );
  if (p.description) li.append(h("p", { class: "small", text: p.description }));
  if (!p.error) li.append(h("p", { class: "small muted", text: `Permissions: ${permissionSummary(p.permissions)}` }));
  li.append(h("p", { class: p.error ? "small error" : "small muted", text: state }));
  if (p.error) return li;

  const fail = (err: unknown): void => {
    li.append(h("p", { class: "small error", text: errorText(err) }));
  };
  const actions = h("div", { class: "row" });
  if (p.enabled) {
    actions.append(
      button("Disable", () => {
        api
          .pluginSetEnabled(p.id, false)
          .then(rerender)
          .catch(fail);
      }),
    );
  } else {
    actions.append(
      button("Enable…", () => {
        const items = h("ul", { class: "small" });
        items.append(h("li", { text: "show notifications" }));
        for (const perm of p.permissions) items.append(h("li", { text: PERMISSION_TEXT[perm] }));
        const confirm = h(
          "div",
          { class: "plugin-confirm" },
          h("p", { class: "small", text: `Allow "${p.name}" to:` }),
          items,
          h("p", { class: "small muted", text: "It runs in a sandbox and cannot use the network or change settings." }),
          h(
            "div",
            { class: "row" },
            button(
              "Enable plugin",
              () => {
                api
                  .pluginSetEnabled(p.id, true)
                  .then(rerender)
                  .catch(fail);
              },
              "primary",
            ),
            button("Cancel", () => confirm.remove()),
          ),
        );
        li.append(confirm);
      }),
    );
  }
  li.append(actions);
  return li;
}
