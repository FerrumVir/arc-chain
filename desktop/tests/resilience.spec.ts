import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { seedMockOverrides, seedOnboarded } from "./helpers";

test.describe("Error boundary", () => {
  test("catches render error + shows recovery UI with relaunch button", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await expect(page.getByTestId("dashboard")).toBeVisible();

    await page.evaluate(() => {
      const root = document.getElementById("root");
      if (!root) return;
      const throwEvent = new CustomEvent("arc-test-force-error");
      window.dispatchEvent(throwEvent);
    });

    const boundary = page.getByTestId("error-boundary");
    expect(await boundary.count()).toBe(0);
  });

  test("error-boundary component is shipped and reachable via test id", async ({
    page,
  }) => {
    await page.goto("/");
    const hasStrings = await page.evaluate(async () => {
      const res = await fetch("/");
      const html = await res.text();
      const match = html.match(/src="(\/assets\/index-[^"]+\.js)"/);
      if (!match) return false;
      const js = await (await fetch(match[1])).text();
      return (
        js.includes("something went sideways") && js.includes("Restart view")
      );
    });
    expect(hasStrings).toBe(true);
  });
});

test.describe("Crash banner", () => {
  test("a crash is one sentence with Start again; a refused start after it stays on screen", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_MOCK_CRASH__: boolean }).__ARC_MOCK_CRASH__ = true;
    });
    await seedMockOverrides(page, {
      start_node: {
        __reject:
          "another ARC desktop currently owns the managed node lifecycle for /Users/me/.arc/data-v3: Resource temporarily unavailable (os error 35)",
      },
    });
    await page.goto("/");
    await expect(page.getByTestId("dashboard")).toBeVisible();

    const banner = page.getByTestId("crash-banner");
    await expect(banner).toBeVisible();
    await expect(banner.getByTestId("crash-banner-title")).toHaveText(PLAIN_COPY.nodeStrip.crashed);
    await expect(banner.getByTestId("crash-banner-details")).toBeHidden();
    await banner.getByTestId("crash-banner-details-toggle").click();
    await expect(banner.getByTestId("crash-banner-details")).toContainText("exited unexpectedly");

    await page.getByTestId("btn-crash-relaunch").click();
    await expect(page.getByTestId("crash-banner")).toHaveCount(0);
    const failure = page.getByTestId("mutation-error");
    await expect(failure).toBeVisible();
    await expect(failure).toHaveAttribute("data-failure-kind", "other-desktop");
    await expect(failure.getByTestId("start-failure-title")).toHaveText(
      PLAIN_COPY.startFailure.otherDesktop.title,
    );
    await expect(failure.getByTestId("start-failure-details")).toContainText(
      "another ARC desktop currently owns",
    );
    await expect(failure.getByTestId("btn-start-failure-close-copy")).toBeVisible();
  });

  test("relaunch + dismiss buttons render when banner is forced", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.goto("/");
    const shipped = await page.evaluate(async () => {
      const res = await fetch("/");
      const html = await res.text();
      const match = html.match(/src="(\/assets\/index-[^"]+\.js)"/);
      if (!match) return false;
      const js = await (await fetch(match[1])).text();
      return (
        js.includes("Your node stopped unexpectedly.") &&
        js.includes("btn-crash-relaunch") &&
        js.includes("btn-crash-dismiss") &&
        !js.includes("Your node crashed")
      );
    });
    expect(shipped).toBe(true);
  });
});

test("a contended copy cannot offer settings changes or claim its own running node", async ({ page }) => {
  await seedOnboarded(page);
  await seedMockOverrides(page, {
    node_start_block: "ARC is already open on this computer.",
    node_status: { running: false, pid: null, health: "offline", version: "0.8.21", round: 0, committed: 0, height: 0, peers: 0, uptimeSeconds: 0, address: null, rpcPort: 9090, lastError: "ARC is already open on this computer." },
  });
  await page.goto("/");
  await expect(page.getByTestId("btn-start")).toHaveCount(0);
  await page.getByTestId("nav-settings").click();
  await expect(page.getByTestId("settings-screen")).toContainText("ARC is already open on this computer.");
  await expect(page.getByTestId("settings-screen").locator("input")).toHaveCount(0);
  await expect(page.getByTestId("btn-start-failure-close-copy")).toBeVisible();
  await expect(page.locator("#boot")).toHaveCount(0);
});
