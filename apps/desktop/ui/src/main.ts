import "./styles.css";
import { api, errorText, type TreeNode, type VaultInfo } from "./api";
import { createEditor, type EditorHandle } from "./editor";

const HL_START = "";
const HL_END = "";
const AUTOSAVE_MS = 600;
const SEARCH_DEBOUNCE_MS = 150;

function $<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
}

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: { class?: string; text?: string; type?: string; title?: string } = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (props.class) el.className = props.class;
  if (props.text !== undefined) el.textContent = props.text;
  if (props.type) el.setAttribute("type", props.type);
  if (props.title) el.title = props.title;
  for (const c of children) el.append(c);
  return el;
}

/** Build a node tree for a snippet, turning U+E000/U+E001 spans into <mark>. Text only. */
function highlighted(snippet: string): DocumentFragment {
  const frag = document.createDocumentFragment();
  let on = false;
  let buf = "";
  const flush = () => {
    if (!buf) return;
    if (on) {
      const m = document.createElement("mark");
      m.textContent = buf;
      frag.append(m);
    } else {
      frag.append(document.createTextNode(buf));
    }
    buf = "";
  };
  for (const ch of snippet) {
    if (ch === HL_START) {
      flush();
      on = true;
    } else if (ch === HL_END) {
      flush();
      on = false;
    } else {
      buf += ch;
    }
  }
  flush();
  return frag;
}

function plain(snippet: string): string {
  return snippet.split(HL_START).join("").split(HL_END).join("");
}

// ---------------------------------------------------------------- state

type SaveState = "saved" | "saving" | "conflict";

const app = {
  vault: null as VaultInfo | null,
  path: null as string | null,
  nav: 0, // bumps on every navigation; stale page loads are dropped
  revision: "",
  version: 0, // bumps on every edit
  savedVersion: 0,
  state: "saved" as SaveState,
  timer: 0,
  saving: null as Promise<void> | null,
  openFolders: new Set<string>(),
  editor: null as EditorHandle | null,
  loading: false,
};

// ---------------------------------------------------------------- notices

function showBanner(text: string, action?: { label: string; run: () => void }): void {
  $("banner-text").textContent = text;
  const btn = $<HTMLButtonElement>("banner-action");
  btn.hidden = !action;
  btn.onclick = action ? action.run : null;
  if (action) btn.textContent = action.label;
  $("banner").hidden = false;
}

function hideBanner(): void {
  $("banner").hidden = true;
}

function setStatus(state: SaveState): void {
  app.state = state;
  $("status-state").textContent =
    state === "saved" ? "Saved" : state === "saving" ? "Saving…" : "Conflict";
  $("status-state").dataset.state = state;
}

// ---------------------------------------------------------------- pages

async function flushSave(): Promise<void> {
  window.clearTimeout(app.timer);
  if (app.saving) await app.saving;
  if (!app.path || !app.editor || app.state === "conflict") return;
  if (app.version === app.savedVersion) return;
  const path = app.path;
  const version = app.version;
  const text = app.editor.getText();
  app.saving = (async () => {
    try {
      const res = await api.savePage(path, text, app.revision);
      if (app.path === path) {
        app.revision = res.revision;
        app.savedVersion = version;
        setStatus(app.version === version ? "saved" : "saving");
      }
    } catch (err) {
      const msg = errorText(err);
      if (app.path === path && msg.startsWith("CONFLICT:")) {
        setStatus("conflict");
        showBanner("Changed on disk —", { label: "Reload", run: () => void reloadPage() });
      } else {
        showBanner(`Could not save: ${msg}`);
      }
    }
  })();
  await app.saving;
  app.saving = null;
  if (app.path === path && app.state === "saving" && app.version !== app.savedVersion) {
    scheduleSave();
  }
}

function scheduleSave(): void {
  window.clearTimeout(app.timer);
  app.timer = window.setTimeout(() => void flushSave(), AUTOSAVE_MS);
}

function onEdit(): void {
  if (app.loading || !app.path) return;
  app.version += 1;
  if (app.state === "conflict") return;
  setStatus("saving");
  scheduleSave();
}

async function reloadPage(): Promise<void> {
  if (!app.path) return;
  await loadPage(app.path, true);
}

async function loadPage(path: string, force = false): Promise<void> {
  if (!force && app.path === path) {
    app.editor?.focus();
    return;
  }
  const token = ++app.nav;
  await flushSave();
  try {
    const page = await api.readPage(path);
    if (token !== app.nav) return; // a newer navigation won
    app.loading = true;
    app.path = path;
    app.revision = page.revision;
    app.version = 0;
    app.savedVersion = 0;
    app.editor?.setText(page.text);
    app.loading = false;
    setStatus("saved");
    hideBanner();
    $("empty").hidden = true;
    $("editor").hidden = false;
    $("status-path").textContent = path;
    renderTree();
    await refreshLinks();
    app.editor?.focus();
  } catch (err) {
    app.loading = false;
    showBanner(`Could not open ${path}: ${errorText(err)}`);
  }
}

async function openTarget(target: string): Promise<void> {
  try {
    let path = await api.resolve(target);
    if (!path) path = await api.createPageAt(target);
    await loadPage(path);
    await refreshTree();
  } catch (err) {
    showBanner(`Could not open link: ${errorText(err)}`);
  }
}

// ---------------------------------------------------------------- tree

let treeData: TreeNode | null = null;

async function refreshTree(): Promise<void> {
  try {
    treeData = await api.tree();
    renderTree();
  } catch (err) {
    showBanner(`Could not load files: ${errorText(err)}`);
  }
}

function renderTree(): void {
  const host = $("tree");
  host.replaceChildren();
  if (!treeData) return;
  host.append(renderChildren(treeData, 0));
}

function renderChildren(node: TreeNode, depth: number): HTMLUListElement {
  const ul = h("ul", { class: "tree-list" });
  ul.setAttribute("role", depth === 0 ? "tree" : "group");
  for (const child of node.children) {
    const li = h("li");
    li.setAttribute("role", "none");
    const label = h("span", { class: "tree-label", text: child.name });
    const btn = h("button", { type: "button", class: "tree-item" }, label);
    btn.style.paddingLeft = `${8 + depth * 14}px`;
    btn.setAttribute("role", "treeitem");
    if (child.is_dir) {
      const open = app.openFolders.has(child.path);
      btn.classList.add("folder");
      btn.setAttribute("aria-expanded", String(open));
      btn.prepend(h("span", { class: "twisty", text: open ? "▾" : "▸" }));
      btn.addEventListener("click", () => {
        if (app.openFolders.has(child.path)) app.openFolders.delete(child.path);
        else app.openFolders.add(child.path);
        renderTree();
      });
      li.append(btn);
      if (open) li.append(renderChildren(child, depth + 1));
    } else if (child.name.toLowerCase().endsWith(".md")) {
      btn.prepend(h("span", { class: "twisty", text: "" }));
      if (child.path === app.path) btn.setAttribute("aria-current", "page");
      btn.addEventListener("click", () => void loadPage(child.path));
      li.append(btn);
    } else {
      btn.disabled = true;
      btn.classList.add("other");
      btn.prepend(h("span", { class: "twisty", text: "" }));
      li.append(btn);
    }
    ul.append(li);
  }
  return ul;
}

async function refreshTags(): Promise<void> {
  try {
    const tags = await api.tags();
    const host = $("tags");
    host.replaceChildren();
    if (tags.length === 0) {
      host.append(h("p", { class: "muted small", text: "No tags yet." }));
    }
    for (const t of tags) {
      const b = h("button", { type: "button", class: "tag-chip", text: `#${t.tag}` });
      b.append(h("span", { class: "tag-count", text: String(t.count) }));
      b.setAttribute("aria-label", `Tag ${t.tag}, ${t.count} pages. Search for it.`);
      b.addEventListener("click", () => openDialog("search", `tag:${t.tag}`));
      host.append(b);
    }
  } catch {
    // tags are non-essential
  }
}

// ---------------------------------------------------------------- link panel

let panelToken = 0;

async function refreshLinks(): Promise<void> {
  const token = ++panelToken;
  const back = $("list-back");
  const out = $("list-out");
  if (!app.path) {
    back.replaceChildren();
    out.replaceChildren();
    return;
  }
  try {
    const [bl, ol] = await Promise.all([api.backlinks(app.path), api.outgoing(app.path)]);
    if (token !== panelToken) return;
    back.replaceChildren();
    if (bl.length === 0) back.append(h("p", { class: "muted small pad", text: "Nothing links here yet." }));
    for (const b of bl) {
      const item = h(
        "button",
        { type: "button", class: "link-item" },
        h("span", { class: "link-title", text: b.source_title }),
        h("span", { class: "link-ctx", text: `Line ${b.line}: ${b.context}` }),
      );
      item.addEventListener("click", () => void loadPage(b.source));
      back.append(item);
    }
    out.replaceChildren();
    if (ol.length === 0) out.append(h("p", { class: "muted small pad", text: "This page has no links." }));
    for (const o of ol) {
      const item = h(
        "button",
        { type: "button", class: o.resolved ? "link-item" : "link-item unresolved" },
        h("span", { class: "link-title", text: o.target }),
        h("span", {
          class: "link-ctx",
          text: o.resolved ? `${o.resolved} · line ${o.line}` : `Not created yet · line ${o.line}`,
        }),
      );
      item.addEventListener("click", () => void openTarget(o.resolved ?? o.target));
      out.append(item);
    }
  } catch {
    // panel is best-effort
  }
}

function selectTab(which: "back" | "out"): void {
  const back = which === "back";
  $("tab-back").setAttribute("aria-selected", String(back));
  $("tab-out").setAttribute("aria-selected", String(!back));
  $("tab-back").tabIndex = back ? 0 : -1;
  $("tab-out").tabIndex = back ? -1 : 0;
  $("list-back").hidden = !back;
  $("list-out").hidden = back;
}

// ---------------------------------------------------------------- dialog (search / switcher / new page)

type DialogMode = "search" | "switch" | "new";

const dlg = {
  mode: "search" as DialogMode,
  results: [] as { path: string; title: string; snippet: string }[],
  active: 0,
  timer: 0,
  token: 0,
  returnFocus: null as HTMLElement | null,
};

function openDialog(mode: DialogMode, initial = ""): void {
  dlg.mode = mode;
  dlg.returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  $("dialog-title").textContent =
    mode === "search" ? "Search" : mode === "switch" ? "Go to page" : "New page";
  $("dialog-hint").textContent =
    mode === "new"
      ? "Type a title and press Enter. Use Folder/Title to place it in a folder."
      : "Up and Down to choose, Enter to open, Esc to close.";
  const input = $<HTMLInputElement>("dialog-input");
  input.value = initial;
  input.setAttribute("aria-label", $("dialog-title").textContent ?? "");
  $("dialog-results").hidden = mode === "new";
  $("overlay").hidden = false;
  dlg.results = [];
  dlg.active = 0;
  renderResults();
  input.focus();
  input.select();
  if (mode !== "new" && initial) void runDialogSearch();
}

function closeDialog(): void {
  $("overlay").hidden = true;
  window.clearTimeout(dlg.timer);
  dlg.token++;
  dlg.returnFocus?.focus();
}

function renderResults(): void {
  const list = $("dialog-results");
  list.replaceChildren();
  const input = $<HTMLInputElement>("dialog-input");
  dlg.results.forEach((r, i) => {
    const li = h("li", { class: "result" });
    li.id = `result-${i}`;
    li.setAttribute("role", "option");
    li.setAttribute("aria-selected", String(i === dlg.active));
    li.append(h("span", { class: "result-title", text: r.title }));
    li.append(h("span", { class: "result-path", text: r.path }));
    if (dlg.mode === "search" && r.snippet) {
      const sn = h("span", { class: "result-snippet" });
      sn.append(highlighted(r.snippet));
      sn.setAttribute("aria-label", plain(r.snippet));
      li.append(sn);
    }
    li.addEventListener("mousedown", (e) => e.preventDefault());
    li.addEventListener("click", () => chooseResult(i));
    list.append(li);
  });
  if (dlg.results.length > 0) input.setAttribute("aria-activedescendant", `result-${dlg.active}`);
  else input.removeAttribute("aria-activedescendant");
  document.getElementById(`result-${dlg.active}`)?.scrollIntoView({ block: "nearest" });
}

function titleQuery(raw: string): string {
  return raw
    .split(/\s+/)
    .filter(Boolean)
    .map((w) => `title:${w}`)
    .join(" ");
}

async function runDialogSearch(): Promise<void> {
  const raw = $<HTMLInputElement>("dialog-input").value.trim();
  const token = ++dlg.token;
  if (!raw) {
    dlg.results = [];
    renderResults();
    return;
  }
  try {
    const q = dlg.mode === "switch" ? titleQuery(raw) : raw;
    const hits = await api.search(q, dlg.mode === "switch" ? 12 : 30);
    if (token !== dlg.token) return;
    dlg.results = hits;
    dlg.active = 0;
    renderResults();
  } catch (err) {
    if (token !== dlg.token) return;
    dlg.results = [];
    renderResults();
    $("dialog-hint").textContent = `Search failed: ${errorText(err)}`;
  }
}

function chooseResult(i: number): void {
  const r = dlg.results[i];
  if (!r) return;
  closeDialog();
  void loadPage(r.path);
}

async function submitNewPage(): Promise<void> {
  const raw = $<HTMLInputElement>("dialog-input").value.trim();
  if (!raw) return;
  const slash = raw.lastIndexOf("/");
  const folder = slash > 0 ? raw.slice(0, slash) : undefined;
  const title = slash >= 0 ? raw.slice(slash + 1) : raw;
  try {
    const path = await api.createPage(title, folder);
    closeDialog();
    await refreshTree();
    await loadPage(path);
  } catch (err) {
    $("dialog-hint").textContent = `Could not create page: ${errorText(err)}`;
  }
}

function wireDialog(): void {
  const input = $<HTMLInputElement>("dialog-input");
  input.addEventListener("input", () => {
    if (dlg.mode === "new") return;
    window.clearTimeout(dlg.timer);
    dlg.timer = window.setTimeout(() => void runDialogSearch(), SEARCH_DEBOUNCE_MS);
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      closeDialog();
    } else if (e.key === "ArrowDown" && dlg.results.length > 0) {
      e.preventDefault();
      dlg.active = (dlg.active + 1) % dlg.results.length;
      renderResults();
    } else if (e.key === "ArrowUp" && dlg.results.length > 0) {
      e.preventDefault();
      dlg.active = (dlg.active - 1 + dlg.results.length) % dlg.results.length;
      renderResults();
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (dlg.mode === "new") void submitNewPage();
      else chooseResult(dlg.active);
    }
  });
  $("overlay").addEventListener("mousedown", (e) => {
    if (e.target === $("overlay")) closeDialog();
  });
}

// ---------------------------------------------------------------- app lifecycle

async function enterVault(info: VaultInfo): Promise<void> {
  app.vault = info;
  app.path = null;
  app.openFolders.clear();
  $("welcome").hidden = true;
  $("app").hidden = false;
  $("vault-name").textContent = info.name || info.root;
  $("vault-name").title = info.root;
  $("mode").textContent = info.mode_label;
  $("status-path").textContent = "";
  $("empty").hidden = false;
  $("editor").hidden = true;
  setStatus("saved");
  hideBanner();
  if (!app.editor) {
    app.editor = createEditor($("editor"), {
      onChange: onEdit,
      onLinkClick: (t) => void openTarget(t),
    });
  }
  await Promise.all([refreshTree(), refreshTags(), refreshLinks()]);
  const first = treeData?.children.find((c) => !c.is_dir && c.name.endsWith(".md"));
  if (first && !app.path) await loadPage(first.path);
}

async function showWelcome(): Promise<void> {
  $("app").hidden = true;
  $("welcome").hidden = false;
  const last = await api.lastVault().catch(() => null);
  $("recent").hidden = !last;
  if (last) {
    $("btn-recent").textContent = last;
    $<HTMLInputElement>("folder-path").value = last;
  }
  $("folder-path").focus();
}

function welcomeError(msg: string): void {
  const el = $("welcome-error");
  el.textContent = msg;
  el.hidden = !msg;
}

async function welcomeAction(kind: "open" | "create", path: string): Promise<void> {
  const p = path.trim();
  if (!p) {
    welcomeError("Enter the full path of a folder first.");
    return;
  }
  welcomeError("");
  try {
    const info = kind === "open" ? await api.openVault(p) : await api.createVault(p);
    await enterVault(info);
  } catch (err) {
    welcomeError(errorText(err));
  }
}

async function onWindowFocus(): Promise<void> {
  if (!app.vault || $("app").hidden) return;
  try {
    await api.refresh();
    await Promise.all([refreshTree(), refreshTags(), refreshLinks()]);
    // Pick up outside edits when nothing of ours is pending.
    if (app.path && app.state === "saved" && app.version === app.savedVersion && !app.saving) {
      const page = await api.readPage(app.path);
      if (page.revision !== app.revision && app.version === app.savedVersion) {
        app.loading = true;
        app.revision = page.revision;
        app.editor?.setText(page.text);
        app.loading = false;
      }
    }
  } catch {
    // a failed refresh must not disturb editing
  }
}

function wireShortcuts(): void {
  document.addEventListener("keydown", (e) => {
    if ($("app").hidden) return;
    const mod = e.ctrlKey || e.metaKey;
    if (!mod) return;
    const key = e.key.toLowerCase();
    if (key === "f" && e.shiftKey) {
      e.preventDefault();
      openDialog("search");
    } else if (key === "o" && !e.shiftKey) {
      e.preventDefault();
      openDialog("switch");
    } else if (key === "n" && !e.shiftKey) {
      e.preventDefault();
      openDialog("new");
    }
  });
}

function wireTabs(): void {
  const tabs = [$("tab-back"), $("tab-out")];
  tabs[0]?.addEventListener("click", () => selectTab("back"));
  tabs[1]?.addEventListener("click", () => selectTab("out"));
  for (const t of tabs) {
    t.addEventListener("keydown", (e) => {
      if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
        const next = t === tabs[0] ? "out" : "back";
        selectTab(next);
        $(next === "back" ? "tab-back" : "tab-out").focus();
      }
    });
  }
}

async function init(): Promise<void> {
  try {
    const info = await api.appInfo();
    document.title = info.name;
    $("welcome-title").textContent = info.name;
  } catch (err) {
    $("welcome-title").textContent = "Unable to load app info";
    welcomeError(errorText(err));
  }

  $("btn-open-folder").addEventListener("click", () =>
    void welcomeAction("open", $<HTMLInputElement>("folder-path").value),
  );
  $("btn-create-vault").addEventListener("click", () =>
    void welcomeAction("create", $<HTMLInputElement>("folder-path").value),
  );
  $("btn-recent").addEventListener("click", () =>
    void welcomeAction("open", $("btn-recent").textContent ?? ""),
  );
  $("folder-path").addEventListener("keydown", (e) => {
    if (e.key === "Enter") void welcomeAction("open", $<HTMLInputElement>("folder-path").value);
  });
  $("btn-new").addEventListener("click", () => openDialog("new"));
  $("btn-search").addEventListener("click", () => openDialog("search"));
  $("btn-today").addEventListener("click", () => {
    void (async () => {
      try {
        const path = await api.dailyNote();
        await refreshTree();
        await loadPage(path);
      } catch (err) {
        showBanner(`Could not open today's note: ${errorText(err)}`);
      }
    })();
  });
  $("banner-close").addEventListener("click", hideBanner);
  wireDialog();
  wireTabs();
  wireShortcuts();
  window.addEventListener("focus", () => void onWindowFocus());

  try {
    const info = await api.vaultInfo();
    await enterVault(info);
  } catch {
    await showWelcome();
  }
}

void init();
