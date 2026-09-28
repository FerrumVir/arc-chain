// The app's look, switched in one place.
//
// "engraved" is the website's language: white line on Arc blue, with Marcellus,
// Hanken Grotesk and JetBrains Mono. "classic" is the original dark theme.
// One attribute on <html> (`data-theme`) swaps every design token (see
// styles/engraved.css); nothing else in the app branches on the theme except
// the engraved illustrations, which read their ground colour from a token.
//
// To revert the product default, change DEFAULT_THEME. People can also switch
// in Settings -> Appearance; that choice is remembered on this device only.

import { useSyncExternalStore } from "react";

export type Theme = "engraved" | "classic";

export const DEFAULT_THEME: Theme = "engraved";

const STORAGE_KEY = "arc-desktop-theme-v1";
const listeners = new Set<() => void>();
let current: Theme = DEFAULT_THEME;

function isTheme(value: unknown): value is Theme {
  return value === "engraved" || value === "classic";
}

export function readStoredTheme(): Theme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (isTheme(stored)) return stored;
  } catch {
    // Storage can be unavailable (private mode, locked-down webview): use the default.
  }
  return DEFAULT_THEME;
}

// Only the classic theme sets Inter. It used to be a render-blocking remote stylesheet in index.html that every
// launch waited on, including the engraved default and offline starts; now it is fetched only when classic is used.
const INTER_HREF = "https://rsms.me/inter/inter.css";

function ensureClassicFont(): void {
  if (typeof document === "undefined") return;
  if (document.querySelector(`link[href="${INTER_HREF}"]`)) return;
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = INTER_HREF;
  document.head.appendChild(link);
}

export function applyTheme(theme: Theme): void {
  current = theme;
  if (typeof document !== "undefined") {
    document.documentElement.dataset.theme = theme;
    if (theme === "classic") ensureClassicFont();
  }
  listeners.forEach((listener) => listener());
}

export function setTheme(theme: Theme): void {
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // Not persisted, but still applied for this session.
  }
  applyTheme(theme);
}

/** Call once before the first render so the first paint is already themed. */
export function initTheme(): Theme {
  const theme = readStoredTheme();
  applyTheme(theme);
  return theme;
}

export function useTheme(): Theme {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => current,
    () => current,
  );
}

/** The ground an engraving is printed on: the colour its masks paint. */
export function engravingGround(): string {
  if (typeof document === "undefined") return "#002dde";
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue("--engrave-ground")
    .trim();
  return value || "#002dde";
}
