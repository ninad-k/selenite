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
};

export function errorText(err: unknown): string {
  return typeof err === "string" ? err : err instanceof Error ? err.message : String(err);
}
