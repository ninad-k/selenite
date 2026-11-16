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
};

export function errorText(err: unknown): string {
  return typeof err === "string" ? err : err instanceof Error ? err.message : String(err);
}
