import { defineConfig } from "@playwright/test";
import baseConfig from "./playwright.config";

// The required gate is deterministic and side-effect free. The screenshot
// gallery intentionally rewrites tracked design assets, while live.spec.ts
// depends on an ambient node at 127.0.0.1:9090; both remain directly runnable
// with the base config for manual review.
export default defineConfig({
  ...baseConfig,
  testIgnore: ["**/screenshots.spec.ts", "**/live.spec.ts"],
});
