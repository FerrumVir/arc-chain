import { exit } from "@tauri-apps/plugin-process";
import { api, isTauri } from "./tauri";

export async function quitArc(): Promise<void> {
  if (!isTauri) {
    window.location.reload();
    return;
  }
  await api.stopNode().catch(() => {});
  await exit(0).catch(() => {});
}

export async function closeThisCopy(): Promise<void> {
  if (isTauri) await exit(0);
  else window.location.reload();
}
