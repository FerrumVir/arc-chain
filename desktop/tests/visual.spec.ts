import { expect, test } from "@playwright/test";
import { seedOnboarded, clearState } from "./helpers";

test.describe("Visual polish smoke tests", () => {
  test("onboarding welcome screen looks right", async ({ page }) => {
    await clearState(page);
    await page.goto("/");
    await expect(page.getByText("Ask for free")).toBeVisible();
    await expect(page.getByText("See who answered")).toBeVisible();
    await expect(page.getByText("Share if you want")).toBeVisible();

    const logo = page.getByTestId("logo-mark").first();
    await expect(logo).toBeVisible();
    const bg = await logo.evaluate((el) => getComputedStyle(el).backgroundImage);
    expect(bg).toContain("linear-gradient");
    await expect(page.getByTestId("tagline")).toBeVisible();
    await expect(page.getByTestId("tagline")).toContainText(/own your ai/i);
  });

  test("dashboard earnings uses gradient text", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    const earnings = page.getByTestId("earnings-total");
    await expect(earnings).toBeVisible();
    const bgClip = await earnings.evaluate((el) => {
      const all = [el, ...el.querySelectorAll("*")] as HTMLElement[];
      return all.some((n) => getComputedStyle(n).webkitBackgroundClip === "text");
    });
    expect(bgClip).toBe(true);
  });

  test("pulse-dot animates when node is live", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    await expect(page.getByTestId("btn-stop")).toBeVisible();
    const liveDot = page.locator(".pulse-dot.live, .pulse-dot.syncing").first();
    await expect(liveDot).toBeVisible();
    const duration = await liveDot.evaluate(
      (el) => getComputedStyle(el, "::after").animationDuration,
    );
    expect(duration).not.toBe("0s");
    expect(duration).not.toBe("");
  });

  test("sidebar active indicator glows", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    const active = page.locator(".nav-item.active").first();
    await expect(active).toBeVisible();
    const hasIndicator = await active.evaluate((el) => {
      const before = getComputedStyle(el, "::before");
      return before.content !== "none" && before.background !== "";
    });
    expect(hasIndicator).toBe(true);
  });
});
