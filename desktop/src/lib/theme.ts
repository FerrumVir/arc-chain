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
  }
  return DEFAULT_THEME;
}

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
  }
  applyTheme(theme);
}

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

export function engravingGround(): string {
  if (typeof document === "undefined") return "#002dde";
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue("--engrave-ground")
    .trim();
  return value || "#002dde";
}
