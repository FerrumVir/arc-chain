import { exit } from "@tauri-apps/plugin-process";
import { api, isTauri } from "./tauri";

/**
 * The tray's Quit, from a screen: stop the node, then exit. While an update
 * holds the node the stop is refused at once and ARC still exits; reopening
 * is what an update ARC couldn't finish needs. Outside the native shell
 * (browser previews) a reload is the closest equivalent.
 */
export async function quitArc(): Promise<void> {
  if (!isTauri) {
    window.location.reload();
    return;
  }
  await api.stopNode().catch(() => {});
  await exit(0).catch(() => {});
}
