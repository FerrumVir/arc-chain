import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig(async ({ mode }) => {
  // Production Tauri builds bake TAURI_ENV_PLATFORM; everything else is a
  // browser/dev build that gets the mock IPC. Flag used inside tauri.ts to
  // refuse mock calls when !isTauri in a production build (can't fabricate
  // node state from a production bundle served off a web host).
  const isProdTauriBundle =
    mode === "production" && !!process.env.TAURI_ENV_PLATFORM;

  return {
    plugins: [react()],
    clearScreen: false,
    define: {
      __ARC_PROD_TAURI__: JSON.stringify(isProdTauriBundle),
    },
    server: {
      port: 1420,
      strictPort: true,
      host: host || false,
      hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
      watch: {
        ignored: ["**/src-tauri/**"],
      },
    },
    envPrefix: ["VITE_", "TAURI_ENV_*"],
    build: {
      target:
        process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
      minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
      sourcemap: !!process.env.TAURI_ENV_DEBUG,
    },
  };
});
