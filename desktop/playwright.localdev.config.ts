import { defineConfig } from "@playwright/test";

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
