import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { seedOnboarded } from "./helpers";

const tauriSource = readFileSync(new URL("../src/lib/tauri.ts", import.meta.url), "utf8");

test.describe("Accessibility basics", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("dashboard has a single h1 with 'Dashboard'", async ({ page }) => {
    await page.goto("/");
    const h1s = page.locator("h1");
    await expect(h1s).toHaveCount(1);
    await expect(h1s.first()).toHaveText(/dashboard/i);
  });

  test("nav buttons are reachable via keyboard (tab focus)", async ({
    page,
  }) => {
    await page.goto("/");
    // Focus the first nav item
    await page.getByTestId("nav-dashboard").focus();
    await expect(page.getByTestId("nav-dashboard")).toBeFocused();
    // Tab forward and confirm another button gets focus
    await page.keyboard.press("Tab");
    const focused = await page.evaluate(
      () => document.activeElement?.getAttribute("data-testid") ?? "",
    );
    expect(focused).toMatch(/nav-|btn-|copy/);
  });

  test("browser fixture is unmistakably labeled as synthetic and not live", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("preview-mode")).toBeVisible();
    await expect(page.getByTestId("preview-mode")).toContainText(
      "Synthetic preview · not live",
    );
    const banner = page.getByTestId("synthetic-preview-banner");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("SYNTHETIC UI PREVIEW — NOT LIVE ARC DATA");
    await expect(banner).toContainText(
      "Balances, blocks, inference activity, receipts, earnings, and projections",
    );

    const layoutBoxes = async () => {
      const [bannerBox, mainBox, sidebarBox] = await Promise.all([
        banner.boundingBox(),
        page.getByTestId("main").boundingBox(),
        page.getByTestId("sidebar").boundingBox(),
      ]);
      expect(bannerBox).not.toBeNull();
      expect(mainBox).not.toBeNull();
      expect(sidebarBox).not.toBeNull();
      return {
        banner: bannerBox!,
        main: mainBox!,
        sidebar: sidebarBox!,
      };
    };

    const desktop = await layoutBoxes();
    expect(desktop.banner.y).toBeGreaterThanOrEqual(
      desktop.main.y + desktop.main.height - 1,
    );
    expect(desktop.banner.y).toBeGreaterThanOrEqual(
      desktop.sidebar.y + desktop.sidebar.height - 1,
    );

    await page.setViewportSize({ width: 720, height: 900 });
    const mobile = await layoutBoxes();
    expect(mobile.banner.y).toBeGreaterThanOrEqual(
      mobile.main.y + mobile.main.height - 1,
    );
    expect(mobile.banner.y + mobile.banner.height).toBeLessThanOrEqual(
      mobile.sidebar.y + 1,
    );
  });

  test("production bundles block before either browser-live or fixture dispatch", () => {
    const invokeBody = tauriSource.slice(
      tauriSource.indexOf("export async function invoke"),
      tauriSource.indexOf("// Typed wrappers"),
    );
    expect(invokeBody.indexOf("if (IS_PROD_TAURI_BUNDLE)")).toBeGreaterThan(-1);
    expect(invokeBody.indexOf("if (IS_PROD_TAURI_BUNDLE)")).toBeLessThan(
      invokeBody.indexOf("if (liveBase())"),
    );
    expect(invokeBody.indexOf("if (IS_PROD_TAURI_BUNDLE)")).toBeLessThan(
      invokeBody.indexOf("return mockInvoke"),
    );
    expect(tauriSource).toContain("if (IS_PROD_TAURI_BUNDLE) return null;");
  });
});
