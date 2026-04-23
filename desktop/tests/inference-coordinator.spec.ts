// Milestone A (#35): observer / no-model nodes fall back to a seed
// coordinator's /inference/run_consensus when local returns 503.
//
// Exercises the live-mode coordinator-fallback code path end-to-end:
//   1. `window.__ARC_LIVE__` makes tauri.ts hit `fetch` instead of its
//      in-process mock.
//   2. `page.route` intercepts fetch and responds:
//        - /inference/run → 503 SERVICE_UNAVAILABLE (observer node)
//        - /inference/run_consensus → a realistic consensus payload
//   3. The UI exercises runInferenceSmart → catches 503 → calls
//      runInferenceViaCoordinator → iterates COORDINATOR_HOSTS → first
//      seed responds → consensus banner renders.

import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

const COORD_PAYLOAD = {
  success: true,
  request_id: "0xtestrequest",
  input: "[INST] Biggest planet? [/INST]",
  output: " Jupiter.",
  output_tokens: [1, 2, 3],
  output_hash:
    "0xc9f2228c0c2c4e163f49c0c476d53c76c8d636cef2445bb4a085e9bdf9a00d57",
  tokens_generated: 3,
  total_ms: 28_400,
  pipeline_length: 6,
  k: 3,
  consensus: {
    k: 3,
    votes_total: 48,
    unanimous: 48,
    majority: 0,
    split: 0,
    divergent_replicas: {},
    auto_challenges: [],
  },
};

test.describe("Inference — coordinator fallback (Milestone A, #35)", () => {
  test("observer node (local 503) falls back to coordinator and renders consensus panel", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    // Local node rejects inference (observer role, no model loaded).
    await page.route("**/inference/run", (route) =>
      route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({
          error: "Coordinator needs a tokenizer loaded.",
        }),
      }),
    );

    // First seed the app tries (NYC 149.28.32.76) responds with a real
    // consensus payload. This mirrors what we saw live: 96/96 unanimous,
    // 0 divergent.
    let coordHitCount = 0;
    await page.route("**/inference/run_consensus", (route) => {
      coordHitCount++;
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(COORD_PAYLOAD),
      });
    });

    // Silence the /health poll so it doesn't clutter test output.
    await page.route("**/health", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          peers: 6,
          uptime_secs: 3600,
          version: "test",
          dag_round: 1,
          dag_committed: 1,
          height: 1,
          validators: 6,
        }),
      }),
    );

    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.getByTestId("inference-prompt").fill("Biggest planet?");
    await page.getByTestId("btn-run-inference").click();

    // Result card renders (coordinator answered).
    await expect(page.getByTestId("inference-result")).toBeVisible({
      timeout: 15_000,
    });

    // Consensus banner is present and names the NYC coordinator.
    const banner = page.getByTestId("inference-consensus");
    await expect(banner).toBeVisible();
    await expect(page.getByTestId("inference-coordinator")).toHaveText("NYC");
    await expect(banner).toContainText("48/48");
    await expect(banner).toContainText("unanimous");
    await expect(banner).toContainText("k=3");

    // Output text is the coordinator's real answer, not the mock stub.
    await expect(page.getByTestId("inference-output")).toContainText(
      "Jupiter",
    );

    // Only one coordinator was hit (NYC succeeded first — no retry).
    expect(coordHitCount).toBe(1);
  });

  test("local node healthy (returns real output) — no coordinator fallback", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    // Local node serves inference successfully (validator with --model).
    await page.route("**/inference/run", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          inference: {
            input: "[INST] Hi [/INST]",
            output: "Hello there.",
            output_hash: "0xaaaa",
            model_hash: "0xbbbb",
            tokens_generated: 3,
            inference_ms: 150,
            deterministic: true,
            engine: "local-int16",
          },
          attestation: { tx_hash: "0xcccc" },
          explorer_url: "/tx/0xcccc",
        }),
      }),
    );

    // If any fetch hits run_consensus, we fail — local should have served it.
    let hitConsensus = false;
    await page.route("**/inference/run_consensus", (route) => {
      hitConsensus = true;
      route.continue();
    });

    await page.route("**/health", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          peers: 6,
          uptime_secs: 3600,
          version: "test",
          dag_round: 1,
          dag_committed: 1,
          height: 1,
          validators: 6,
        }),
      }),
    );

    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.getByTestId("inference-prompt").fill("Hi");
    await page.getByTestId("btn-run-inference").click();

    await expect(page.getByTestId("inference-result")).toBeVisible({
      timeout: 10_000,
    });

    // Consensus banner must NOT be shown — local served directly.
    await expect(page.getByTestId("inference-consensus")).toHaveCount(0);
    expect(hitConsensus).toBe(false);
  });
});
