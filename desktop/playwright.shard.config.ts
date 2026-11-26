import { defineConfig } from "@playwright/test";
import base from "./playwright.config";

export default defineConfig({
  ...base,
  testMatch: ["partial-worker-ui.spec.ts", "compute-contribution.spec.ts"],
  workers: 1,
  retries: 0,
  webServer: {
    command: "npm run preview",
    url: "http://localhost:4173",
    reuseExistingServer: false,
    timeout: 30_000,
  },
});
