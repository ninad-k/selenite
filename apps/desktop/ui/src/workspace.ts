// Workspace mode shell: the page tree, the block editor and the slash menu. Presentation only:
// every edit is a Tauri command and the page is re-read from Rust after it. Vault text is written
// with textContent; the single innerHTML use is RenderedBlock.html, which Rust has sanitised.

import {
  api,
  errorText,
  type BlockPage,
  type BlockType,
  type PageNode,
  type RenderedBlock,
  type ViewResult,
} from "./api";
import { $, button, h, popupMenu, promptText } from "./dom";
import { closeDatabase, initDatabase, openDatabase, propertyCell } from "./database";
import { createBlockEditor, type BlockEditorHandle, type SlashKey } from "./editor";

export interface WorkspaceHost {
  banner(text: string, action?: { label: string; run: () => void }): void;
  openTasks(): void;
  flush(): Promise<void>;
}

interface Shift {
  at: number;
  delta: number;
}

interface Edit {
  target: number | "pending";
  /** For a pending block: insert after this index (null = at the start). */
  after: number | null;
  ed: BlockEditorHandle;
  original: string;
  slash: Slash | null;
}

interface Slash {
  menu: HTMLElement;
  items: BlockType[];
  active: number;
}

const ws = {
  host: null as WorkspaceHost | null,
  active: false,
  tree: [] as PageNode[],
  open: new Set<string>(),
  current: null as { kind: "page" | "database"; path: string } | null,
  page: null as BlockPage | null,
  row: null as { folder: string; view: ViewResult } | null,
  types: [] as BlockType[],
  editing: null as Edit | null,
  commitP: null as Promise<Shift | null> | null,
  dragIdx: -1,
  token: 0,
};

const BARE_MARKER = /^(#{1,6}|[-*+]|\d+\.|- \[[ x]\]|>)\s*$/;

function banner(text: string, action?: { label: string; run: () => void }): void {
  ws.host?.banner(text, action);
}

// ---------------------------------------------------------------- tree

async function refreshTree(): Promise<void> {
  try {
    ws.tree = await api.workspacePages();
    renderTree();
  } catch (err) {
    banner(`Could not load pages: ${errorText(err)}`);
  }
}

function nodeKey(n: PageNode): string {
  return n.path;
}

function renderTree(): void {
  const host = $("ws-tree");
  const hadFocus = host.contains(document.activeElement);
  const focusKey = hadFocus ? (document.activeElement as HTMLElement).dataset.key : undefined;
  host.replaceChildren();
  if (ws.tree.length === 0) {
    host.append(h("p", { class: "muted small pad", text: "No pages yet. Use New page to start." }));
    return;
  }
  host.append(renderNodes(ws.tree, 0));
  if (focusKey) host.querySelector<HTMLElement>(`[data-key="${CSS.escape(focusKey)}"]`)?.focus();
}

function nodeGlyph(n: PageNode): string {
  if (n.icon) return n.icon;
  return n.kind === "database" ? "▦" : n.kind === "tasks" ? "☑" : n.kind === "folder" ? "▸" : "▫";
}

function renderNodes(nodes: PageNode[], depth: number): HTMLUListElement {
  const ul = h("ul", { class: "tree-list", role: depth === 0 ? "tree" : "group" });
  for (const n of nodes) {
    const li = h("li", { role: "none" });
    const expandable = n.children.length > 0;
    const isOpen = ws.open.has(nodeKey(n));
    const row = h("div", { class: "ws-node" });
    row.style.paddingLeft = `${4 + depth * 14}px`;
    const twisty = h("button", {
      type: "button",
      class: "ws-twisty",
      text: expandable ? (isOpen ? "▾" : "▸") : "",
      label: expandable ? `${isOpen ? "Collapse" : "Expand"} ${n.name}` : "",
    });
    twisty.tabIndex = -1;
    twisty.hidden = !expandable;
    twisty.addEventListener("click", () => toggleNode(n));
    const label = h(
      "button",
      { type: "button", class: "ws-label" },
      h("span", { class: "ws-glyph", text: nodeGlyph(n) }),
      h("span", { class: "ws-name", text: n.name }),
    );
    label.dataset.key = nodeKey(n);
    label.setAttribute("role", "treeitem");
    if (expandable) label.setAttribute("aria-expanded", String(isOpen));
    const cur = ws.current;
    if (cur && cur.path === n.path) label.setAttribute("aria-current", "page");
    label.addEventListener("click", () => activateNode(n));
    row.append(twisty, label);
    if (n.kind === "page") {
      const add = h("button", {
        type: "button",
        class: "ws-add",
        text: "+",
        label: `Add a sub-page to ${n.name}`,
        title: "Add a sub-page",
      });
      add.addEventListener("click", () => void newPage(n.path));
      row.append(add);
    }
    li.append(row);
    if (expandable && isOpen) li.append(renderNodes(n.children, depth + 1));
    ul.append(li);
  }
  return ul;
}

function toggleNode(n: PageNode): void {
  const k = nodeKey(n);
  if (ws.open.has(k)) ws.open.delete(k);
  else ws.open.add(k);
  renderTree();
}

function activateNode(n: PageNode): void {
  if (n.kind === "page") void openPage(n.path);
  else if (n.kind === "database") void openDb(n.path);
  else if (n.kind === "tasks") ws.host?.openTasks();
  else toggleNode(n);
}

function wireTreeKeys(): void {
  $("ws-tree").addEventListener("keydown", (e) => {
    const t = e.target as HTMLElement;
    if (!t.classList.contains("ws-label")) return;
    const labels = [...$("ws-tree").querySelectorAll<HTMLElement>(".ws-label")];
    const i = labels.indexOf(t);
    const find = (key: string): PageNode | undefined => {
      const walk = (ns: PageNode[]): PageNode | undefined => {
        for (const n of ns) {
          if (n.path === key) return n;
          const r = walk(n.children);
          if (r) return r;
        }
        return undefined;
      };
      return walk(ws.tree);
    };
    const node = t.dataset.key ? find(t.dataset.key) : undefined;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      labels[Math.min(i + 1, labels.length - 1)]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      labels[Math.max(i - 1, 0)]?.focus();
    } else if (e.key === "ArrowRight" && node && node.children.length > 0) {
      e.preventDefault();
      if (!ws.open.has(node.path)) toggleNode(node);
    } else if (e.key === "ArrowLeft" && node && node.children.length > 0) {
      e.preventDefault();
      if (ws.open.has(node.path)) toggleNode(node);
    }
  });
}

async function newPage(parent: string | null): Promise<void> {
  const title = await promptText(
    parent ? "New sub-page" : "New page",
    "Title",
    "",
    parent ? `It will live inside ${parent.replace(/\.md$/i, "")}.` : "",
  );
  if (!title || !title.trim()) return;
  try {
    const path = await api.createSubpage(parent, title);
    if (parent) ws.open.add(parent);
    await refreshTree();
    await openPage(path);
  } catch (err) {
    banner(`Could not create page: ${errorText(err)}`);
  }
}

async function newDatabase(): Promise<void> {
  const name = await promptText(
    "New database",
    "Name",
    "",
    "A folder with a schema file and one Markdown file per row.",
  );
  if (!name || !name.trim()) return;
  try {
    const folder = name.trim();
    await api.databaseCreate(folder, []);
    await refreshTree();
    await openDb(folder);
  } catch (err) {
    banner(`Could not create database: ${errorText(err)}`);
  }
}

function expandAncestors(path: string): void {
  const segs = path.split("/");
  segs.pop();
  let acc = "";
  for (const s of segs) {
    acc = acc ? `${acc}/${s}` : s;
    ws.open.add(`${acc}.md`);
    ws.open.add(acc);
  }
}

// ---------------------------------------------------------------- showing things

function showPanel(which: "page" | "db" | "empty"): void {
  $("ws-page").hidden = which !== "page";
  $("ws-db").hidden = which !== "db";
  $("ws-empty").hidden = which !== "empty";
}

async function openDb(folder: string): Promise<void> {
  await commitEditing();
  ws.token++;
  ws.current = { kind: "database", path: folder };
  ws.page = null;
  ws.row = null;
  expandAncestors(folder);
  showPanel("db");
  renderTree();
  await openDatabase(folder, $("ws-db"));
}

async function openPage(path: string): Promise<void> {
  await commitEditing();
  const token = ++ws.token;
  try {
    const page = await api.pageBlocks(path);
    if (token !== ws.token) return;
    closeDatabase();
    ws.current = { kind: "page", path };
    ws.page = page;
    ws.row = await rowContext(path);
    if (token !== ws.token) return;
    expandAncestors(path);
    showPanel("page");
    renderPage();
    renderTree();
  } catch (err) {
    banner(`Could not open ${path}: ${errorText(err)}`);
  }
}

/** If the page is a row of a table database, load that database's view for its properties. */
async function rowContext(path: string): Promise<{ folder: string; view: ViewResult } | null> {
  const cut = path.lastIndexOf("/");
  if (cut < 0) return null;
  const folder = path.slice(0, cut);
  try {
    const dbs = await api.databases();
    if (!dbs.some((d) => d.folder === folder && d.kind === "table")) return null;
    const view = await api.databaseView(folder, null, null);
    return view.rows.some((r) => r.path === path) ? { folder, view } : null;
  } catch {
    return null;
  }
}

async function reloadPage(): Promise<void> {
  const cur = ws.current;
  if (!cur || cur.kind !== "page") return;
  const panel = $("workspace-panel");
  const top = panel.scrollTop;
  const page = await api.pageBlocks(cur.path);
  ws.page = page;
  ws.row = await rowContext(cur.path);
  renderPage();
  panel.scrollTop = top;
}

// ---------------------------------------------------------------- page view

function hasTitleBlock(page: BlockPage): boolean {
  const b = page.blocks[0];
  return !!b && b.kind === "heading" && /^# /.test(b.markdown);
}

function renderPage(): void {
  const page = ws.page;
  const host = $("ws-page");
  host.replaceChildren();
  if (!page) return;
  closeSlashMenu();

  if (page.cover) {
    const img = h("img", { class: "ws-cover-img" });
    img.src = page.cover;
    img.alt = "";
    const bar = h(
      "div",
      { class: "ws-cover-actions" },
      button("Change cover", () => void editCover(), "small-btn"),
      button("Remove cover", () => void setMeta(undefined, ""), "small-btn"),
    );
    host.append(h("div", { class: "ws-cover" }, img, bar));
  }

  const head = h("div", { class: "ws-head" });
  const crumbDb = ws.row?.folder;
  if (crumbDb) {
    head.append(button(`← ${crumbDb}`, () => void openDb(crumbDb), "crumb", `Back to database ${crumbDb}`));
  }
  const iconBtn = h("button", {
    type: "button",
    class: page.icon ? "ws-icon" : "ws-icon empty",
    text: page.icon ?? "＋",
    label: page.icon ? `Page icon ${page.icon}. Change it.` : "Add a page icon",
    title: "Set icon",
  });
  iconBtn.addEventListener("click", () => void editIcon());
  const title = h("input", { type: "text", class: "ws-title", label: "Page title" });
  title.value = page.title;
  title.readOnly = !hasTitleBlock(page);
  title.addEventListener("change", () => void renameTitle(title.value));
  title.addEventListener("keydown", (e) => {
    if (e.key === "Enter") title.blur();
  });
  const tools = h("div", { class: "ws-tools" });
  if (!page.cover) tools.append(button("Add cover", () => void editCover(), "small-btn"));
  head.append(iconBtn, title, tools);
  host.append(head);

  if (ws.row) host.append(renderRowProps(ws.row, page.path));

  const blocks = h("div", { class: "ws-blocks", role: "list", label: "Blocks" });
  blocks.id = "ws-blocks";
  wireBlocks(blocks);
  host.append(blocks);
  renderBlocks();
  const add = h("button", { type: "button", class: "ws-add-block", text: "＋ Add a block" });
  add.addEventListener("click", () => void startPending(page.blocks.length - 1));
  host.append(add);
}

function renderRowProps(
  ctx: { folder: string; view: ViewResult },
  path: string,
): HTMLElement {
  const row = ctx.view.rows.find((r) => r.path === path);
  const box = h("section", { class: "row-props", label: "Properties" });
  if (!row) return box;
  for (const p of ctx.view.properties) {
    if (p.type === "title") continue;
    const id = `rp-${p.name.replace(/\W+/g, "_")}`;
    const lab = h("label", { class: "row-prop-label", text: p.name });
    lab.htmlFor = id;
    const cell = propertyCell(p, row, ctx.view, (name, value) => {
      void (async () => {
        try {
          await api.rowSet(path, name, value);
          await reloadPage();
        } catch (err) {
          banner(`Could not save ${name}: ${errorText(err)}`);
        }
      })();
    });
    const control = cell.querySelector<HTMLElement>("input, select");
    if (control) control.id = id;
    box.append(h("div", { class: "row-prop" }, lab, cell));
  }
  return box;
}

async function setMeta(icon?: string, cover?: string): Promise<void> {
  const cur = ws.current;
  if (!cur || cur.kind !== "page") return;
  try {
    await commitEditing();
    await api.setPageMeta(cur.path, icon, cover);
    await reloadPage();
    await refreshTree();
  } catch (err) {
    handleError(err, "Could not update the page");
  }
}

async function editIcon(): Promise<void> {
  const v = await promptText(
    "Page icon",
    "Emoji or short text (empty removes the icon)",
    ws.page?.icon ?? "",
  );
  if (v === null) return;
  await setMeta(v.trim(), undefined);
}

async function editCover(): Promise<void> {
  const v = await promptText(
    "Cover image",
    "Image file in the vault, or a web address (empty removes the cover)",
    "",
    "For example cover.png or Pictures/lake.jpg.",
  );
  if (v === null) return;
  await setMeta(undefined, v.trim());
}

async function renameTitle(value: string): Promise<void> {
  const page = ws.page;
  const v = value.trim();
  if (!page || !hasTitleBlock(page) || !v || v === page.title) return;
  await runEdit({ op: "replace", block: "0", markdown: `# ${v}` });
  await refreshTree();
}

// ---------------------------------------------------------------- blocks

function wireBlocks(container: HTMLElement): void {
  container.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    const t = e.target as HTMLElement;
    const body = t.closest<HTMLElement>(".block-body");
    if (!body || t.closest(".block-editing")) return;
    const idx = Number(body.parentElement?.dataset.index);
    if (Number.isNaN(idx)) return;
    const cb = t.closest<HTMLInputElement>('input[type="checkbox"]');
    if (cb) {
      e.preventDefault();
      void toggleTask(idx, Number(cb.dataset.line ?? 0));
      return;
    }
    const a = t.closest<HTMLAnchorElement>("a");
    if (a) {
      e.preventDefault();
      void followAnchor(a);
      return;
    }
    const callout = t.closest<HTMLElement>(".callout");
    if (callout && t.closest(".callout-title") && callout.dataset.fold !== "none") {
      e.preventDefault();
      toggleCallout(callout);
      return;
    }
    e.preventDefault();
    void startEdit(idx);
  });
  container.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    if (t.closest("a")) e.preventDefault();
    const cb = t.closest<HTMLInputElement>('input[type="checkbox"]');
    if (cb && e instanceof MouseEvent && e.detail === 0) {
      // Keyboard activation (Space); a mouse click was handled on mousedown.
      e.preventDefault();
      const idx = Number(cb.closest<HTMLElement>(".block")?.dataset.index);
      void toggleTask(idx, Number(cb.dataset.line ?? 0));
    }
  });
  container.addEventListener("keydown", (e) => {
    const t = e.target as HTMLElement;
    if (e.key === "Enter" && t.classList.contains("block-body")) {
      e.preventDefault();
      const idx = Number(t.parentElement?.dataset.index);
      if (!Number.isNaN(idx)) void startEdit(idx);
    }
  });
  // Drag and drop reordering.
  container.addEventListener("dragstart", (e) => {
    const handle = (e.target as HTMLElement).closest<HTMLElement>(".block-handle");
    const block = handle?.closest<HTMLElement>(".block");
    if (!handle || !block) return;
    ws.dragIdx = Number(block.dataset.index);
    block.classList.add("dragging");
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", String(ws.dragIdx));
    }
  });
  container.addEventListener("dragover", (e) => {
    if (ws.dragIdx < 0) return;
    e.preventDefault();
    clearDropMarks();
    const block = (e.target as HTMLElement).closest<HTMLElement>(".block");
    if (block) block.classList.add(dropAfter(block, e.clientY) ? "drop-after" : "drop-before");
  });
  container.addEventListener("drop", (e) => {
    if (ws.dragIdx < 0) return;
    e.preventDefault();
    const from = ws.dragIdx;
    const block = (e.target as HTMLElement).closest<HTMLElement>(".block");
    ws.dragIdx = -1;
    clearDropMarks();
    if (!block) return;
    const j = Number(block.dataset.index);
    const raw = dropAfter(block, e.clientY) ? j + 1 : j;
    const to = raw > from ? raw - 1 : raw;
    if (to !== from) void moveBlock(from, to);
  });
  container.addEventListener("dragend", () => {
    ws.dragIdx = -1;
    clearDropMarks();
    container.querySelector(".dragging")?.classList.remove("dragging");
  });
}

function dropAfter(block: HTMLElement, y: number): boolean {
  const r = block.getBoundingClientRect();
  return y > r.top + r.height / 2;
}

function clearDropMarks(): void {
  for (const el of document.querySelectorAll(".drop-before, .drop-after")) {
    el.classList.remove("drop-before", "drop-after");
  }
}

function toggleCallout(callout: HTMLElement): void {
  const collapsed = callout.classList.toggle("collapsed");
  callout.setAttribute("aria-expanded", String(!collapsed));
}

function prepareBlockHtml(body: HTMLElement): void {
  for (const cb of body.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')) {
    cb.disabled = false;
    cb.setAttribute("aria-label", "Task done");
  }
  for (const c of body.querySelectorAll<HTMLElement>(".callout")) {
    if (c.dataset.fold === "collapsed") c.classList.add("collapsed");
    if (c.dataset.fold !== "none") {
      c.setAttribute("aria-expanded", String(!c.classList.contains("collapsed")));
      c.querySelector<HTMLElement>(".callout-title")?.setAttribute("role", "button");
    }
  }
}

function renderBlocks(): void {
  const page = ws.page;
  const host = document.getElementById("ws-blocks");
  if (!page || !host) return;
  host.replaceChildren();
  const skipTitle = hasTitleBlock(page);
  const pending = ws.editing && ws.editing.target === "pending" ? ws.editing : null;
  if (pending && pending.after === null) host.append(pendingNode());
  for (const b of page.blocks) {
    if (b.index === 0 && skipTitle) {
      if (pending && pending.after === 0) host.append(pendingNode());
      continue;
    }
    host.append(blockNode(b));
    if (pending && pending.after === b.index) host.append(pendingNode());
  }
  const edit = ws.editing;
  if (edit && edit.target !== "pending") {
    const el = host.querySelector<HTMLElement>(`.block[data-index="${edit.target}"] .block-body`);
    if (el) mountEditor(el, edit);
  } else if (pending) {
    const el = host.querySelector<HTMLElement>(".block.pending .block-body");
    if (el) mountEditor(el, pending);
  }
}

function blockNode(b: RenderedBlock): HTMLElement {
  const el = h("div", { class: `block kind-${b.kind}`, role: "listitem" });
  el.dataset.index = String(b.index);
  const gutter = h("div", { class: "block-gutter" });
  const add = h("button", {
    type: "button",
    class: "block-add",
    text: "+",
    label: `Add a block below block ${b.index + 1}`,
    title: "Add a block below",
  });
  add.addEventListener("click", () => void startPending(b.index, true));
  const handle = h("button", {
    type: "button",
    class: "block-handle",
    text: "⋮⋮",
    label: `Block ${b.index + 1} menu. Drag to reorder.`,
    title: "Drag to reorder, click for menu",
  });
  handle.draggable = true;
  handle.addEventListener("click", () => blockMenu(handle, b.index));
  gutter.append(add, handle);
  const body = h("div", { class: "block-body" });
  body.tabIndex = 0;
  body.setAttribute("aria-label", `Block ${b.index + 1}, ${b.kind}. Press Enter to edit.`);
  body.innerHTML = b.html; // Rust sanitises RenderedBlock.html; this is the only innerHTML use.
  if (!b.html.trim()) body.classList.add("block-empty");
  prepareBlockHtml(body);
  el.append(gutter, body);
  return el;
}

function pendingNode(): HTMLElement {
  const el = h("div", { class: "block pending", role: "listitem" });
  el.append(h("div", { class: "block-gutter" }), h("div", { class: "block-body" }));
  return el;
}

function blockMenu(anchor: HTMLElement, idx: number): void {
  const page = ws.page;
  if (!page) return;
  const min = hasTitleBlock(page) ? 1 : 0;
  const last = page.blocks.length - 1;
  const items = [
    { label: "Add block below", run: () => void startPending(idx, true) },
    { label: "Move up", run: () => idx > min && void moveBlock(idx, idx - 1) },
    { label: "Move down", run: () => idx < last && void moveBlock(idx, idx + 1) },
    { label: "Delete", run: () => void runEdit({ op: "delete", block: String(idx) }) },
  ];
  popupMenu(anchor, items, `Block ${idx + 1} actions`);
}

async function moveBlock(from: number, to: number): Promise<void> {
  await commitEditing();
  await runEdit({ op: "move", block: String(from), to });
}

async function toggleTask(idx: number, line: number): Promise<void> {
  const s = await commitEditing();
  const i = s && idx > s.at ? idx + s.delta : idx;
  await runEdit({ op: "toggle_task", block: String(i), line });
}

async function followAnchor(a: HTMLAnchorElement): Promise<void> {
  const path = a.dataset.path;
  const target = a.dataset.target;
  try {
    if (path && /\.md$/i.test(path)) {
      await openPage(path);
    } else if (target && a.classList.contains("wikilink")) {
      let p = await api.resolve(target);
      if (!p) p = await api.createPageAt(target);
      await refreshTree();
      await openPage(p);
    }
  } catch (err) {
    banner(`Could not open link: ${errorText(err)}`);
  }
}

type EditOp = Parameters<typeof api.editBlock>[1];

function handleError(err: unknown, what: string): void {
  const msg = errorText(err);
  if (msg.startsWith("CONFLICT:")) {
    banner("This page changed on disk, so it was reloaded. Redo your last change.", {
      label: "Dismiss",
      run: () => undefined,
    });
    void reloadPage().catch(() => undefined);
  } else {
    banner(`${what}: ${msg}`);
  }
}

/** Apply one edit against the revision we rendered, then re-read the page from Rust. */
async function runEdit(edit: EditOp): Promise<boolean> {
  const page = ws.page;
  const cur = ws.current;
  if (!page || !cur || cur.kind !== "page") return false;
  try {
    await api.editBlock(cur.path, edit, page.revision);
    await reloadPage();
    return true;
  } catch (err) {
    handleError(err, "Could not save the change");
    return false;
  }
}

// ---------------------------------------------------------------- editing a block

function caretFor(md: string): number {
  if (md.startsWith("```")) return md.indexOf("\n") + 1;
  if (md.endsWith("()")) return md.length - 1;
  if (md.endsWith("]]")) return md.length - 2;
  return md.length;
}

function mountEditor(host: HTMLElement, edit: Edit): void {
  host.classList.add("block-editing");
  host.replaceChildren();
  const holder = h("div", { class: "block-editor" });
  host.append(holder);
  const ed = createBlockEditor(holder, {
    doc: edit.original,
    onChange: (text) => updateSlash(edit, text),
    onBlur: () => {
      if (ws.editing === edit) void commitEditing();
    },
    onEscape: () => void commitEditing(),
    onCtrlEnter: () => void insertBelow(),
    onBackspaceEmpty: () => void commitEditing(),
    slashKey: (key) => slashKey(edit, key),
  });
  edit.ed = ed;
  ed.focus(caretFor(edit.original));
}

async function startEdit(idx: number, original?: string): Promise<void> {
  const s = await commitEditing();
  const i = s && idx > s.at ? idx + s.delta : idx;
  const page = ws.page;
  const b = page?.blocks[i];
  if (!page || !b) return;
  ws.editing = {
    target: i,
    after: null,
    ed: null as unknown as BlockEditorHandle,
    original: original ?? b.markdown,
    slash: null,
  };
  renderBlocks();
}

async function startPending(after: number, openSlash = false): Promise<void> {
  const s = await commitEditing();
  const a = s && after > s.at ? after + s.delta : after;
  const page = ws.page;
  if (!page) return;
  const edit: Edit = {
    target: "pending",
    after: a < 0 ? null : a,
    ed: null as unknown as BlockEditorHandle,
    original: openSlash ? "/" : "",
    slash: null,
  };
  ws.editing = edit;
  renderBlocks();
  if (openSlash) updateSlash(edit, "/");
  document.querySelector(".block.pending")?.scrollIntoView({ block: "nearest" });
}

async function insertBelow(): Promise<void> {
  const edit = ws.editing;
  if (!edit) return;
  const base = edit.target === "pending" ? (edit.after ?? -1) : edit.target;
  const s = await commitEditing();
  await startPending(base + (s ? s.delta : 0));
}

function commitEditing(): Promise<Shift | null> {
  const edit = ws.editing;
  if (!edit) return ws.commitP ?? Promise.resolve(null);
  ws.editing = null;
  closeSlashMenu();
  const text = edit.ed.getText();
  edit.ed.destroy();
  const p = doCommit(edit, text);
  ws.commitP = p;
  void p.finally(() => {
    if (ws.commitP === p) ws.commitP = null;
  });
  return p;
}

async function doCommit(edit: Edit, text: string): Promise<Shift | null> {
  const page = ws.page;
  const cur = ws.current;
  if (!page || !cur || cur.kind !== "page") return null;
  const before = page.blocks.length;
  try {
    if (edit.target === "pending") {
      if (text.trim() === "" || BARE_MARKER.test(text.trim()) || /^\/\S*$/.test(text.trim())) {
        renderBlocks();
        return null;
      }
      await api.editBlock(
        cur.path,
        { op: "insert", after: edit.after === null ? null : String(edit.after), markdown: text },
        page.revision,
      );
      await reloadPage();
      const base = edit.after ?? -1;
      return { at: base, delta: (ws.page?.blocks.length ?? before) - before };
    }
    const idx = edit.target;
    const remove = text.trim() === "" || BARE_MARKER.test(text.trim());
    if (!remove && text === edit.original) {
      renderBlocks();
      return null;
    }
    await api.editBlock(
      cur.path,
      remove
        ? { op: "delete", block: String(idx) }
        : { op: "replace", block: String(idx), markdown: text },
      page.revision,
    );
    await reloadPage();
    return { at: idx, delta: (ws.page?.blocks.length ?? before) - before };
  } catch (err) {
    handleError(err, "Could not save the block");
    return null;
  }
}

// ---------------------------------------------------------------- slash menu

function closeSlashMenu(): void {
  document.querySelector(".slash-menu")?.remove();
  if (ws.editing) ws.editing.slash = null;
}

function updateSlash(edit: Edit, text: string): void {
  const wants = edit.target === "pending" && /^\/[^\s/]*$/.test(text);
  if (!wants) {
    document.querySelector(".slash-menu")?.remove();
    edit.slash = null;
    return;
  }
  const q = text.slice(1).toLowerCase();
  const items = ws.types.filter(
    (t) => t.label.toLowerCase().includes(q) || t.kind.toLowerCase().includes(q),
  );
  if (items.length === 0) {
    document.querySelector(".slash-menu")?.remove();
    edit.slash = null;
    return;
  }
  let menu = document.querySelector<HTMLElement>(".slash-menu");
  if (!menu) {
    menu = h("div", { class: "slash-menu", role: "listbox", label: "Block types" });
    menu.addEventListener("mousedown", (e) => e.preventDefault());
    document.body.append(menu);
  }
  const prevActive = edit.slash ? edit.slash.active : 0;
  edit.slash = { menu, items, active: Math.min(prevActive, items.length - 1) };
  renderSlash(edit);
  const anchor = document.querySelector<HTMLElement>(".block.pending .block-body, .block-editing");
  if (anchor) {
    const r = anchor.getBoundingClientRect();
    menu.style.left = `${Math.max(4, Math.min(r.left, window.innerWidth - 260))}px`;
    const below = r.bottom + 4;
    menu.style.top = `${below + 280 > window.innerHeight ? Math.max(4, r.top - 284) : below}px`;
  }
}

function renderSlash(edit: Edit): void {
  const s = edit.slash;
  if (!s) return;
  s.menu.replaceChildren();
  s.items.forEach((t, i) => {
    const opt = h(
      "div",
      { class: "slash-item", role: "option" },
      h("span", { class: "slash-label", text: t.label }),
      h("span", { class: "muted small", text: t.hint }),
    );
    opt.id = `slash-${t.kind}`;
    opt.setAttribute("aria-selected", String(i === s.active));
    opt.addEventListener("click", () => void pickSlash(edit, t));
    s.menu.append(opt);
  });
  s.menu.querySelector(".slash-item[aria-selected='true']")?.scrollIntoView({ block: "nearest" });
}

function slashKey(edit: Edit, key: SlashKey): boolean {
  const s = edit.slash;
  if (!s) return false;
  if (key === "ArrowDown") s.active = (s.active + 1) % s.items.length;
  else if (key === "ArrowUp") s.active = (s.active - 1 + s.items.length) % s.items.length;
  else if (key === "Enter") {
    const t = s.items[s.active];
    if (t) void pickSlash(edit, t);
    return true;
  } else if (key === "Escape") {
    closeSlashMenu();
    return true;
  }
  renderSlash(edit);
  return true;
}

async function pickSlash(edit: Edit, t: BlockType): Promise<void> {
  closeSlashMenu();
  if (ws.editing !== edit) return;
  if (t.markdown === "") {
    edit.ed.setText("");
    return;
  }
  // Insert the template as a real block, then open it for editing.
  const page = ws.page;
  const cur = ws.current;
  if (!page || !cur) return;
  ws.editing = null;
  edit.ed.destroy();
  try {
    await api.editBlock(
      cur.path,
      { op: "insert", after: edit.after === null ? null : String(edit.after), markdown: t.markdown },
      page.revision,
    );
    await reloadPage();
    const idx = (edit.after ?? -1) + 1;
    await startEdit(idx, t.markdown);
  } catch (err) {
    handleError(err, "Could not insert the block");
    renderBlocks();
  }
}

// ---------------------------------------------------------------- lifecycle

export function initWorkspace(host: WorkspaceHost): void {
  ws.host = host;
  initDatabase({
    openPage: (path) => void openPage(path),
    banner,
    openTasks: () => host.openTasks(),
    refreshTree: () => void refreshTree(),
  });
  $("ws-new-page").addEventListener("click", () => void newPage(null));
  $("ws-new-db").addEventListener("click", () => void newDatabase());
  wireTreeKeys();
}

export async function enterWorkspace(): Promise<void> {
  ws.active = true;
  if (ws.types.length === 0) {
    try {
      ws.types = await api.blockTypes();
    } catch (err) {
      banner(`Could not load block types: ${errorText(err)}`);
    }
  }
  await refreshTree();
  if (ws.current?.kind === "page") await openPage(ws.current.path);
  else if (ws.current?.kind === "database") await openDb(ws.current.path);
  else showPanel("empty");
}

export function leaveWorkspace(): void {
  if (!ws.active) return;
  ws.active = false;
  void commitEditing();
  closeSlashMenu();
}

export async function reloadWorkspace(): Promise<void> {
  if (!ws.active) return;
  await refreshTree();
  if (ws.editing || ws.commitP) return;
  const cur = ws.current;
  if (cur?.kind === "page" && ws.page) {
    try {
      const fresh = await api.pageBlocks(cur.path);
      if (fresh.revision !== ws.page.revision) await reloadPage();
    } catch {
      // the page may have been removed outside the app
    }
  }
}
