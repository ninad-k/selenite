// Tasks mode shell: nav, list/board, quick add and the detail drawer. Presentation only: every
// rule (filters, parsing, completion, recurrence, reminders) lives behind the Tauri commands.
// All vault text is written with textContent; nothing here uses innerHTML.

import {
  api,
  errorText,
  type ProjectCount,
  type SavedView,
  type TaskDto,
  type TaskPatch,
} from "./api";

export interface TasksHost {
  /** Switch to Vault mode and open this file in the editor. */
  openAsPage(path: string): void;
}

type BuiltinId = "inbox" | "today" | "upcoming" | "completed";
type View =
  | { kind: "builtin"; id: BuiltinId }
  | { kind: "project"; name: string }
  | { kind: "label"; name: string }
  | { kind: "saved"; name: string; filter: string; layout: string }
  | { kind: "filter"; expr: string };
type Layout = "list" | "board";
interface Group {
  title: string | null;
  tasks: TaskDto[];
  empty?: string;
}

const BUILTINS: { id: BuiltinId; label: string }[] = [
  { id: "inbox", label: "Inbox" },
  { id: "today", label: "Today" },
  { id: "upcoming", label: "Upcoming" },
];

const st = {
  host: null as TasksHost | null,
  active: false,
  wired: false,
  view: { kind: "builtin", id: "inbox" } as View,
  layout: "list" as Layout,
  order: [] as TaskDto[],
  selected: null as string | null,
  drawerPath: null as string | null,
  token: 0,
  projects: [] as ProjectCount[],
  labels: [] as string[],
  views: [] as SavedView[],
};

function $<T extends HTMLElement>(id: string): T {
  const e = document.getElementById(id);
  if (!e) throw new Error(`missing element #${id}`);
  return e as T;
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function isTyping(t: EventTarget | null): boolean {
  return (
    t instanceof HTMLInputElement ||
    t instanceof HTMLTextAreaElement ||
    t instanceof HTMLSelectElement ||
    (t instanceof HTMLElement && t.isContentEditable)
  );
}

// ---------------------------------------------------------------- date formatting

const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

function ymd(d: Date): string {
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

function parseYmd(s: string): Date {
  const [y, m, d] = s.split("-").map(Number);
  return new Date(y ?? 1970, (m ?? 1) - 1, d ?? 1);
}

function dayDiff(date: string): number {
  const a = parseYmd(date);
  const b = parseYmd(ymd(new Date()));
  return Math.round((a.getTime() - b.getTime()) / 86_400_000);
}

function dateLabel(date: string): string {
  const diff = dayDiff(date);
  const d = parseYmd(date);
  if (diff === 0) return "Today";
  if (diff === 1) return "Tomorrow";
  if (diff >= 2 && diff <= 6) return WEEKDAYS[d.getDay()] ?? date;
  return `${(WEEKDAYS[d.getDay()] ?? "").slice(0, 3)} ${d.getDate()} ${MONTHS[d.getMonth()] ?? ""}`;
}

function dayTitle(date: string): string {
  const d = parseYmd(date);
  const short = `${(WEEKDAYS[d.getDay()] ?? "").slice(0, 3)} ${d.getDate()} ${MONTHS[d.getMonth()] ?? ""}`;
  const diff = dayDiff(date);
  return diff === 0 || diff === 1 ? `${dateLabel(date)} · ${short}` : short;
}

function dueLabel(due: string): { text: string; overdue: boolean } {
  const date = due.slice(0, 10);
  const time = due.length > 10 ? due.slice(11, 16) : "";
  const text = time ? `${dateLabel(date)} ${time}` : dateLabel(date);
  return { text, overdue: dayDiff(date) < 0 };
}

// ---------------------------------------------------------------- views

function viewKey(v: View): string {
  switch (v.kind) {
    case "builtin":
      return `b:${v.id}`;
    case "project":
      return `p:${v.name}`;
    case "label":
      return `l:${v.name}`;
    case "saved":
      return `s:${v.name}`;
    case "filter":
      return "f";
  }
}

function quoted(name: string): string {
  return `"${name.split('"').join("")}"`;
}

function viewExpr(v: View): string {
  switch (v.kind) {
    case "builtin":
      return v.id;
    case "project":
      return `#${quoted(v.name)} & open`;
    case "label":
      return `@${quoted(v.name)} & open`;
    case "saved":
      return v.filter;
    case "filter":
      return v.expr;
  }
}

function viewTitle(v: View): string {
  switch (v.kind) {
    case "builtin":
      return v.id.charAt(0).toUpperCase() + v.id.slice(1);
    case "project":
      return v.name;
    case "label":
      return `@${v.name}`;
    case "saved":
      return v.name;
    case "filter":
      return "Custom filter";
  }
}

function boardable(v: View): boolean {
  return v.kind === "project" || v.kind === "saved";
}

function setView(v: View): void {
  st.view = v;
  st.layout = v.kind === "saved" && v.layout === "board" ? "board" : "list";
  st.selected = null;
  renderNav();
  void loadList();
}

// ---------------------------------------------------------------- messages

function showMsg(text: string, action?: { label: string; run: () => void }): void {
  const m = $("task-msg");
  m.replaceChildren(document.createTextNode(text));
  if (action) {
    const b = el("button", "link-btn", action.label);
    b.type = "button";
    b.addEventListener("click", action.run);
    m.append(" ", b);
  }
}

function clearMsg(): void {
  $("task-msg").replaceChildren();
}

// ---------------------------------------------------------------- nav

function navButton(
  label: string,
  key: string,
  onClick: () => void,
  opts: { count?: number; depth?: number; title?: string } = {},
): HTMLLIElement {
  const li = el("li");
  const b = el("button", "nav-item");
  b.type = "button";
  if (opts.title) b.title = opts.title;
  b.style.paddingLeft = `${10 + (opts.depth ?? 0) * 14}px`;
  b.append(el("span", "nav-label", label));
  if (opts.count !== undefined) {
    const c = el("span", "nav-count", String(opts.count));
    c.setAttribute("aria-label", `${opts.count} open`);
    b.append(c);
  }
  if (viewKey(st.view) === key) b.setAttribute("aria-current", "page");
  b.addEventListener("click", onClick);
  li.append(b);
  return li;
}

function renderNav(): void {
  const main = $("nav-main");
  main.replaceChildren(
    ...BUILTINS.map((b) =>
      navButton(b.label, `b:${b.id}`, () => setView({ kind: "builtin", id: b.id })),
    ),
  );

  const projects = $("nav-projects");
  projects.replaceChildren();
  if (st.projects.length === 0) {
    projects.append(el("li", "muted small nav-note", "No projects yet. Add #name to a task."));
  }
  for (const p of st.projects) {
    const segs = p.project.split("/");
    projects.append(
      navButton(
        segs[segs.length - 1] ?? p.project,
        `p:${p.project}`,
        () => setView({ kind: "project", name: p.project }),
        { count: p.open, depth: segs.length - 1, title: p.project },
      ),
    );
  }

  const labels = $("nav-labels");
  labels.replaceChildren();
  if (st.labels.length === 0) labels.append(el("p", "muted small nav-note", "No labels yet."));
  for (const name of st.labels) {
    const b = el("button", "tag-chip", `@${name}`);
    b.type = "button";
    if (viewKey(st.view) === `l:${name}`) b.setAttribute("aria-current", "page");
    b.addEventListener("click", () => setView({ kind: "label", name }));
    labels.append(b);
  }

  const views = $("nav-views");
  views.replaceChildren();
  if (st.views.length === 0) views.append(el("li", "muted small nav-note", "None saved yet."));
  for (const v of st.views) {
    views.append(
      navButton(
        v.name,
        `s:${v.name}`,
        () => setView({ kind: "saved", name: v.name, filter: v.filter, layout: v.layout }),
        { title: v.filter },
      ),
    );
  }

  $("nav-end").replaceChildren(
    navButton("Completed", "b:completed", () => setView({ kind: "builtin", id: "completed" })),
  );
}

async function loadNav(): Promise<void> {
  try {
    const [projects, labels, views] = await Promise.all([
      api.taskProjects(),
      api.taskLabels(),
      api.taskViews(),
    ]);
    st.projects = projects;
    st.labels = labels;
    st.views = views;
    const dl = $("d-projects");
    dl.replaceChildren(
      ...projects.map((p) => {
        const o = el("option");
        o.value = p.project;
        return o;
      }),
    );
  } catch (err) {
    showMsg(`Could not load lists: ${errorText(err)}`);
  }
  renderNav();
}

// ---------------------------------------------------------------- list rendering

function taskRow(t: TaskDto): HTMLLIElement {
  const li = el("li", "task");
  li.dataset.path = t.path;
  li.dataset.p = String(t.priority);
  li.tabIndex = -1;
  if (t.done) li.classList.add("done");

  const check = el("button", "check");
  check.type = "button";
  check.tabIndex = -1;
  check.setAttribute("role", "checkbox");
  check.setAttribute("aria-checked", String(t.done));
  check.setAttribute("aria-label", `${t.done ? "Reopen" : "Complete"}: ${t.title}`);
  check.addEventListener("click", (e) => {
    e.stopPropagation();
    void toggleDone(t);
  });

  const body = el("div", "task-body");
  const title = el("span", "task-title", t.title);
  body.append(title);
  if (t.priority < 4) {
    body.append(el("span", "sr-only", ` Priority ${t.priority}.`));
  }
  const meta = el("div", "task-meta");
  if (t.due) {
    const d = dueLabel(t.due);
    const chip = el("span", d.overdue && !t.done ? "due overdue" : "due", d.text);
    if (d.overdue && !t.done) chip.append(el("span", "sr-only", " (overdue)"));
    meta.append(chip);
  }
  if (t.recurring) {
    const r = el("span", "repeat", "↻");
    r.title = t.recurrence ? `Repeats: ${t.recurrence}` : "Repeats";
    r.setAttribute("role", "img");
    r.setAttribute("aria-label", "Repeats");
    meta.append(r);
  }
  if (t.project) meta.append(el("span", "chip chip-project", t.project));
  for (const l of t.labels) meta.append(el("span", "chip", `@${l}`));
  if (meta.childNodes.length > 0) body.append(meta);

  li.append(check, body);
  li.addEventListener("click", () => {
    select(t.path, true);
    void openDrawer(t.path, false);
  });
  return li;
}

function taskList(tasks: TaskDto[]): HTMLUListElement {
  const ul = el("ul", "task-ul");
  for (const t of tasks) ul.append(taskRow(t));
  return ul;
}

function renderGroups(groups: Group[], board: boolean): void {
  const host = $("task-list");
  st.order = groups.flatMap((g) => g.tasks);
  host.replaceChildren();
  host.classList.toggle("board", board);
  const total = st.order.length;
  const anyEmptyNote = groups.some((g) => g.empty);
  if (total === 0 && !anyEmptyNote) {
    host.append(el("p", "muted empty-note", "Nothing here."));
    return;
  }
  for (const g of groups) {
    const sec = el("section", board ? "column" : "group");
    if (g.title !== null) {
      const head = el("h2", "group-title", g.title);
      if (g.tasks.length > 0) head.append(el("span", "group-count", String(g.tasks.length)));
      sec.append(head);
    }
    if (g.tasks.length === 0) {
      if (g.empty) sec.append(el("p", "muted small empty-day", g.empty));
    } else {
      sec.append(taskList(g.tasks));
    }
    host.append(sec);
  }
}

function sectionGroups(tasks: TaskDto[]): Group[] {
  const none: TaskDto[] = [];
  const by = new Map<string, TaskDto[]>();
  for (const t of tasks) {
    if (!t.section) {
      none.push(t);
    } else {
      const arr = by.get(t.section) ?? [];
      arr.push(t);
      by.set(t.section, arr);
    }
  }
  const names = [...by.keys()].sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
  const groups: Group[] = [{ title: "No section", tasks: none }];
  for (const n of names) groups.push({ title: n, tasks: by.get(n) ?? [] });
  return groups;
}

async function loadList(): Promise<void> {
  const token = ++st.token;
  const v = st.view;
  $("tasks-title").textContent = viewTitle(v);
  renderLayoutToggle();
  const hadFocus = $("task-list").contains(document.activeElement);
  const prevIndex = st.order.findIndex((t) => t.path === st.selected);
  try {
    let groups: Group[];
    let board = false;
    if (v.kind === "builtin" && v.id === "upcoming") {
      const agenda = await api.taskAgenda(7);
      groups = agenda.days.map((d) => ({
        title: dayTitle(d.date),
        tasks: d.tasks,
        empty: "Nothing scheduled",
      }));
    } else {
      const tasks = await api.tasksQuery(viewExpr(v));
      if (v.kind === "builtin" && v.id === "today") {
        const today = ymd(new Date());
        const overdue = tasks.filter((t) => (t.due ?? "").slice(0, 10) < today);
        const rest = tasks.filter((t) => (t.due ?? "").slice(0, 10) >= today);
        groups = [];
        if (overdue.length > 0) groups.push({ title: "Overdue", tasks: overdue });
        groups.push({ title: "Today", tasks: rest, empty: "Nothing due today" });
      } else if (boardable(v) && st.layout === "board") {
        board = true;
        groups = sectionGroups(tasks);
      } else {
        groups = [{ title: null, tasks }];
      }
    }
    if (token !== st.token) return;
    renderGroups(groups, board);
  } catch (err) {
    if (token !== st.token) return;
    $("task-list").replaceChildren(el("p", "error", errorText(err)));
    st.order = [];
    return;
  }
  // Keep the selection, or the same position if the selected task left the list.
  let next = st.order.find((t) => t.path === st.selected)?.path ?? null;
  if (!next && prevIndex >= 0 && st.order.length > 0) {
    next = st.order[Math.min(prevIndex, st.order.length - 1)]?.path ?? null;
  }
  select(next, hadFocus);
}

function renderLayoutToggle(): void {
  const host = $("layout-toggle");
  host.hidden = !boardable(st.view);
  host.replaceChildren();
  for (const l of ["list", "board"] as const) {
    const b = el("button", "", l === "list" ? "List" : "Board");
    b.type = "button";
    b.setAttribute("aria-pressed", String(st.layout === l));
    b.addEventListener("click", () => {
      st.layout = l;
      void loadList();
    });
    host.append(b);
  }
}

function rowEls(): HTMLElement[] {
  return [...$("task-list").querySelectorAll<HTMLElement>("li.task")];
}

function select(path: string | null, focus: boolean): void {
  st.selected = path;
  const rows = rowEls();
  let found = false;
  for (const r of rows) {
    const on = r.dataset.path === path;
    if (on) found = true;
    r.classList.toggle("selected", on);
    if (on) r.setAttribute("aria-current", "true");
    else r.removeAttribute("aria-current");
    r.tabIndex = on ? 0 : -1;
    if (on && focus) r.focus();
    if (on) r.scrollIntoView({ block: "nearest" });
  }
  if (!found) {
    st.selected = null;
    if (rows[0]) rows[0].tabIndex = 0;
  }
}

function move(delta: number): void {
  const rows = rowEls();
  if (rows.length === 0) return;
  const cur = rows.findIndex((r) => r.dataset.path === st.selected);
  const next = cur < 0 ? (delta > 0 ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, cur + delta));
  select(rows[next]?.dataset.path ?? null, true);
}

async function afterMutation(): Promise<void> {
  await Promise.all([loadList(), loadNav(), refreshReminders()]);
}

async function toggleDone(t: TaskDto): Promise<void> {
  try {
    if (t.done) {
      await api.taskReopen(t.path);
      showMsg(`Reopened “${t.title}”.`);
    } else {
      const r = await api.taskComplete(t.path);
      if (!r.done && r.recurring) {
        showMsg(`Done. “${t.title}” repeats; next due ${r.due ? dueLabel(r.due).text : "later"}.`);
      } else {
        showMsg(`Completed “${t.title}”.`, {
          label: "Undo",
          run: () => {
            void api
              .taskReopen(t.path)
              .then(() => {
                clearMsg();
                return afterMutation();
              })
              .catch((e: unknown) => showMsg(`Could not undo: ${errorText(e)}`));
          },
        });
      }
    }
    if (st.drawerPath === t.path) await openDrawer(t.path, false);
    await afterMutation();
  } catch (err) {
    showMsg(`Could not update the task: ${errorText(err)}`);
  }
}

// ---------------------------------------------------------------- quick add

async function quickAdd(): Promise<void> {
  const input = $<HTMLInputElement>("quickadd");
  const text = input.value.trim();
  if (!text) return;
  const warnings = $("quickadd-warnings");
  try {
    const res = await api.taskQuickAdd(text);
    input.value = "";
    warnings.replaceChildren(...res.warnings.map((w) => el("li", "", w)));
    showMsg("Task added.");
    st.selected = res.path;
    await afterMutation();
  } catch (err) {
    warnings.replaceChildren();
    showMsg(`Could not add the task: ${errorText(err)}`);
  }
}

// ---------------------------------------------------------------- drawer

function drawerError(msg: string): void {
  const e = $("d-error");
  e.textContent = msg;
  e.hidden = !msg;
}

function fillDrawer(t: TaskDto): void {
  $<HTMLInputElement>("d-title").value = t.title;
  $<HTMLSelectElement>("d-priority").value = String(t.priority);
  $<HTMLInputElement>("d-date").value = t.due ? t.due.slice(0, 10) : "";
  $<HTMLInputElement>("d-time").value = t.due && t.due.length > 10 ? t.due.slice(11, 16) : "";
  $<HTMLInputElement>("d-project").value = t.project ?? "";
  $<HTMLInputElement>("d-section").value = t.section ?? "";
  $<HTMLInputElement>("d-labels").value = t.labels.join(", ");
  $<HTMLInputElement>("d-reminders").value = t.reminders.join(", ");
  $("d-repeat").textContent = t.recurrence ? `Repeats: ${t.recurrence}` : "";
  $("d-path").textContent = t.path;
  drawerError("");
}

async function openDrawer(path: string, focusTitle: boolean): Promise<void> {
  try {
    const detail = await api.taskGet(path);
    st.drawerPath = path;
    fillDrawer(detail.fields);
    $("drawer-empty").hidden = true;
    $("drawer-form").hidden = false;
    if (focusTitle) $<HTMLInputElement>("d-title").focus();
  } catch (err) {
    showMsg(`Could not open the task: ${errorText(err)}`);
  }
}

function closeDrawer(): void {
  st.drawerPath = null;
  $("drawer-form").hidden = true;
  $("drawer-empty").hidden = false;
}

function splitList(raw: string): string[] {
  return raw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

async function saveDrawer(): Promise<void> {
  const path = st.drawerPath;
  if (!path) return;
  const date = $<HTMLInputElement>("d-date").value;
  const time = $<HTMLInputElement>("d-time").value;
  if (time && !date) {
    drawerError("Pick a due date to go with the time.");
    return;
  }
  const patch: TaskPatch = {
    title: $<HTMLInputElement>("d-title").value,
    priority: Number($<HTMLSelectElement>("d-priority").value),
    due: date ? (time ? `${date}T${time}` : date) : null,
    project: $<HTMLInputElement>("d-project").value || null,
    section: $<HTMLInputElement>("d-section").value || null,
    labels: splitList($<HTMLInputElement>("d-labels").value),
    reminders: splitList($<HTMLInputElement>("d-reminders").value),
  };
  try {
    const t = await api.taskUpdate(path, patch);
    fillDrawer(t);
    showMsg("Saved.");
    await afterMutation();
  } catch (err) {
    drawerError(errorText(err));
  }
}

async function archiveDrawer(): Promise<void> {
  const path = st.drawerPath;
  if (!path) return;
  if (!window.confirm("Archive this task? It moves to the vault trash.")) return;
  try {
    await api.taskArchive(path);
    closeDrawer();
    st.selected = null;
    showMsg("Task archived.");
    await afterMutation();
  } catch (err) {
    drawerError(errorText(err));
  }
}

// ---------------------------------------------------------------- filter box / saved views

function filterMsg(text: string, isError = false): void {
  const m = $("filter-msg");
  m.textContent = text;
  m.classList.toggle("error", isError);
}

async function runFilter(): Promise<void> {
  const expr = $<HTMLInputElement>("filter-input").value.trim();
  if (!expr) return;
  try {
    await api.tasksQuery(expr); // surface parse errors before switching views
    filterMsg("");
    setView({ kind: "filter", expr });
  } catch (err) {
    filterMsg(errorText(err), true);
  }
}

async function saveView(): Promise<void> {
  const nameInput = $<HTMLInputElement>("view-name");
  const expr = $<HTMLInputElement>("filter-input").value.trim() || viewExpr(st.view);
  if (nameInput.hidden) {
    nameInput.hidden = false;
    nameInput.focus();
    filterMsg("Name this view, then press Save view again.");
    return;
  }
  const name = nameInput.value.trim();
  if (!name) {
    filterMsg("Give the view a name first.", true);
    nameInput.focus();
    return;
  }
  try {
    st.views = await api.taskViewSave(name, expr, st.layout);
    nameInput.value = "";
    nameInput.hidden = true;
    filterMsg(`Saved view “${name}”.`);
    renderNav();
  } catch (err) {
    filterMsg(errorText(err), true);
  }
}

// ---------------------------------------------------------------- reminders status

/** Update the status bar from the reminder registration status. */
export async function refreshReminders(): Promise<void> {
  const out = $("status-reminders");
  try {
    const s = await api.remindersStatus();
    if (s.last_error) {
      out.textContent = "Reminders: not registered";
      out.title = s.last_error;
      out.dataset.state = "error";
    } else {
      out.textContent = `Reminders: ${s.scheduled} scheduled`;
      out.title = `Backend: ${s.kind}`;
      out.dataset.state = "ok";
    }
  } catch {
    out.textContent = "";
    out.title = "";
  }
}

// ---------------------------------------------------------------- lifecycle

function wire(): void {
  if (st.wired) return;
  st.wired = true;

  $("quickadd-form").addEventListener("submit", (e) => {
    e.preventDefault();
    void quickAdd();
  });
  $("quickadd").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      $<HTMLInputElement>("quickadd").blur();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      const rows = rowEls();
      const target = rows.find((r) => r.dataset.path === st.selected) ?? rows[0];
      select(target?.dataset.path ?? null, true);
    }
  });

  $("task-list").addEventListener("keydown", (e) => {
    if (isTyping(e.target) || e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      if (st.selected) {
        e.preventDefault();
        void openDrawer(st.selected, true);
      }
    } else if (e.key === " " || e.key.toLowerCase() === "x") {
      const t = st.order.find((x) => x.path === st.selected);
      if (t) {
        e.preventDefault();
        void toggleDone(t);
      }
    }
  });

  document.addEventListener("keydown", (e) => {
    if (!st.active || e.ctrlKey || e.metaKey || e.altKey) return;
    if (!$("overlay").hidden) return;
    if (e.key === "q" && !isTyping(e.target)) {
      e.preventDefault();
      $<HTMLInputElement>("quickadd").focus();
    }
  });

  $("drawer-form").addEventListener("submit", (e) => {
    e.preventDefault();
    void saveDrawer();
  });
  $("drawer-form").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      const row = rowEls().find((r) => r.dataset.path === st.selected);
      row?.focus();
    }
  });
  $("d-archive").addEventListener("click", () => void archiveDrawer());
  $("d-open").addEventListener("click", (e) => {
    e.preventDefault();
    if (st.drawerPath) st.host?.openAsPage(st.drawerPath);
  });

  $("filter-form").addEventListener("submit", (e) => {
    e.preventDefault();
    void runFilter();
  });
  $("btn-save-view").addEventListener("click", () => void saveView());
  $("view-name").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void saveView();
    }
  });
}

/** One-time setup. */
export function initTasks(host: TasksHost): void {
  st.host = host;
  wire();
}

/** Show the Tasks shell and load it. */
export async function enterTasks(): Promise<void> {
  st.active = true;
  closeDrawer();
  await Promise.all([loadNav(), loadList(), refreshReminders()]);
}

/** Hide-side bookkeeping when leaving Tasks mode. */
export function leaveTasks(): void {
  st.active = false;
  st.token++;
}

/** Reload lists after the window regains focus. */
export async function reloadTasks(): Promise<void> {
  if (!st.active) return;
  await Promise.all([loadNav(), loadList(), refreshReminders()]);
}
