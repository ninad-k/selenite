// Workspace database views. The UI only builds structures (a ViewQuery, a schema edit) and shows
// the rows Rust returns; filtering, sorting, grouping and formulas happen in the engine.
// All vault text is written with textContent.

import {
  api,
  errorText,
  type CellValue,
  type DbRow,
  type PropertyDef,
  type SortRule,
  type ViewDef,
  type ViewQuery,
  type ViewResult,
} from "./api";
import { button, h, modal, promptText } from "./dom";

export interface DatabaseHost {
  openPage(path: string): void;
  banner(text: string, action?: { label: string; run: () => void }): void;
  openTasks(): void;
  refreshTree(): void;
}

interface DraftRule {
  property: string;
  op: string;
  text: string;
}

const NO_VALUE = "No value";
const LAYOUTS = ["table", "board", "calendar", "list", "gallery", "timeline"] as const;
const DAY = 86_400_000;

const db = {
  host: null as DatabaseHost | null,
  folder: "",
  root: null as HTMLElement | null,
  viewName: null as string | null,
  result: null as ViewResult | null,
  ready: false, // working state initialised from the loaded view
  local: {} as Partial<ViewDef>,
  join: "and" as "and" | "or",
  draft: [] as DraftRule[],
  sorts: [] as SortRule[],
  group: null as string | null,
  panel: null as "filter" | "sort" | "group" | null,
  month: new Date(),
  tlStart: null as number | null,
  status: "",
  token: 0,
};

export function initDatabase(host: DatabaseHost): void {
  db.host = host;
}

export function closeDatabase(): void {
  db.token++;
  db.root = null;
}

function banner(text: string): void {
  db.host?.banner(text);
}

// ---------------------------------------------------------------- value helpers

export function fmt(v: CellValue | undefined): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "boolean") return v ? "☑" : "☐";
  if (Array.isArray(v)) return v.map((x) => fmt(x)).join(", ");
  return String(v);
}

function titleName(r: ViewResult): string {
  return r.properties.find((p) => p.type === "title")?.name ?? "Name";
}

function isComputed(p: PropertyDef): boolean {
  return ["formula", "rollup", "created_time", "updated_time"].includes(p.type);
}

function parseDay(v: CellValue | undefined): number | null {
  if (typeof v !== "string") return null;
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(v);
  if (!m) return null;
  return Date.UTC(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
}

function dayString(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/** An editable control for one property of one row (computed properties are read-only text). */
export function propertyCell(
  p: PropertyDef,
  row: DbRow,
  _view: ViewResult,
  onSet: (name: string, value: CellValue) => void,
): HTMLElement {
  const v = row.values[p.name] ?? null;
  const label = `${p.name} of ${row.title}`;
  const wrap = h("span", { class: "cell" });
  if (isComputed(p)) {
    wrap.append(h("span", { class: "cell-computed", text: fmt(v), title: `${p.name} (computed)` }));
    return wrap;
  }
  if (p.type === "checkbox") {
    const cb = h("input", { type: "checkbox", label });
    cb.checked = v === true;
    cb.addEventListener("change", () => onSet(p.name, cb.checked));
    wrap.append(cb);
  } else if (p.type === "select") {
    const sel = h("select", { label });
    sel.append(h("option", { text: "", attrs: { value: "" } }));
    const names = (p.options ?? []).map((o) => o.name);
    if (typeof v === "string" && v && !names.includes(v)) names.push(v);
    for (const n of names) sel.append(h("option", { text: n, attrs: { value: n } }));
    sel.value = typeof v === "string" ? v : "";
    sel.addEventListener("change", () => onSet(p.name, sel.value === "" ? null : sel.value));
    wrap.append(sel);
  } else if (p.type === "multi_select" || p.type === "relation") {
    const inp = h("input", { type: "text", label });
    inp.value = fmt(v);
    inp.placeholder = p.type === "relation" ? "Page names, comma separated" : "a, b";
    inp.addEventListener("change", () =>
      onSet(
        p.name,
        inp.value
          .split(",")
          .map((x) => x.trim())
          .filter(Boolean),
      ),
    );
    wrap.append(inp);
  } else if (p.type === "number") {
    const inp = h("input", { type: "text", label });
    inp.inputMode = "decimal";
    inp.value = fmt(v);
    inp.addEventListener("change", () => {
      const t = inp.value.trim();
      if (t === "") return onSet(p.name, null);
      const n = Number(t);
      if (Number.isNaN(n)) {
        inp.value = fmt(v);
        return banner(`${p.name} must be a number.`);
      }
      onSet(p.name, n);
    });
    wrap.append(inp);
  } else if (p.type === "date") {
    const timed = typeof v === "string" && v.includes("T");
    const inp = h("input", { type: timed ? "datetime-local" : "date", label });
    inp.value = typeof v === "string" ? v.slice(0, timed ? 16 : 10) : "";
    inp.addEventListener("change", () => onSet(p.name, inp.value === "" ? null : inp.value));
    wrap.append(inp);
  } else {
    const inp = h("input", { type: "text", label });
    inp.value = fmt(v);
    inp.addEventListener("change", () => onSet(p.name, inp.value === "" ? null : inp.value));
    wrap.append(inp);
  }
  return wrap;
}

// ---------------------------------------------------------------- loading

export async function openDatabase(folder: string, root: HTMLElement): Promise<void> {
  db.folder = folder;
  db.root = root;
  db.viewName = null;
  db.ready = false;
  db.panel = null;
  db.status = "";
  await load();
}

function resetWorking(r: ViewResult): void {
  const v = r.view;
  db.local = {
    layout: v.layout,
    visible: v.visible,
    date_property: v.date_property,
    end_date_property: v.end_date_property,
    cover_property: v.cover_property,
  };
  db.join = v.filter?.op ?? "and";
  db.draft = (v.filter?.rules ?? []).map((r) => ({
    property: r.property,
    op: r.op,
    text: r.value === undefined || r.value === null ? "" : fmt(r.value as CellValue),
  }));
  db.sorts = (v.sorts ?? []).map((s) => ({ ...s }));
  db.group = v.group_by ?? null;
  db.ready = true;
}

function propOf(name: string): PropertyDef | undefined {
  return db.result?.properties.find((p) => p.name === name);
}

const NEEDS_NO_VALUE = ["is_empty", "is_not_empty", "checked", "unchecked"];

/** Turn the filter form into the ViewQuery JSON Rust understands. */
function compileQuery(): ViewQuery {
  const rules: { property: string; op: string; value?: unknown }[] = [];
  for (const d of db.draft) {
    const p = propOf(d.property);
    if (!p) continue;
    if (NEEDS_NO_VALUE.includes(d.op)) {
      rules.push({ property: d.property, op: d.op });
      continue;
    }
    if (d.text.trim() === "") continue;
    const numeric =
      ["number", "formula", "rollup"].includes(p.type) &&
      ["is", "is_not", "gt", "ge", "lt", "le"].includes(d.op);
    if (d.op === "within_past_days" || d.op === "within_next_days") {
      const n = Math.trunc(Number(d.text));
      if (!Number.isNaN(n)) rules.push({ property: d.property, op: d.op, value: n });
    } else if (numeric) {
      const n = Number(d.text);
      if (!Number.isNaN(n)) rules.push({ property: d.property, op: d.op, value: n });
    } else {
      rules.push({ property: d.property, op: d.op, value: d.text.trim() });
    }
  }
  const q: ViewQuery = {};
  if (rules.length > 0) q.filter = { op: db.join, rules };
  if (db.sorts.length > 0) q.sorts = db.sorts;
  if (db.group) q.group_by = db.group;
  return q;
}

async function load(): Promise<void> {
  const root = db.root;
  if (!root) return;
  const token = ++db.token;
  try {
    const r = await api.databaseView(db.folder, db.viewName, db.ready ? compileQuery() : null);
    if (token !== db.token) return;
    db.result = r;
    if (!db.ready) resetWorking(r);
    render();
  } catch (err) {
    if (token !== db.token) return;
    root.replaceChildren(h("p", { class: "error pad", role: "alert", text: errorText(err) }));
  }
}

function effectiveView(): ViewDef {
  const r = db.result;
  const q = compileQuery();
  return { ...(r?.view as ViewDef), ...db.local, filter: q.filter, sorts: q.sorts, group_by: q.group_by } as ViewDef;
}

// ---------------------------------------------------------------- shell

function render(): void {
  const root = db.root;
  const r = db.result;
  if (!root || !r) return;
  const scroll = root.scrollTop;
  root.replaceChildren();
  const layout = db.local.layout ?? "table";

  const head = h("header", { class: "db-head" });
  head.append(h("h1", { class: "db-title", text: db.folder.split("/").pop() ?? db.folder }));
  head.append(
    h(
      "div",
      { class: "db-actions" },
      button("+ New row", () => void newRow(), "primary"),
      button("+ Property", () => void newProperty()),
      button("Columns", () => columnsDialog()),
    ),
  );
  root.append(head);

  // View tabs.
  const tabs = h("div", { class: "db-tabs", role: "tablist", label: "Views" });
  const views = r.views.length > 0 ? r.views : [r.view];
  for (const v of views) {
    const active = v.name === r.view.name;
    const t = h("button", { type: "button", class: "db-tab", text: v.name, role: "tab" });
    t.setAttribute("aria-selected", String(active));
    t.tabIndex = active ? 0 : -1;
    t.addEventListener("click", () => {
      db.viewName = v.name;
      db.ready = false;
      void load();
    });
    t.addEventListener("keydown", (e) => {
      const i = views.indexOf(v);
      const next = e.key === "ArrowRight" ? i + 1 : e.key === "ArrowLeft" ? i - 1 : null;
      const target = next === null ? undefined : views[(next + views.length) % views.length];
      if (target) {
        e.preventDefault();
        db.viewName = target.name;
        db.ready = false;
        void load();
      }
    });
    tabs.append(t);
  }
  tabs.append(button("+ View", () => void newView(), "db-tab add", "Add a view"));
  root.append(tabs);

  // Toolbar.
  const bar = h("div", { class: "db-toolbar" });
  const seg = h("div", { class: "segmented small-seg", role: "group", label: "Layout" });
  for (const l of LAYOUTS) {
    const b = h("button", { type: "button", text: l[0]?.toUpperCase() + l.slice(1) });
    b.setAttribute("aria-pressed", String(l === layout));
    b.addEventListener("click", () => setLayout(l));
    seg.append(b);
  }
  bar.append(seg);
  const nf = compileQuery().filter?.rules.length ?? 0;
  bar.append(
    toolButton(nf ? `Filter (${nf})` : "Filter", "filter"),
    toolButton(db.sorts.length ? `Sort (${db.sorts.length})` : "Sort", "sort"),
    toolButton(db.group ? `Group: ${db.group}` : "Group", "group"),
  );
  bar.append(layoutOptions(layout));
  bar.append(button("Save view", () => void saveView(), "primary"));
  bar.append(h("span", { class: "db-status muted small", role: "status", text: db.status }));
  root.append(bar);

  if (db.panel) root.append(renderPanel(db.panel));

  const body = h("div", { class: "db-body" });
  body.id = "db-body";
  root.append(body);
  switch (layout) {
    case "board":
      body.append(renderBoard(r));
      break;
    case "calendar":
      body.append(renderCalendar(r));
      break;
    case "list":
      body.append(renderList(r));
      break;
    case "gallery":
      body.append(renderGallery(r));
      break;
    case "timeline":
      body.append(renderTimeline(r));
      break;
    default:
      body.append(renderTable(r));
  }
  root.scrollTop = scroll;
}

function toolButton(text: string, panel: "filter" | "sort" | "group"): HTMLButtonElement {
  const b = button(text, () => {
    db.panel = db.panel === panel ? null : panel;
    render();
    if (db.panel) db.root?.querySelector<HTMLElement>(".db-panel select, .db-panel button")?.focus();
  });
  b.setAttribute("aria-expanded", String(db.panel === panel));
  return b;
}

function propsOfType(types: string[]): PropertyDef[] {
  return (db.result?.properties ?? []).filter((p) => types.includes(p.type));
}

function layoutOptions(layout: string): HTMLElement {
  const wrap = h("span", { class: "layout-opts" });
  const sel = (label: string, key: "date_property" | "end_date_property" | "cover_property", types: string[], none: boolean) => {
    const s = h("select", { label });
    if (none) s.append(h("option", { text: "(none)", attrs: { value: "" } }));
    for (const p of propsOfType(types)) s.append(h("option", { text: p.name, attrs: { value: p.name } }));
    s.value = db.local[key] ?? "";
    s.addEventListener("change", () => {
      db.local[key] = s.value === "" ? undefined : s.value;
      render();
    });
    wrap.append(h("label", { class: "small muted", text: `${label} ` }), s);
  };
  const dates = ["date", "created_time", "updated_time"];
  if (layout === "calendar") sel("Date", "date_property", dates, false);
  if (layout === "timeline") {
    sel("Start", "date_property", dates, false);
    sel("End", "end_date_property", dates, true);
  }
  if (layout === "gallery") sel("Cover", "cover_property", ["text", "url"], true);
  return wrap;
}

function setLayout(l: string): void {
  const r = db.result;
  if (!r) return;
  db.local.layout = l;
  const firstDate = propsOfType(["date"])[0]?.name;
  if (l === "calendar" || l === "timeline") {
    if (!db.local.date_property && firstDate) db.local.date_property = firstDate;
  }
  if (l === "timeline" && !db.local.end_date_property) {
    const second = propsOfType(["date"])[1]?.name;
    if (second) db.local.end_date_property = second;
  }
  if (l === "board" && !db.group) {
    const sel = propsOfType(["select"])[0]?.name;
    if (sel) {
      db.group = sel;
      void load();
      return;
    }
  }
  render();
}

// ---------------------------------------------------------------- filter / sort / group panel

const OPS: Record<string, [string, string][]> = {
  text: [
    ["is", "is"],
    ["is_not", "is not"],
    ["contains", "contains"],
    ["does_not_contain", "does not contain"],
    ["starts_with", "starts with"],
    ["ends_with", "ends with"],
    ["is_empty", "is empty"],
    ["is_not_empty", "is not empty"],
  ],
  number: [
    ["is", "="],
    ["is_not", "≠"],
    ["gt", ">"],
    ["ge", "≥"],
    ["lt", "<"],
    ["le", "≤"],
    ["is_empty", "is empty"],
    ["is_not_empty", "is not empty"],
  ],
  select: [
    ["is", "is"],
    ["is_not", "is not"],
    ["is_empty", "is empty"],
    ["is_not_empty", "is not empty"],
  ],
  list: [
    ["contains", "contains"],
    ["does_not_contain", "does not contain"],
    ["is_empty", "is empty"],
    ["is_not_empty", "is not empty"],
  ],
  date: [
    ["is", "is"],
    ["before", "before"],
    ["after", "after"],
    ["on_or_before", "on or before"],
    ["on_or_after", "on or after"],
    ["within_past_days", "within past days"],
    ["within_next_days", "within next days"],
    ["is_empty", "is empty"],
    ["is_not_empty", "is not empty"],
  ],
  checkbox: [
    ["checked", "is checked"],
    ["unchecked", "is not checked"],
  ],
};

function opsFor(p: PropertyDef | undefined): [string, string][] {
  const t = p?.type ?? "text";
  const key =
    t === "number" || t === "formula" || t === "rollup"
      ? "number"
      : t === "select"
        ? "select"
        : t === "multi_select" || t === "relation"
          ? "list"
          : t === "date" || t === "created_time" || t === "updated_time"
            ? "date"
            : t === "checkbox"
              ? "checkbox"
              : "text";
  return OPS[key] ?? OPS["text"] ?? [];
}

function propSelect(label: string, value: string, onPick: (v: string) => void, none = false): HTMLSelectElement {
  const s = h("select", { label });
  if (none) s.append(h("option", { text: "None", attrs: { value: "" } }));
  for (const p of db.result?.properties ?? []) s.append(h("option", { text: p.name, attrs: { value: p.name } }));
  s.value = value;
  s.addEventListener("change", () => onPick(s.value));
  return s;
}

function renderPanel(which: "filter" | "sort" | "group"): HTMLElement {
  const panel = h("section", { class: "db-panel", label: `${which} settings` });
  const apply = (): void => void load();
  if (which === "filter") {
    const join = h("select", { label: "Combine filters with" });
    join.append(h("option", { text: "All of (AND)", attrs: { value: "and" } }), h("option", { text: "Any of (OR)", attrs: { value: "or" } }));
    join.value = db.join;
    join.addEventListener("change", () => {
      db.join = join.value === "or" ? "or" : "and";
      apply();
    });
    panel.append(join);
    db.draft.forEach((d, i) => {
      const p = propOf(d.property);
      const row = h("div", { class: "db-rule" });
      row.append(
        propSelect("Filter property", d.property, (v) => {
          d.property = v;
          d.op = opsFor(propOf(v))[0]?.[0] ?? "is";
          d.text = "";
          render();
          apply();
        }),
      );
      const op = h("select", { label: "Condition" });
      for (const [k, l] of opsFor(p)) op.append(h("option", { text: l, attrs: { value: k } }));
      op.value = opsFor(p).some(([k]) => k === d.op) ? d.op : (opsFor(p)[0]?.[0] ?? "is");
      d.op = op.value;
      op.addEventListener("change", () => {
        d.op = op.value;
        render();
        apply();
      });
      row.append(op);
      if (!NEEDS_NO_VALUE.includes(d.op)) row.append(filterValue(d, p, apply));
      row.append(
        button(
          "✕",
          () => {
            db.draft.splice(i, 1);
            render();
            apply();
          },
          "icon-btn",
          "Remove filter",
        ),
      );
      panel.append(row);
    });
    panel.append(
      button("+ Add filter", () => {
        const first = db.result?.properties[0];
        if (!first) return;
        db.draft.push({ property: first.name, op: opsFor(first)[0]?.[0] ?? "is", text: "" });
        render();
      }),
    );
  } else if (which === "sort") {
    db.sorts.forEach((s, i) => {
      const row = h("div", { class: "db-rule" });
      row.append(
        propSelect("Sort property", s.property, (v) => {
          s.property = v;
          apply();
        }),
      );
      const dir = h("select", { label: "Direction" });
      dir.append(h("option", { text: "Ascending", attrs: { value: "asc" } }), h("option", { text: "Descending", attrs: { value: "desc" } }));
      dir.value = s.descending ? "desc" : "asc";
      dir.addEventListener("change", () => {
        s.descending = dir.value === "desc";
        apply();
      });
      row.append(
        dir,
        button(
          "✕",
          () => {
            db.sorts.splice(i, 1);
            render();
            apply();
          },
          "icon-btn",
          "Remove sort",
        ),
      );
      panel.append(row);
    });
    panel.append(
      button("+ Add sort", () => {
        const first = db.result?.properties[0];
        if (!first) return;
        db.sorts.push({ property: first.name, descending: false });
        render();
        apply();
      }),
    );
  } else {
    panel.append(
      h("label", { class: "small muted", text: "Group rows by " }),
      propSelect(
        "Group by property",
        db.group ?? "",
        (v) => {
          db.group = v === "" ? null : v;
          apply();
        },
        true,
      ),
    );
  }
  return panel;
}

function filterValue(d: DraftRule, p: PropertyDef | undefined, apply: () => void): HTMLElement {
  let el: HTMLInputElement | HTMLSelectElement;
  if (p?.type === "select") {
    el = h("select", { label: "Filter value" });
    el.append(h("option", { text: "", attrs: { value: "" } }));
    for (const o of p.options ?? []) el.append(h("option", { text: o.name, attrs: { value: o.name } }));
    el.value = d.text;
  } else {
    const isDate = ["date", "created_time", "updated_time"].includes(p?.type ?? "") && !d.op.startsWith("within");
    el = h("input", { type: isDate ? "date" : "text", label: "Filter value" });
    el.value = d.text;
  }
  el.addEventListener("change", () => {
    d.text = el.value;
    apply();
  });
  return el;
}

// ---------------------------------------------------------------- saving

async function saveView(): Promise<void> {
  const r = db.result;
  if (!r) return;
  try {
    const schema = await api.databaseSchema(db.folder);
    const v = effectiveView();
    const clean: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v)) {
      if (val === undefined || val === null) continue;
      if (Array.isArray(val) && val.length === 0) continue;
      clean[k] = val;
    }
    const views = [...(schema.table_views ?? [])];
    const i = views.findIndex((x) => x.name === v.name);
    if (i >= 0) views[i] = clean as unknown as ViewDef;
    else views.push(clean as unknown as ViewDef);
    schema.table_views = views;
    await api.databaseSchemaSave(db.folder, schema);
    db.viewName = v.name;
    db.status = `Saved view "${v.name}"`;
    db.ready = false;
    await load();
  } catch (err) {
    banner(`Could not save the view: ${errorText(err)}`);
  }
}

async function newView(): Promise<void> {
  const name = await promptText("New view", "View name");
  if (!name || !name.trim()) return;
  const r = db.result;
  if (!r) return;
  try {
    const schema = await api.databaseSchema(db.folder);
    const v = { ...effectiveView(), name: name.trim() };
    const clean: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v)) {
      if (val === undefined || val === null) continue;
      if (Array.isArray(val) && val.length === 0) continue;
      clean[k] = val;
    }
    if (schema.table_views.some((x) => x.name.toLowerCase() === name.trim().toLowerCase())) {
      banner(`A view named ${name.trim()} already exists.`);
      return;
    }
    if (schema.table_views.length === 0) {
      schema.table_views.push({ name: r.view.name, layout: r.view.layout });
    }
    schema.table_views.push(clean as unknown as ViewDef);
    await api.databaseSchemaSave(db.folder, schema);
    db.viewName = name.trim();
    db.ready = false;
    await load();
  } catch (err) {
    banner(`Could not add the view: ${errorText(err)}`);
  }
}

// ---------------------------------------------------------------- rows and properties

async function newRow(values: Record<string, CellValue> = {}): Promise<void> {
  const title = await promptText("New row", "Title");
  if (!title || !title.trim()) return;
  try {
    await api.rowCreate(db.folder, title.trim(), values);
    await load();
  } catch (err) {
    banner(`Could not add the row: ${errorText(err)}`);
  }
}

async function setCell(path: string, name: string, value: CellValue): Promise<void> {
  try {
    await api.rowSet(path, name, value);
    await load();
  } catch (err) {
    banner(`Could not save ${name}: ${errorText(err)}`);
    await load();
  }
}

const ROLLUP_FNS = [
  "count",
  "count_values",
  "sum",
  "average",
  "min",
  "max",
  "show_original",
  "percent_checked",
  "earliest",
  "latest",
];
const PROP_TYPES = [
  "text",
  "number",
  "select",
  "multi_select",
  "date",
  "checkbox",
  "url",
  "relation",
  "rollup",
  "formula",
  "created_time",
  "updated_time",
];

async function newProperty(): Promise<void> {
  const r = db.result;
  if (!r) return;
  let others: string[] = [];
  try {
    others = (await api.databases()).filter((d) => d.kind === "table").map((d) => d.folder);
  } catch {
    // the relation target list is optional
  }
  const m = modal("New property");
  const name = h("input", { type: "text", label: "Property name" });
  name.autocomplete = "off";
  const type = h("select", { label: "Property type" });
  for (const t of PROP_TYPES) type.append(h("option", { text: t.replace("_", " "), attrs: { value: t } }));
  const extra = h("div", { class: "modal-extra" });
  const err = h("p", { class: "error", role: "alert" });
  err.hidden = true;
  const field = (label: string, el: HTMLElement): HTMLElement => {
    const l = h("label", { class: "field-label", text: label });
    return h("div", {}, l, el);
  };
  const options = h("input", { type: "text", label: "Options, comma separated" });
  options.placeholder = "Todo, Doing, Done";
  const target = h("select", { label: "Target database" });
  for (const f of others) target.append(h("option", { text: f, attrs: { value: f } }));
  const rel = h("select", { label: "Relation property" });
  for (const p of r.properties.filter((x) => x.type === "relation")) rel.append(h("option", { text: p.name, attrs: { value: p.name } }));
  const rprop = h("input", { type: "text", label: "Property of the related rows" });
  const rfn = h("select", { label: "Rollup function" });
  for (const f of ROLLUP_FNS) rfn.append(h("option", { text: f.replace("_", " "), attrs: { value: f } }));
  const expr = h("input", { type: "text", label: "Formula" });
  expr.placeholder = 'prop("Price") * prop("Qty")';
  const sync = (): void => {
    extra.replaceChildren();
    if (type.value === "select" || type.value === "multi_select") extra.append(field("Options (comma separated)", options));
    else if (type.value === "relation") extra.append(field("Target database", target));
    else if (type.value === "rollup") extra.append(field("Relation", rel), field("Property of related rows", rprop), field("Function", rfn));
    else if (type.value === "formula") extra.append(field("Expression", expr));
  };
  type.addEventListener("change", sync);
  const form = h("form", { class: "modal-form" }, field("Name", name), field("Type", type), extra, err);
  form.append(h("div", { class: "row" }, h("button", { type: "submit", class: "primary", text: "Add property" }), button("Cancel", () => m.close())));
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const def: PropertyDef = { name: name.value.trim(), type: type.value };
    if (!def.name) return fail("Give the property a name.");
    if (def.type === "select" || def.type === "multi_select") {
      def.options = options.value
        .split(",")
        .map((x) => x.trim())
        .filter(Boolean)
        .map((n) => ({ name: n }));
    } else if (def.type === "relation") {
      if (!target.value) return fail("Pick a target database.");
      def.database = target.value;
    } else if (def.type === "rollup") {
      def.relation = rel.value;
      def.property = rprop.value.trim();
      def.function = rfn.value;
    } else if (def.type === "formula") {
      def.expression = expr.value.trim();
    }
    void (async () => {
      try {
        const schema = await api.databaseSchema(db.folder);
        schema.properties = [...schema.properties, def];
        await api.databaseSchemaSave(db.folder, schema);
        m.close();
        await load();
      } catch (e2) {
        fail(errorText(e2));
      }
    })();
  });
  function fail(msg: string): void {
    err.textContent = msg;
    err.hidden = false;
  }
  m.body.append(form);
  sync();
  name.focus();
}

function columnsDialog(): void {
  const r = db.result;
  if (!r) return;
  const m = modal("Columns");
  const current = new Set(db.local.visible && db.local.visible.length > 0 ? db.local.visible : r.properties.map((p) => p.name));
  const form = h("form", { class: "modal-form" });
  for (const p of r.properties) {
    const cb = h("input", { type: "checkbox" });
    cb.checked = current.has(p.name);
    cb.disabled = p.type === "title";
    cb.dataset.name = p.name;
    form.append(h("label", { class: "check-row" }, cb, document.createTextNode(` ${p.name}`)));
  }
  form.append(h("div", { class: "row" }, h("button", { type: "submit", class: "primary", text: "Apply" }), button("Cancel", () => m.close())));
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const picked = [...form.querySelectorAll<HTMLInputElement>("input[type=checkbox]")].filter((c) => c.checked).map((c) => c.dataset.name ?? "");
    db.local.visible = picked.length === r.properties.length ? undefined : picked;
    m.close();
    render();
  });
  m.body.append(form);
  form.querySelector<HTMLElement>("input:not([disabled])")?.focus();
}

function visibleProps(r: ViewResult): PropertyDef[] {
  const vis = db.local.visible;
  const list = vis && vis.length > 0 ? r.properties.filter((p) => vis.includes(p.name) || p.type === "title") : r.properties;
  return list;
}

function titleButton(row: DbRow): HTMLButtonElement {
  const b = h("button", { type: "button", class: "row-title", text: row.title || "Untitled" });
  b.addEventListener("click", () => db.host?.openPage(row.path));
  return b;
}

function chips(row: DbRow, r: ViewResult, limit: number, skip: string[] = []): HTMLElement {
  const box = h("span", { class: "row-chips" });
  let n = 0;
  for (const p of visibleProps(r)) {
    if (p.type === "title" || skip.includes(p.name) || n >= limit) continue;
    const t = fmt(row.values[p.name]);
    if (!t) continue;
    box.append(h("span", { class: "chip", text: `${p.name}: ${t}` }));
    n++;
  }
  return box;
}

// ---------------------------------------------------------------- table

function renderTable(r: ViewResult): HTMLElement {
  const wrap = h("div", { class: "db-table-wrap" });
  const cols = visibleProps(r);
  const table = h("table", { class: "db-table", label: `${db.folder} rows` });
  const thead = h("thead");
  const hr = h("tr");
  for (const p of cols) {
    const th = h("th", { text: p.name, title: `${p.name} (${p.type.replace("_", " ")})` });
    th.scope = "col";
    hr.append(th);
  }
  thead.append(hr);
  table.append(thead);
  const tbody = h("tbody");
  const rowEl = (row: DbRow): HTMLElement => {
    const tr = h("tr");
    for (const p of cols) {
      const td = h("td");
      if (p.type === "title") td.append(titleButton(row));
      else td.append(propertyCell(p, row, r, (n, v) => void setCell(row.path, n, v)));
      tr.append(td);
    }
    return tr;
  };
  if (r.groups) {
    for (const g of r.groups) {
      const tr = h("tr", { class: "group-row" });
      const td = h("td", { text: `${g.label} (${g.rows.length})` });
      td.colSpan = Math.max(cols.length, 1);
      tr.append(td);
      tbody.append(tr);
      for (const i of g.rows) {
        const row = r.rows[i];
        if (row) tbody.append(rowEl(row));
      }
    }
  } else {
    for (const row of r.rows) tbody.append(rowEl(row));
  }
  table.append(tbody);
  wrap.append(table);
  if (r.rows.length === 0) wrap.append(h("p", { class: "muted pad", text: "No rows match. Add one with + New row." }));
  wrap.append(button("+ New row", () => void newRow(), "add-row"));
  return wrap;
}

// ---------------------------------------------------------------- board

function renderBoard(r: ViewResult): HTMLElement {
  const group = db.group;
  const gp = group ? propOf(group) : undefined;
  const wrap = h("div", { class: "board" });
  if (!group || !r.groups) {
    wrap.append(h("p", { class: "muted pad", text: "Choose a property to group by (Group button) to see columns." }));
    return wrap;
  }
  const columns: { label: string; rows: DbRow[] }[] = r.groups.map((g) => ({
    label: g.label,
    rows: g.rows.map((i) => r.rows[i]).filter((x): x is DbRow => !!x),
  }));
  for (const o of gp?.options ?? []) {
    if (!columns.some((c) => c.label === o.name)) columns.push({ label: o.name, rows: [] });
  }
  const movable = gp?.type === "select";
  const move = (row: DbRow, label: string): void => {
    void setCell(row.path, group, label === NO_VALUE ? null : label);
  };
  for (const col of columns) {
    const colEl = h("section", { class: "board-col", label: `${col.label} column` });
    colEl.dataset.label = col.label;
    colEl.append(h("h3", { class: "board-head", text: `${col.label} · ${col.rows.length}` }));
    const list = h("div", { class: "board-list" });
    for (const row of col.rows) {
      const card = h("div", { class: "card" });
      card.draggable = movable;
      card.dataset.path = row.path;
      card.addEventListener("dragstart", (e) => {
        e.dataTransfer?.setData("text/plain", row.path);
        card.classList.add("dragging");
        (db as unknown as { drag: DbRow }).drag = row;
      });
      card.addEventListener("dragend", () => card.classList.remove("dragging"));
      card.append(titleButton(row), chips(row, r, 3, [group]));
      if (movable) {
        const sel = h("select", { label: `Move ${row.title} to column`, class: "card-move" });
        for (const c of columns) sel.append(h("option", { text: c.label, attrs: { value: c.label } }));
        sel.value = col.label;
        sel.addEventListener("change", () => move(row, sel.value));
        card.append(sel);
      }
      list.append(card);
    }
    colEl.append(list);
    if (movable) {
      colEl.addEventListener("dragover", (e) => {
        e.preventDefault();
        colEl.classList.add("drop-target");
      });
      colEl.addEventListener("dragleave", () => colEl.classList.remove("drop-target"));
      colEl.addEventListener("drop", (e) => {
        e.preventDefault();
        colEl.classList.remove("drop-target");
        const row = (db as unknown as { drag?: DbRow }).drag;
        if (row && (row.values[group] ?? NO_VALUE) !== col.label) move(row, col.label);
      });
    }
    colEl.append(
      button("+ Add", () => void newRow(col.label === NO_VALUE ? {} : { [group]: col.label }), "board-add", `Add a row to ${col.label}`),
    );
    wrap.append(colEl);
  }
  return wrap;
}

// ---------------------------------------------------------------- calendar

function renderCalendar(r: ViewResult): HTMLElement {
  const wrap = h("div", { class: "calendar" });
  const dp = db.local.date_property;
  if (!dp) {
    wrap.append(h("p", { class: "muted pad", text: "Add a date property to use the calendar." }));
    return wrap;
  }
  const y = db.month.getFullYear();
  const m = db.month.getMonth();
  const nav = h("div", { class: "cal-nav" });
  const shift = (d: number): void => {
    db.month = new Date(y, m + d, 1);
    render();
  };
  nav.append(
    button("◀", () => shift(-1), "icon-btn", "Previous month"),
    h("strong", { class: "cal-title", text: new Date(y, m, 1).toLocaleDateString(undefined, { month: "long", year: "numeric" }) }),
    button("▶", () => shift(1), "icon-btn", "Next month"),
    button("Today", () => {
      db.month = new Date();
      render();
    }),
  );
  wrap.append(nav);
  const byDay = new Map<string, DbRow[]>();
  const undated: DbRow[] = [];
  for (const row of r.rows) {
    const d = parseDay(row.values[dp]);
    if (d === null) undated.push(row);
    else {
      const k = dayString(d);
      byDay.set(k, [...(byDay.get(k) ?? []), row]);
    }
  }
  const grid = h("div", { class: "cal-grid", role: "grid", label: "Month" });
  for (const n of ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]) grid.append(h("div", { class: "cal-dow", text: n, role: "columnheader" }));
  const first = Date.UTC(y, m, 1);
  const offset = (new Date(first).getUTCDay() + 6) % 7;
  const total = Math.ceil((offset + new Date(y, m + 1, 0).getDate()) / 7) * 7;
  for (let i = 0; i < total; i++) {
    const day = first + (i - offset) * DAY;
    const inMonth = new Date(day).getUTCMonth() === m;
    const cell = h("div", { class: inMonth ? "cal-cell" : "cal-cell other", role: "gridcell" });
    cell.append(h("span", { class: "cal-day", text: String(new Date(day).getUTCDate()) }));
    for (const row of byDay.get(dayString(day)) ?? []) cell.append(titleButton(row));
    grid.append(cell);
  }
  wrap.append(grid);
  if (undated.length > 0) {
    const un = h("section", { class: "undated", label: "Rows without a date" }, h("h3", { text: "No date" }));
    for (const row of undated) un.append(titleButton(row));
    wrap.append(un);
  }
  return wrap;
}

// ---------------------------------------------------------------- list and gallery

function renderList(r: ViewResult): HTMLElement {
  const ul = h("ul", { class: "db-list", label: "Rows" });
  for (const row of r.rows) ul.append(h("li", { class: "db-list-item" }, titleButton(row), chips(row, r, 99)));
  if (r.rows.length === 0) ul.append(h("li", { class: "muted pad", text: "No rows match." }));
  return h("div", {}, ul, button("+ New row", () => void newRow(), "add-row"));
}

function renderGallery(r: ViewResult): HTMLElement {
  const grid = h("div", { class: "gallery" });
  const cp = db.local.cover_property;
  for (const row of r.rows) {
    const card = h("div", { class: "gcard" });
    const cover = h("div", { class: "gcover" });
    const ref = cp ? row.values[cp] : null;
    if (typeof ref === "string" && ref) {
      void api
        .assetUrl(row.path, ref)
        .then((url) => {
          if (!url) return;
          const img = h("img", { attrs: { alt: "" } });
          img.src = url;
          cover.replaceChildren(img);
        })
        .catch(() => undefined);
    }
    card.append(cover, titleButton(row), chips(row, r, 2, cp ? [cp] : []));
    grid.append(card);
  }
  if (r.rows.length === 0) grid.append(h("p", { class: "muted pad", text: "No rows match." }));
  return h("div", {}, grid, button("+ New row", () => void newRow(), "add-row"));
}

// ---------------------------------------------------------------- timeline

function renderTimeline(r: ViewResult): HTMLElement {
  const wrap = h("div", { class: "timeline" });
  const sp = db.local.date_property;
  const ep = db.local.end_date_property;
  if (!sp) {
    wrap.append(h("p", { class: "muted pad", text: "Add a date property to use the timeline." }));
    return wrap;
  }
  const dated: { row: DbRow; s: number; e: number }[] = [];
  const undated: DbRow[] = [];
  for (const row of r.rows) {
    const s = parseDay(row.values[sp]);
    if (s === null) {
      undated.push(row);
      continue;
    }
    const e = ep ? (parseDay(row.values[ep]) ?? s) : s;
    dated.push({ row, s, e: Math.max(s, e) });
  }
  if (db.tlStart === null) {
    const base = dated.length > 0 ? Math.min(...dated.map((d) => d.s)) : Date.now();
    const d = new Date(base);
    const dayStart = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
    db.tlStart = dayStart - ((new Date(dayStart).getUTCDay() + 6) % 7) * DAY;
  }
  const start = db.tlStart;
  const nav = h("div", { class: "cal-nav" });
  const shift = (days: number): void => {
    db.tlStart = start + days * DAY;
    render();
  };
  nav.append(
    button("◀", () => shift(-14), "icon-btn", "Earlier"),
    h("strong", { class: "cal-title", text: `${dayString(start)} to ${dayString(start + 41 * DAY)}` }),
    button("▶", () => shift(14), "icon-btn", "Later"),
  );
  wrap.append(nav);
  const grid = h("div", { class: "tl-grid", label: "Timeline, six weeks" });
  const head = h("div", { class: "tl-row tl-head" }, h("div", { class: "tl-label" }));
  for (let w = 0; w < 6; w++) {
    const c = h("div", { class: "tl-week", text: dayString(start + w * 7 * DAY).slice(5) });
    c.style.gridColumn = `${w * 7 + 2} / span 7`;
    head.append(c);
  }
  grid.append(head);
  let shown = 0;
  for (const d of dated) {
    const s = Math.round((d.s - start) / DAY);
    const e = Math.round((d.e - start) / DAY);
    if (e < 0 || s > 41) continue;
    shown++;
    const row = h("div", { class: "tl-row" });
    const lab = h("div", { class: "tl-label" }, titleButton(d.row));
    const bar = h("div", { class: "tl-bar", title: `${d.row.title}: ${dayString(d.s)} to ${dayString(d.e)}` });
    bar.style.gridColumn = `${Math.max(s, 0) + 2} / ${Math.min(e, 41) + 3}`;
    bar.setAttribute("role", "img");
    bar.setAttribute("aria-label", `${d.row.title}, ${dayString(d.s)} to ${dayString(d.e)}`);
    row.append(lab, bar);
    grid.append(row);
  }
  if (shown === 0) grid.append(h("p", { class: "muted pad", text: "No dated rows in this window." }));
  wrap.append(grid);
  if (undated.length > 0) {
    const un = h("section", { class: "undated", label: "Rows without dates" }, h("h3", { text: "No dates" }));
    for (const row of undated) un.append(titleButton(row));
    wrap.append(un);
  }
  return wrap;
}

export { titleName };
