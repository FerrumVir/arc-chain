import { expect, test } from "@playwright/test";
import { seedOnboarded, clearState } from "./helpers";

test.describe("Visual polish smoke tests", () => {
  test("onboarding welcome screen looks right", async ({ page }) => {
    await clearState(page);
    await page.goto("/");
    // Hero title and three value props (one-click-join copy)
    await expect(page.getByText("One click")).toBeVisible();
    await expect(page.getByText("Your identity, on-chain")).toBeVisible();
    await expect(page.getByText("Ready when you are")).toBeVisible();

    // Logo gradient uses our brand gradient
    const logo = page.getByTestId("logo-mark").first();
    await expect(logo).toBeVisible();
    const bg = await logo.evaluate((el) => getComputedStyle(el).backgroundImage);
    expect(bg).toContain("linear-gradient");
    // Tagline is present
    await expect(page.getByTestId("tagline")).toBeVisible();
    await expect(page.getByTestId("tagline")).toContainText(/ai for humans first/i);
  });

  test("dashboard earnings uses gradient text", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    const earnings = page.getByTestId("earnings-total");
    await expect(earnings).toBeVisible();
    const bgClip = await earnings.evaluate((el) => {
      // Either the element OR one of its descendants has background-clip:text
      const all = [el, ...el.querySelectorAll("*")] as HTMLElement[];
      return all.some((n) => getComputedStyle(n).webkitBackgroundClip === "text");
    });
    expect(bgClip).toBe(true);
  });

  test("pulse-dot animates when node is live", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    // Start the node so at least one pulse-dot is in the `live` state,
    // where the ::after ring animation is enabled.
    await page.getByTestId("btn-start").click();
    await expect(page.getByTestId("btn-stop")).toBeVisible();
    // Wait for status to become live (mock flips after ~8s of uptime).
    // Or just pick any non-offline pulse-dot.
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
    // Check the ::before indicator is present via computed style
    const hasIndicator = await active.evaluate((el) => {
      const before = getComputedStyle(el, "::before");
      return before.content !== "none" && before.background !== "";
    });
    expect(hasIndicator).toBe(true);
  });
});
