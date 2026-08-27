// Current zero-peer recovery UI.
//
// Live mode (window.__ARC_LIVE__ = 9090) makes tauri.ts hit fetch
// instead of its in-process mock. The configured coordinators now use dashed
// HTTPS nip.io origins, so these routes mirror the origins a shipped v0.8
// desktop actually probes.

import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.describe("Dashboard zero-peer recovery", () => {
  test("renders honest host-scoped client mode for a reachable coordinator", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    await page.route("**/health", (route) => {
      const host = new URL(route.request().url()).hostname;
      if (host === "127.0.0.1") {
        return route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({
            status: "ok",
            version: "0.8.0",
            height: 0,
            peers: 0,
            uptime_secs: 60,
            dag_round: 0,
            dag_committed: 0,
            validators: 0,
          }),
        });
      }
      if (host === "149-28-32-76.nip.io") {
        return route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({ status: "ok" }),
        });
      }
      return route.fulfill({ status: 503, body: "unavailable" });
    });

    await page.goto("/");

    const banner = page.getByTestId("lite-mode-banner");
    await expect(banner).toBeVisible({ timeout: 15_000 });

    // Client mode names the exact configured host and does not turn a peer
    // threshold into an earnings promise.
    await expect(banner).toContainText("Client mode");
    await expect(banner).toContainText("scoped to that host");
    await expect(banner).toContainText("composite explorer");
    await expect(banner).not.toContainText("public seeds remain divergent");
    await expect(banner).toContainText("cannot receive peer-routed community work");
    await expect(banner).toContainText("0 peers");
    await expect(banner).toContainText("NYC");

    // Reset button is present and enabled.
    const resetBtn = page.getByTestId("reset-peer-state-btn");
    await expect(resetBtn).toBeVisible();
    await expect(resetBtn).toBeEnabled();
    await expect(resetBtn).toContainText(/Reset peer state/i);
  });

  test("zero peers without a coordinator still exposes working recovery", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });
    await page.route("**/health", (route) => {
      const host = new URL(route.request().url()).hostname;
      if (host === "127.0.0.1") {
        return route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({
            status: "ok",
            version: "0.8.0",
            height: 0,
            peers: 0,
            uptime_secs: 60,
            dag_round: 0,
            dag_committed: 0,
            validators: 0,
          }),
        });
      }
      return route.fulfill({ status: 503, body: "unavailable" });
    });
    await page.goto("/");

    const banner = page.getByTestId("no-peers-banner");
    await expect(banner).toBeVisible({ timeout: 15_000 });
    await expect(banner).toContainText("No peers yet");
    await expect(banner).toContainText(
      "cannot receive peer-routed community work",
    );

    const btn = page.getByTestId("reset-peer-state-btn");
    await expect(btn).toBeEnabled();
    await btn.click();
    await expect(page.getByTestId("reset-peer-state-result")).toContainText(
      "Rebootstrapping from testnet seeds",
    );
  });
});
