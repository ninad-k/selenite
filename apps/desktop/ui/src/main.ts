import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

interface AppInfo {
  name: string;
  version: string;
}

async function init(): Promise<void> {
  const title = document.getElementById("app-title");
  try {
    const info = await invoke<AppInfo>("app_info");
    document.title = info.name;
    if (title) title.textContent = `${info.name} v${info.version}`;
  } catch (err) {
    if (title) title.textContent = `Unable to load app info: ${String(err)}`;
  }
}

void init();
