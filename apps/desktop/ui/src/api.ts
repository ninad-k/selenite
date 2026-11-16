import { invoke } from "@tauri-apps/api/core";

export interface AppInfo {
  name: string;
  version: string;
}
export interface VaultInfo {
  root: string;
  name: string;
  mode_id: string;
  mode_label: string;
  page_count: number;
  modes: ModeInfo[];
}
export interface ModeInfo {
  id: string;
  label: string;
}
export interface TreeNode {
  name: string;
  path: string;
  is_dir: boolean;
  children: TreeNode[];
}
export interface PageContent {
  text: string;
  revision: string;
}
export interface SearchHit {
  path: string;
  title: string;
  snippet: string;
}
export interface Backlink {
  source: string;
  source_title: string;
  line: number;
  context: string;
}
export interface OutLink {
  target: string;
  resolved: string | null;
  line: number;
}
export interface TagCount {
  tag: string;
  count: number;
}

export interface TaskDto {
  path: string;
  title: string;
  done: boolean;
  priority: number;
  due: string | null;
  recurring: boolean;
  recurrence: string | null;
  reminders: string[];
  project: string | null;
  section: string | null;
  labels: string[];
  completed_at: string | null;
}
export interface TaskDetail {
  fields: TaskDto;
  body: string;
}
export interface QuickAddResult {
  path: string;
  warnings: string[];
}
export interface AgendaDay {
  date: string;
  tasks: TaskDto[];
}
export interface Agenda {
  overdue: TaskDto[];
  days: AgendaDay[];
}
export interface ProjectCount {
  project: string;
  open: number;
}
export interface SavedView {
  name: string;
  filter: string;
  layout: string;
}
export interface RemindersStatus {
  kind: string;
  scheduled: number;
  last_error: string | null;
}
/** Fields a task edit may change; `null` clears an optional field. */
export interface TaskPatch {
  title?: string;
  priority?: number;
  due?: string | null;
  project?: string | null;
  section?: string | null;
  labels?: string[];
  reminders?: string[];
}

// ---- Workspace
export interface RenderedBlock {
  index: number;
  kind: string;
  anchor: string | null;
  markdown: string;
  html: string;
}
export interface BlockPage {
  path: string;
  title: string;
  revision: string;
  icon: string | null;
  cover: string | null;
  blocks: RenderedBlock[];
}
export interface PageNode {
  name: string;
  path: string;
  kind: "page" | "database" | "tasks" | "folder";
  icon: string | null;
  children: PageNode[];
}
export interface BlockType {
  kind: string;
  label: string;
  hint: string;
  markdown: string;
}
export type BlockEdit =
  | { op: "replace"; block: string; markdown: string }
  | { op: "insert"; after: string | null; markdown: string }
  | { op: "delete"; block: string }
  | { op: "move"; block: string; to: number }
  | { op: "toggle_task"; block: string; line: number };

// ---- Databases
export interface SelectOption {
  name: string;
  color?: string;
}
/** A property definition as stored in `_schema.yaml` (type-specific keys are flattened). */
export interface PropertyDef {
  name: string;
  type: string;
  options?: SelectOption[];
  database?: string;
  relation?: string;
  property?: string;
  function?: string;
  expression?: string;
  format?: string;
}
export interface FilterRule {
  property: string;
  op: string;
  value?: unknown;
}
export interface FilterGroup {
  op: "and" | "or";
  rules: FilterRule[];
}
export interface SortRule {
  property: string;
  descending: boolean;
}
export interface ViewQuery {
  filter?: FilterGroup;
  sorts?: SortRule[];
  group_by?: string;
}
export interface ViewDef extends ViewQuery {
  name: string;
  layout: string;
  visible?: string[];
  date_property?: string;
  end_date_property?: string;
  cover_property?: string;
}
export type CellValue = null | boolean | number | string | CellValue[];
export interface DbRow {
  path: string;
  title: string;
  values: Record<string, CellValue>;
}
export interface ViewResult {
  folder: string;
  properties: PropertyDef[];
  views: ViewDef[];
  view: ViewDef;
  rows: DbRow[];
  groups: { label: string; rows: number[] }[] | null;
}
export interface DatabaseInfo {
  folder: string;
  kind: string;
  rows: number;
}
export interface DatabaseSchemaJson {
  kind: string;
  version: number;
  properties: PropertyDef[];
  table_views: ViewDef[];
  [k: string]: unknown;
}

// ---- Cloud sync
export interface RemoteInfo {
  id: string;
  provider: string;
  provider_label: string;
  folder: string;
  account_hint: string | null;
  last_sync: number | null;
}
export interface ConflictCopy {
  path: string;
  copy_path: string;
  local_copy_path: string;
}
export interface SyncSummary {
  remotes: number;
  pushed: number;
  pulled: number;
  trashed: number;
  conflicts: ConflictCopy[];
  unresolved: string[];
  errors: string[];
  offline: boolean;
}
export interface SyncStatus {
  running: boolean;
  remotes: number;
  last_run: number | null;
  last_report: SyncSummary | null;
  pending: number;
  error: string | null;
}

/** Thin wrappers over Tauri commands. Errors arrive as plain strings. */
export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  openVault: (path: string) => invoke<VaultInfo>("open_vault", { path }),
  createVault: (path: string) => invoke<VaultInfo>("create_vault", { path }),
  lastVault: () => invoke<string | null>("last_vault"),
  vaultInfo: () => invoke<VaultInfo>("vault_info"),
  tree: () => invoke<TreeNode>("tree"),
  readPage: (path: string) => invoke<PageContent>("read_page", { path }),
  savePage: (path: string, text: string, revision: string) =>
    invoke<{ revision: string }>("save_page", { path, text, revision }),
  createPage: (title: string, folder?: string) =>
    invoke<string>("create_page", { title, folder: folder ?? null }),
  createPageAt: (path: string) => invoke<string>("create_page_at", { path }),
  resolve: (target: string) => invoke<string | null>("resolve", { target }),
  search: (query: string, limit: number) => invoke<SearchHit[]>("search", { query, limit }),
  backlinks: (path: string) => invoke<Backlink[]>("backlinks", { path }),
  outgoing: (path: string) => invoke<OutLink[]>("outgoing", { path }),
  tags: () => invoke<TagCount[]>("tags"),
  dailyNote: () => invoke<string>("daily_note"),
  refresh: () => invoke<void>("refresh"),
  setMode: (mode: string) => invoke<VaultInfo>("set_mode", { mode }),
  tasksQuery: (expr: string) => invoke<TaskDto[]>("tasks_query", { expr }),
  taskQuickAdd: (text: string) => invoke<QuickAddResult>("task_quick_add", { text }),
  taskComplete: (path: string) => invoke<TaskDto>("task_complete", { path }),
  taskReopen: (path: string) => invoke<TaskDto>("task_reopen", { path }),
  taskArchive: (path: string) => invoke<void>("task_archive", { path }),
  taskUpdate: (path: string, patch: TaskPatch) => invoke<TaskDto>("task_update", { path, patch }),
  taskGet: (path: string) => invoke<TaskDetail>("task_get", { path }),
  taskProjects: () => invoke<ProjectCount[]>("task_projects"),
  taskLabels: () => invoke<string[]>("task_labels"),
  taskAgenda: (days: number) => invoke<Agenda>("task_agenda", { days }),
  taskViews: () => invoke<SavedView[]>("task_views"),
  taskViewSave: (name: string, filter: string, layout: string) =>
    invoke<SavedView[]>("task_view_save", { name, filter, layout }),
  remindersStatus: () => invoke<RemindersStatus>("reminders_status"),
  blockTypes: () => invoke<BlockType[]>("block_types"),
  assetUrl: (page: string, target: string) => invoke<string | null>("asset_url", { page, target }),
  pageBlocks: (path: string) => invoke<BlockPage>("page_blocks", { path }),
  editBlock: (path: string, edit: BlockEdit, revision: string) =>
    invoke<{ revision: string }>("edit_block", { path, edit, revision }),
  workspacePages: () => invoke<PageNode[]>("workspace_pages"),
  createSubpage: (parent: string | null, title: string) =>
    invoke<string>("create_subpage", { parent, title }),
  /** `undefined` keeps a field, an empty string clears it. */
  setPageMeta: (path: string, icon?: string, cover?: string) =>
    invoke<{ revision: string }>("set_page_meta", { path, icon: icon ?? null, cover: cover ?? null }),
  databases: () => invoke<DatabaseInfo[]>("databases"),
  databaseSchema: (folder: string) => invoke<DatabaseSchemaJson>("database_schema", { folder }),
  databaseView: (folder: string, view: string | null, query: ViewQuery | null) =>
    invoke<ViewResult>("database_view", { folder, view, query }),
  databaseCreate: (folder: string, properties: PropertyDef[]) =>
    invoke<void>("database_create", { folder, properties }),
  databaseSchemaSave: (folder: string, schema: DatabaseSchemaJson) =>
    invoke<void>("database_schema_save", { folder, schema }),
  rowCreate: (folder: string, title: string, values: Record<string, CellValue>) =>
    invoke<string>("row_create", { folder, title, values }),
  rowSet: (path: string, property: string, value: CellValue) =>
    invoke<void>("row_set", { path, property, value }),
  remotesList: () => invoke<RemoteInfo[]>("remotes_list"),
  remoteConnect: (provider: string, folder: string | null) =>
    invoke<RemoteInfo>("remote_connect", { provider, folder }),
  remoteDisconnect: (id: string) => invoke<void>("remote_disconnect", { id }),
  syncNow: (id: string | null) => invoke<SyncSummary>("sync_now", { id }),
  syncStatus: () => invoke<SyncStatus>("sync_status"),
  syncAuto: (reason: "open" | "focus") => invoke<boolean>("sync_auto", { reason }),
};

export function errorText(err: unknown): string {
  return typeof err === "string" ? err : err instanceof Error ? err.message : String(err);
}
