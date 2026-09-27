import { expect, test, type Page } from "@playwright/test";
import { seedOnboarded } from "./helpers";

// The native paid-request journey on the browser model of a protocol-4 chain
// (src/lib/native-mock.ts). It exercises the screen and the lifecycle wiring:
// signing, admission, certification, settlement, expiry, refund, withdrawal
// and a restart. The Rust signing path is covered by native_paid.rs tests and
// the post-soak live acceptance run, not by this mock.

async function openNative(page: Page, flag: true | "incompatible" = true) {
  await seedOnboarded(page);
  await page.addInitScript((value) => {
    (window as Window & { __ARC_MOCK_NATIVE__?: unknown }).__ARC_MOCK_NATIVE__ = value;
  }, flag);
  await page.goto("/");
  await page.getByTestId("nav-inference").click();
  await expect(page.getByTestId("native-paid-card")).toBeVisible();
}

async function closeNativeAdmission(page: Page) {
  await page.evaluate(() => {
    (window as Window & { __ARC_MOCK_NATIVE__?: unknown }).__ARC_MOCK_NATIVE__ = "closed-admission";
  });
}

async function submitPrompt(page: Page, prompt: string, options: { price?: string; reserve?: string } = {}) {
  await page.getByTestId("native-prompt").fill(prompt);
  if (options.price) await page.getByTestId("native-price").fill(options.price);
  if (options.reserve) await page.getByTestId("native-reserve").fill(options.reserve);
  await page.getByTestId("btn-native-review").click();
  await expect(page.getByTestId("native-review")).toContainText("A signed request cannot be recalled");
  await page.getByTestId("btn-native-sign").click();
}

test.describe("native paid requests", () => {
  test("a host that is not a protocol-4 chain shows no paid-request panel", async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await expect(page.getByTestId("inference-screen")).toBeVisible();
    await expect(page.getByTestId("native-paid-card")).toHaveCount(0);
  });

  test("an incompatible node is named with what to update, and nothing can be signed", async ({ page }) => {
    await openNative(page, "incompatible");
    await expect(page.getByTestId("native-context-status")).toContainText("update the app");
    await expect(page.getByTestId("btn-native-review")).toBeDisabled();
    await expect(page.getByTestId("btn-native-sign")).toHaveCount(0);
  });

  test("a request is signed, admitted, certified and settled, and the balance follows the chain", async ({ page }) => {
    await openNative(page);
    await expect(page.getByTestId("native-chain-status")).toContainText("deterministic TEST executor");
    await expect(page.getByTestId("native-available")).toHaveText("5 ARC");

    await submitPrompt(page, "What is ARC?", { price: "0.25", reserve: "1" });
    await expect(page.getByTestId("native-review")).toHaveCount(0);
    const row = page.getByTestId("native-request-row").first();
    await expect(row).toHaveAttribute("data-phase", /submitted|awaiting_certificate/);
    // Admission moves the reservation into escrow.
    await expect(row).toHaveAttribute("data-phase", "awaiting_certificate", { timeout: 15_000 });
    await expect(page.getByTestId("native-reserved")).toContainText("Reserved in escrow: 1 ARC");
    // Certification settles: the price is paid, the rest returns.
    await expect(row).toHaveAttribute("data-phase", "finalized", { timeout: 15_000 });
    await expect(row.getByTestId("native-output")).toContainText("test executor output, not an answer");
    await expect(row.getByTestId("native-output")).toContainText("reconciles with the reservation");
    await expect(page.getByTestId("native-reserved")).toContainText("Reserved in escrow: 0 ARC");
    await expect(page.getByTestId("native-available")).toHaveText("4.75 ARC", { timeout: 10_000 });
  });

  test("an admitted request that expires uncertified is refunded only by a claim", async ({ page }) => {
    await openNative(page);
    await submitPrompt(page, "[expire] never certified", { price: "0.5", reserve: "2" });
    const row = page.getByTestId("native-request-row").first();
    await expect(row).toHaveAttribute("data-phase", "refund_due", { timeout: 20_000 });
    await expect(row.getByTestId("native-request-phase")).toContainText("claim the reservation back");
    // The reservation is still held until the refund lands.
    await expect(page.getByTestId("native-reserved")).toContainText("Reserved in escrow: 2 ARC");
    await row.getByTestId("btn-native-refund").click();
    await expect(row).toHaveAttribute("data-phase", /refund_submitted|refunded/);
    await expect(row).toHaveAttribute("data-phase", "refunded", { timeout: 15_000 });
    await expect(row).toContainText("Returned 2 ARC");
    await expect(page.getByTestId("native-available")).toHaveText("5 ARC", { timeout: 10_000 });
  });

  test("closing new-request admission keeps an in-flight request tracked and refundable", async ({ page }) => {
    test.setTimeout(60_000);
    await openNative(page);
    await submitPrompt(page, "[expire] close admission while this request is pending");
    const row = page.getByTestId("native-request-row").first();
    await expect(row).toHaveAttribute("data-phase", /submitted|awaiting_certificate/);

    await closeNativeAdmission(page);
    await expect(page.getByTestId("native-context-status")).toContainText("not admitting new native paid requests", { timeout: 25_000 });
    await expect(page.getByTestId("btn-native-review")).toBeDisabled();
    await expect(page.getByTestId("btn-native-sign")).toHaveCount(0);
    await expect(row).toHaveAttribute("data-phase", "refund_due", { timeout: 20_000 });
    await expect(row.getByTestId("btn-native-refund")).toBeEnabled();
    await row.getByTestId("btn-native-refund").click();
    await expect(row).toHaveAttribute("data-phase", /refund_submitted|refunded/);
    await expect(row).toHaveAttribute("data-phase", "refunded", { timeout: 15_000 });
  });

  test("a request no block admits before its expiry is dropped and costs nothing", async ({ page }) => {
    await openNative(page);
    await submitPrompt(page, "[drop] never admitted");
    const row = page.getByTestId("native-request-row").first();
    await expect(row).toHaveAttribute("data-phase", "dropped", { timeout: 20_000 });
    await expect(row).toContainText("nothing charged");
    await expect(page.getByTestId("native-available")).toHaveText("5 ARC");
  });

  test("only an unsigned request can be withdrawn; a signed one has no hide button", async ({ page }) => {
    await openNative(page);
    // The first request holds the account's nonce until a block admits it,
    // so the second waits unsigned and can be withdrawn.
    await submitPrompt(page, "[hold] holds the nonce");
    await submitPrompt(page, "second, still unsigned");
    const rows = page.getByTestId("native-request-row");
    const waiting = rows.filter({ hasText: "second, still unsigned" });
    await expect(waiting).toHaveAttribute("data-phase", "waiting");
    await waiting.getByTestId("btn-native-withdraw").click();
    await expect(waiting).toHaveAttribute("data-phase", "withdrawn");
    await expect(waiting).toContainText("nothing was signed or sent");
    const signed = rows.filter({ hasText: "holds the nonce" });
    await expect(signed.getByTestId("btn-native-withdraw")).toHaveCount(0);
  });

  test("on a protocol-4 chain the wallet explains that ARC cannot be transferred", async ({ page }) => {
    await openNative(page);
    // Its blocks carry only native paid-inference transactions (D13): the
    // node refuses a transfer or faucet claim, so the wallet offers neither.
    await page.getByTestId("nav-wallet").click();
    await expect(page.getByTestId("send-unavailable")).toContainText("cannot be transferred here");
    await page.getByTestId("send-recipient").fill("11".repeat(32));
    await page.getByTestId("send-amount").fill("0.5");
    await expect(page.getByTestId("btn-send-arc")).toBeDisabled();
    await expect(page.getByTestId("btn-faucet")).toBeDisabled();
    await expect(page.getByTestId("faucet-unavailable")).toBeVisible();
    await expect(page.getByTestId("wallet-screen")).toContainText(
      "Transfers cannot reach this address on this chain.",
    );
  });

  test("an unfunded request is refused before anything is signed", async ({ page }) => {
    await openNative(page);
    await submitPrompt(page, "too expensive", { price: "1", reserve: "50" });
    const row = page.getByTestId("native-request-row").first();
    await expect(row).toHaveAttribute("data-phase", "rejected", { timeout: 10_000 });
    await expect(row).toContainText("not signed: insufficient balance");
  });

  test("after a restart, signed requests are followed again from the journal", async ({ page }) => {
    await openNative(page);
    await submitPrompt(page, "survives a restart", { price: "0.1", reserve: "0.1" });
    const row = page.getByTestId("native-request-row").first();
    await expect(row).toHaveAttribute("data-phase", /submitted|awaiting_certificate/, { timeout: 10_000 });
    await page.reload();
    await page.getByTestId("nav-inference").click();
    const restored = page.getByTestId("native-request-row").filter({ hasText: "survives a restart" });
    await expect(restored).toBeVisible();
    await expect(restored).toHaveAttribute("data-phase", "finalized", { timeout: 20_000 });
  });
});
