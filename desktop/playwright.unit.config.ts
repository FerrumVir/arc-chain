import { defineConfig } from "@playwright/test";

// Pure-logic specs: no browser, no build, no preview server. A spec listed
// here must not use `page`; anything that renders belongs in the default
// config, which builds the app and serves it first.
export default defineConfig({
  testDir: ".",
  // Unit-only contracts live in unit/; the audited browser inventory stays in tests/.
  testMatch: ["update-controller.spec.ts", "native-request.spec.ts", "model-download-progress.spec.ts", "serving-stats.spec.ts"],
  workers: 1,
  reporter: "list",
  timeout: 10_000,
});
