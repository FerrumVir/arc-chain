import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { api } from "./lib/tauri";
import "./styles/tokens.css";
import "./styles/reset.css";
import "./styles/app.css";

// Stamp the platform on <html> so CSS can scope platform-specific chrome —
// chiefly the 80px titlebar inset that clears macOS traffic lights and is
// dead space anywhere else. Done before render so the first paint is
// already correct; failure is non-fatal and just leaves the neutral layout.
function stampPlatform() {
  const el = document.documentElement;
  // Fast path: the UA string is available synchronously, so the very first
  // frame is right even before detectHardware() resolves.
  const ua = navigator.userAgent;
  if (/Mac/i.test(ua)) el.dataset.platform = "macos";
  else if (/Win/i.test(ua)) el.dataset.platform = "windows";
  else if (/Linux|X11/i.test(ua)) el.dataset.platform = "linux";

  // Then confirm against what the OS actually reports. On Linux the webview
  // UA can read as generic, and Tauri's own value is authoritative.
  api
    .detectHardware()
    .then((hw) => {
      const p = hw.platform.toLowerCase();
      if (p.includes("mac")) el.dataset.platform = "macos";
      else if (p.includes("win")) el.dataset.platform = "windows";
      else if (p.includes("linux")) el.dataset.platform = "linux";
    })
    .catch(() => {
      /* keep the UA-derived guess */
    });
}

stampPlatform();

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      refetchOnWindowFocus: false,
      retry: 1,
      staleTime: 2_000,
    },
  },
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>
    </ErrorBoundary>
  </React.StrictMode>,
);
