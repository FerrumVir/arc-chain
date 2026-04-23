import { test } from "@playwright/test";
import { clearState, seedOnboarded } from "./helpers";

// Captures a gallery of the finished UI. Failing this suite does NOT fail CI —
// it's meant for design review. Run with: npx playwright test screenshots.spec.ts
test.describe("Screenshot gallery", () => {
  test("onboarding — welcome", async ({ page }) => {
    await clearState(page);
    await page.goto("/");
    await page.waitForSelector('[data-testid="step-welcome"]');
    await page.screenshot({ path: "screenshots/01-onboarding-welcome.png", fullPage: false });
  });

  test("onboarding — identity revealed", async ({ page }) => {
    await clearState(page);
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await page.waitForSelector('[data-testid="btn-reveal-seed"]');
    await page.getByTestId("btn-reveal-seed").click();
    await page.screenshot({ path: "screenshots/02-onboarding-identity.png" });
  });

  test("onboarding — launch ready", async ({ page }) => {
    await clearState(page);
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await page.waitForSelector('[data-testid="step-launch"]');
    await page.screenshot({ path: "screenshots/03-onboarding-launch.png" });
  });

  test("dashboard — stopped", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.waitForSelector('[data-testid="dashboard"]');
    await page.waitForTimeout(400);
    await page.screenshot({ path: "screenshots/05-dashboard-stopped.png" });
  });

  test("dashboard — running", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    await page.waitForSelector('[data-testid="btn-stop"]');
    await page.waitForTimeout(900);
    await page.screenshot({ path: "screenshots/06-dashboard-running.png" });
  });

  test("earnings screen", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await page.waitForTimeout(700);
    await page.screenshot({ path: "screenshots/07-earnings.png", fullPage: true });
  });

  test("network screen", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-network").click();
    await page.waitForTimeout(400);
    await page.screenshot({ path: "screenshots/08-network.png" });
  });

  test("logs screen", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    await page.getByTestId("nav-logs").click();
    await page.waitForTimeout(700);
    await page.screenshot({ path: "screenshots/09-logs.png" });
  });

  test("settings screen", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await page.waitForTimeout(400);
    await page.screenshot({ path: "screenshots/10-settings.png", fullPage: true });
  });

  test("wallet screen", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-wallet").click();
    await page.waitForTimeout(500);
    await page.screenshot({ path: "screenshots/11-wallet.png", fullPage: true });
  });

  test("inference tester (idle)", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.waitForTimeout(400);
    await page.screenshot({ path: "screenshots/12-inference-idle.png", fullPage: true });
  });

  test("inference tester (with result)", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.getByTestId("inference-prompt").fill("Biggest planet?");
    await page.getByTestId("btn-run-inference").click();
    await page.waitForSelector('[data-testid="inference-result"]', { timeout: 6000 });
    await page.waitForTimeout(400);
    await page.screenshot({ path: "screenshots/13-inference-result.png", fullPage: true });
  });
});
