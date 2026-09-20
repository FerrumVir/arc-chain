import { defineConfig } from "@playwright/test";

// LOCAL-DEVELOPMENT browser coverage against a disposable local network.
//
// This is deliberately a separate, separately named project. It is NOT the
// recovered-production live gate: `playwright.live.config.ts` and
// `explorer/test-live.mjs` keep their fleet-specific checkpoint, interlock and
// signing requirements untouched, and nothing here may be presented as
// production acceptance.
//
// What it adds is the thing mock-backed suites cannot: the real explorer HTML
// and app.js, rendered by a real browser, reading a real local node.
//
// It serves the repository root so both /explorer/ and /shared/ resolve the way
// they do in production, using python3's http.server so no extra dependency is
// introduced to run it.
export default defineConfig({
  testDir: "./tests-localdev",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: 1,
  reporter: [["list"], ["json", { outputFile: process.env.PLAYWRIGHT_JSON_OUTPUT_NAME || "localdev-report.json" }]],
  timeout: 60_000,
  use: {
    baseURL: "http://127.0.0.1:4180",
    trace: "retain-on-failure",
  },
  webServer: {
    command: "python3 -m http.server 4180 --bind 127.0.0.1 --directory ..",
    url: "http://127.0.0.1:4180/explorer/index.html",
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
