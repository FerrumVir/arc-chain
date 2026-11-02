import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { seedMockOverrides, seedOnboarded } from "./helpers";

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

// A failed claim or transfer: "didn't go through" only when nothing of ours
// can have been included; a lost answer after the POST says to check the
// balance first, because the host may have mined it (lib/wallet-outcome.ts).
test.describe("wallet failures", () => {
  test("a transfer whose answer was lost after the POST says to check the balance, not that it failed", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await seedMockOverrides(page, {
      send_arc: {
        __reject:
          "transfer submission failed: error sending request for url (https://140.82.16.112/tx/submit_signed): operation timed out",
      },
    });
    await page.goto("/");
    await page.getByTestId("nav-wallet").click();
    await page.getByTestId("send-recipient").fill(`0x${"11".repeat(32)}`);
    await page.getByTestId("send-amount").fill("1.25");
    await page.getByTestId("btn-send-arc").click();

    const failure = page.getByTestId("send-error");
    await expect(failure.getByTestId("send-error-sentence")).toHaveText(PLAIN_COPY.wallet.sendUnconfirmed);
    await expect(failure.getByTestId("send-error-sentence")).not.toContainText("didn't go through");
    await expect(failure).toHaveAttribute("data-failure-kind", "unconfirmed");
    // The native text waits behind Show details, never as the headline.
    const details = failure.getByTestId("send-error-details");
    await expect(details.locator("pre")).toBeHidden();
    await details.locator("summary").click();
    await expect(details.locator("pre")).toContainText("transfer submission failed");
  });

  test("a refused transfer and a refused faucet claim say they didn't go through", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await seedMockOverrides(page, {
      send_arc: {
        __reject:
          "insufficient balance: available 0 ARC, requested 1.25 ARC plus 0.000000001 ARC network fee",
      },
      faucet_claim: { __reject: "faucet exhausted (429)" },
    });
    await page.goto("/");
    await page.getByTestId("nav-wallet").click();

    await page.getByTestId("btn-faucet").click();
    const faucet = page.getByTestId("faucet-error");
    await expect(faucet.getByTestId("faucet-error-sentence")).toHaveText(PLAIN_COPY.wallet.faucetFailed);
    await expect(faucet).toHaveAttribute("data-failure-kind", "refused");
    // The host's text is behind Show details, not in the sentence.
    await expect(faucet.getByTestId("faucet-error-sentence")).not.toContainText("faucet exhausted");

    await page.getByTestId("send-recipient").fill(`0x${"11".repeat(32)}`);
    await page.getByTestId("send-amount").fill("1.25");
    await page.getByTestId("btn-send-arc").click();
    const send = page.getByTestId("send-error");
    await expect(send.getByTestId("send-error-sentence")).toHaveText(PLAIN_COPY.wallet.sendFailed);
    await expect(send).toHaveAttribute("data-failure-kind", "refused");
    await send.getByTestId("send-error-details").locator("summary").click();
    await expect(send.getByTestId("send-error-details").locator("pre")).toContainText(
      "insufficient balance",
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
