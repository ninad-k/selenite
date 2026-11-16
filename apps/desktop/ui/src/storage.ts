// Cloud storage UI: the sync status in the status bar and the Storage section of the settings
// dialog. No sync logic lives here; every action is a Tauri command. All text via textContent.

import "./storage.css";
import { listen } from "@tauri-apps/api/event";
import { api, errorText, type RemoteInfo, type SyncStatus } from "./api";
import { $, button, h, modal, type ModalHandle } from "./dom";

export interface StorageHooks {
  /** Open a vault path as a page (switches to Vault mode). */
  openPage: (path: string) => void;
  /** Show a message banner. */
  banner: (text: string) => void;
  /** The vault files or index changed because of a sync; reload the visible lists. */
  afterSync: () => void;
}

const CLOCK_MS = 30_000;

let hooks: StorageHooks | null = null;
let status: SyncStatus | null = null;
let settings: { handle: ModalHandle; render: () => Promise<void>; message: string } | null = null;
let connecting = false;
let syncingId: string | null | undefined; // undefined = idle, null = all remotes

/** "just now", "2 min ago", "3 h ago", "5 d ago". */
export function ago(unixSecs: number, nowMs = Date.now()): string {
  const s = Math.max(0, Math.floor(nowMs / 1000 - unixSecs));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86_400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86_400)} d ago`;
}

interface Line {
  text: string;
  state: "synced" | "syncing" | "conflict" | "offline" | "error";
  title: string;
}

/** The status-bar wording for a sync status, or null when there is nothing to show. */
export function describe(st: SyncStatus | null, busy: boolean, nowMs = Date.now()): Line | null {
  if (!st || st.remotes === 0) return null;
  if (st.running || busy) return { text: "Syncing…", state: "syncing", title: "Sync in progress" };
  const conflicts = st.last_report?.unresolved.length ?? 0;
  if (conflicts > 0) {
    return {
      text: conflicts === 1 ? "1 conflict" : `${conflicts} conflicts`,
      state: "conflict",
      title: "Both versions were kept. Click to review the conflict copies.",
    };
  }
  if (st.last_report?.offline) {
    const n = st.pending;
    return {
      text: `Offline — ${n} ${n === 1 ? "change" : "changes"} waiting`,
      state: "offline",
      title: "The cloud could not be reached. Changes sync when it is back.",
    };
  }
  if (st.error) return { text: "Sync problem", state: "error", title: st.error };
  if (st.last_run !== null) {
    return {
      text: `Synced ${ago(st.last_run, nowMs)}`,
      state: "synced",
      title: "Click to open storage settings",
    };
  }
  return { text: "Not synced yet", state: "synced", title: "Click to open storage settings" };
}

async function loadStatus(): Promise<void> {
  try {
    status = await api.syncStatus();
  } catch {
    status = null;
  }
  renderStatus();
}

function renderStatus(): void {
  const el = $<HTMLButtonElement>("status-sync");
  const line = describe(status, syncingId !== undefined || connecting);
  el.hidden = !line;
  if (!line) return;
  el.textContent = line.text;
  el.dataset.state = line.state;
  el.title = line.title;
}

// ---------------------------------------------------------------- conflicts

function showConflicts(): void {
  const report = status?.last_report;
  const paths = report?.unresolved ?? [];
  const copies = new Map<string, string[]>();
  for (const c of report?.conflicts ?? []) copies.set(c.path, [c.copy_path, c.local_copy_path]);
  const m = modal("Conflicts");
  m.body.append(
    h("p", {
      class: "muted small",
      text: "Both versions of these pages were kept. Open them, decide what to keep, and delete the extra copy.",
    }),
  );
  const list = h("ul", { class: "conflict-list" });
  const open = (p: string): void => {
    m.close();
    hooks?.openPage(p);
  };
  for (const p of paths) {
    const li = h("li");
    li.append(button(p, () => open(p), "", `Open ${p}`));
    for (const copy of copies.get(p) ?? []) {
      const b = button(`${copy}`, () => open(copy), "", `Open conflict copy ${copy}`);
      b.classList.add("small");
      li.append(b);
    }
    list.append(li);
  }
  if (paths.length === 0) m.body.append(h("p", { class: "muted", text: "No conflicts." }));
  m.body.append(list, h("div", { class: "row" }, button("Close", () => m.close())));
}

// ---------------------------------------------------------------- settings dialog

function summaryText(s: { pushed: number; pulled: number; errors: string[]; offline: boolean }): string {
  if (s.offline) return "Offline: changes are waiting.";
  const base = `Sent ${s.pushed}, received ${s.pulled}.`;
  return s.errors.length > 0 ? `${base} ${s.errors.length} problem(s): ${s.errors[0]}` : base;
}

function confirmDisconnect(r: RemoteInfo, then: () => void): void {
  const m = modal(`Disconnect ${r.provider_label}?`);
  m.body.append(
    h("p", {
      text: "This device will stop syncing with this account and forget the sign-in. Files already stored there are kept; nothing is deleted from the cloud or from this vault.",
    }),
  );
  const yes = h("button", { type: "button", class: "primary", text: "Disconnect" });
  yes.addEventListener("click", () => {
    m.close();
    then();
  });
  m.body.append(h("div", { class: "row" }, yes, button("Cancel", () => m.close())));
}

async function runSync(id: string | null): Promise<void> {
  if (syncingId !== undefined) return;
  syncingId = id;
  renderStatus();
  if (settings) void settings.render();
  try {
    const s = await api.syncNow(id);
    if (settings) settings.message = summaryText(s);
  } catch (err) {
    if (settings) settings.message = `Sync did not run: ${errorText(err)}`;
  } finally {
    syncingId = undefined;
    await loadStatus();
    hooks?.afterSync();
    if (settings) void settings.render();
  }
}

async function connect(provider: "google-drive" | "onedrive"): Promise<void> {
  if (connecting) return;
  connecting = true;
  if (settings) {
    settings.message = "Waiting for sign-in in your browser…";
    void settings.render();
  }
  try {
    const r = await api.remoteConnect(provider, null);
    if (settings) settings.message = `Connected ${r.provider_label}. Syncing…`;
    connecting = false;
    await loadStatus();
    void runSync(r.id);
  } catch (err) {
    if (settings) settings.message = `Could not connect: ${errorText(err)}`;
  } finally {
    connecting = false;
    await loadStatus();
    if (settings) void settings.render();
  }
}

function remoteRow(r: RemoteInfo): HTMLLIElement {
  const busy = syncingId !== undefined || connecting;
  const when = r.last_sync !== null ? `Last synced ${ago(r.last_sync)}` : "Not synced yet in this session";
  const main = h(
    "div",
    { class: "remote-main" },
    h("span", { class: "remote-title", text: `${r.provider_label} · ${r.folder}` }),
    h("span", { class: "small muted", text: r.account_hint ? `${r.account_hint} · ${when}` : when }),
  );
  const sync = button(
    syncingId === r.id || syncingId === null ? "Syncing…" : "Sync now",
    () => void runSync(r.id),
    "",
    `Sync ${r.provider_label} now`,
  );
  sync.disabled = busy;
  const off = button(
    "Disconnect",
    () =>
      confirmDisconnect(r, () => {
        void (async () => {
          try {
            await api.remoteDisconnect(r.id);
            if (settings) settings.message = `Disconnected ${r.provider_label}. Cloud files were kept.`;
          } catch (err) {
            if (settings) settings.message = `Could not disconnect: ${errorText(err)}`;
          }
          await loadStatus();
          if (settings) void settings.render();
        })();
      }),
    "",
    `Disconnect ${r.provider_label}`,
  );
  off.disabled = busy;
  return h("li", { class: "remote-item" }, main, h("div", { class: "remote-actions" }, sync, off));
}

function openSettings(): void {
  if (settings) return;
  const handle = modal("Settings", () => {
    settings = null;
  });
  handle.body.parentElement?.classList.add("settings");
  const state = { handle, message: "", render: async (): Promise<void> => {} };
  state.render = async (): Promise<void> => {
    let remotes: RemoteInfo[] = [];
    try {
      remotes = await api.remotesList();
    } catch (err) {
      state.message = `Could not read remotes: ${errorText(err)}`;
    }
    if (settings !== state) return;
    const busy = syncingId !== undefined || connecting;
    const body = handle.body;
    body.replaceChildren();
    body.append(h("h3", { text: "Storage" }));
    body.append(
      h("p", {
        class: "small muted",
        text: "Your notes stay plain files on this computer. You can also keep a copy in your own cloud drive so your devices stay in step. It is your account and your storage; nothing goes through our servers.",
      }),
    );
    if (remotes.length === 0) {
      body.append(h("p", { text: "No cloud drive connected." }));
    } else {
      const list = h("ul", { class: "remote-list" });
      for (const r of remotes) list.append(remoteRow(r));
      body.append(list);
    }
    const msg = h("p", { class: "small", role: "status", text: state.message });
    msg.setAttribute("aria-live", "polite");
    body.append(msg);
    body.append(h("h3", { text: "Connect a cloud drive" }));
    body.append(
      h("p", {
        class: "small muted",
        text: "Google Drive: the app can only see files it creates itself, never the rest of your Drive.",
      }),
      h("p", {
        class: "small muted",
        text: "OneDrive: the app gets its own private app folder and cannot see anything else in your OneDrive.",
      }),
      h("p", {
        class: "small muted",
        text: "Sign-in opens in your browser. Disconnecting later keeps the files that are already in the cloud.",
      }),
    );
    const g = button("Connect Google Drive", () => void connect("google-drive"));
    const o = button("Connect OneDrive", () => void connect("onedrive"));
    g.disabled = busy;
    o.disabled = busy;
    body.append(h("div", { class: "connect-row" }, g, o));
    body.append(h("div", { class: "row" }, button("Close", () => handle.close())));
  };
  settings = state;
  void state.render();
}

/** Wire the gear button, the status bar and the `sync-changed` event. */
export function initStorage(h2: StorageHooks): void {
  hooks = h2;
  $("btn-settings").addEventListener("click", openSettings);
  $("status-sync").addEventListener("click", () => {
    const conflicts = status?.last_report?.unresolved.length ?? 0;
    if (conflicts > 0) showConflicts();
    else openSettings();
  });
  void listen("sync-changed", () => {
    void loadStatus().then(() => {
      hooks?.afterSync();
      if (settings) void settings.render();
    });
  });
  window.setInterval(renderStatus, CLOCK_MS);
}

/** Called when a vault opens: show the status and let the backend decide whether to sync. */
export async function onVaultOpened(): Promise<void> {
  await loadStatus();
  try {
    if (await api.syncAuto("open")) await loadStatus();
  } catch {
    // sync problems are shown in the status bar
  }
}

/** Called when the window regains focus (the backend skips it if the last sync is recent). */
export async function onFocusSync(): Promise<void> {
  try {
    if (await api.syncAuto("focus")) await loadStatus();
  } catch {
    // shown through sync_status
  }
  await loadStatus();
}
