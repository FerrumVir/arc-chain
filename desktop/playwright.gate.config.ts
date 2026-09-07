import { defineConfig } from "@playwright/test";
import baseConfig from "./playwright.config";

// The required gate is deterministic and side-effect free. The screenshot
// gallery intentionally rewrites tracked design assets. live.spec.ts is still
// discovered by the required gate, but skips unless ARC_LIVE_PORT is explicit;
// playwright.live.config.ts turns that same suite into a fail-closed live gate.
export default defineConfig({
  ...baseConfig,
  testIgnore: ["**/screenshots.spec.ts"],
});
