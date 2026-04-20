import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.describe("Navigation", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  const routes = [
    { id: "dashboard", testid: "dashboard" },
    { id: "earnings", testid: "earnings-screen" },
    { id: "network", testid: "network-screen" },
    { id: "logs", testid: "logs-screen" },
    { id: "settings", testid: "settings-screen" },
  ] as const;

  for (const r of routes) {
    test(`navigates to ${r.id}`, async ({ page }) => {
      await page.goto("/");
      await page.getByTestId(`nav-${r.id}`).click();
      await expect(page.getByTestId(r.testid)).toBeVisible();
    });
  }

  test("active nav item has aria-current=page", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("nav-earnings")).toHaveAttribute(
      "aria-current",
      "page",
    );
    await expect(page.getByTestId("nav-dashboard")).not.toHaveAttribute(
      "aria-current",
      "page",
    );
  });
});
