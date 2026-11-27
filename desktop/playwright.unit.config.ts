import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  testMatch: [
    "update-controller.spec.ts",
    "native-request.spec.ts",
    "model-download-progress.spec.ts",
    "start-failure.spec.ts",
    "connection.spec.ts",
    "wallet-outcome.spec.ts",
    "node-care.spec.ts",
    "auto-recovery.spec.ts",
    "worker-status.spec.ts",
  ],
  workers: 1,
  reporter: "list",
  timeout: 10_000,
});
