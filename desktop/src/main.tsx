import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { api } from "./lib/tauri";
import { startNodeCare } from "./lib/node-care-runtime";
import { initTheme } from "./lib/theme";
import "./styles/fonts.css";
import "./styles/tokens.css";
import "./styles/reset.css";
import "./styles/app.css";
import "./styles/engraved.css";
import "./styles/chat.css";

function stampPlatform() {
  const el = document.documentElement;
  const ua = navigator.userAgent;
  if (/Mac/i.test(ua)) el.dataset.platform = "macos";
  else if (/Win/i.test(ua)) el.dataset.platform = "windows";
  else if (/Linux|X11/i.test(ua)) el.dataset.platform = "linux";

  api
    .detectHardware()
    .then((hw) => {
      const p = hw.platform.toLowerCase();
      if (p.includes("mac")) el.dataset.platform = "macos";
      else if (p.includes("win")) el.dataset.platform = "windows";
      else if (p.includes("linux")) el.dataset.platform = "linux";
    })
    .catch(() => {
    });
}

stampPlatform();
initTheme();

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      refetchOnWindowFocus: false,
      retry: 1,
      staleTime: 2_000,
    },
  },
});

startNodeCare(queryClient);

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>
    </ErrorBoundary>
  </React.StrictMode>,
);
