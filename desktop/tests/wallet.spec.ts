import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { seedOnboarded } from "./helpers";

const here = path.dirname(fileURLToPath(import.meta.url));
const desktop = path.resolve(here, "..");

test.describe("exact, receipt-backed wallet", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-wallet").click();
    await expect(page.getByTestId("wallet-screen")).toBeVisible();
  });

  test("renders base units as exact 9-decimal ARC", async ({ page }) => {
    await expect(page.getByTestId("wallet-balance")).toContainText("28,500");
    await expect(page.getByTestId("wallet-balance")).not.toContainText(
      "28,500,000,000,000",
    );
  });

  test("faucet submission remains pending until a mined receipt exists", async ({
    page,
  }) => {
    await page.getByTestId("btn-faucet").click();
    const status = page.getByTestId("faucet-success");
    await expect(status).toContainText("waiting for a mined receipt");
    await expect(status).not.toContainText("Confirmed");
    await expect(status).not.toContainText("+1");
  });

  test("submits only recipient and decimal amount through the wallet UI", async ({
    page,
  }) => {
    await page
      .getByTestId("send-recipient")
      .fill(`0x${"11".repeat(32)}`);
    await page.getByTestId("send-amount").fill("1.25");
    await page.getByTestId("btn-send-arc").click();
    await expect(page.getByTestId("send-status")).toContainText(
      "Submitted 1.25 ARC",
    );
    await expect(page.getByTestId("send-status")).toContainText(
      "waiting for a mined receipt",
    );
  });
});

test("send IPC wrapper has no seed or private-key argument", () => {
  const source = fs.readFileSync(
    path.join(desktop, "src", "lib", "tauri.ts"),
    "utf8",
  );
  expect(source).toMatch(
    /sendArc:\s*\(to: string, amountArc: string\)\s*=>\s*\n?\s*invoke<WalletTxResult>\("send_arc", \{ to, amountArc \}\)/,
  );
  expect(source).not.toMatch(
    /sendArc:\s*\([^)]*(seed|phrase|privateKey|secret)/i,
  );
});
