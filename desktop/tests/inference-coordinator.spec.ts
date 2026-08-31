// Observer / no-model nodes call a seed's community-first /inference/run
// before the standalone /inference/run_consensus fallback.
//
// Exercises the live-mode coordinator-fallback code path end-to-end:
//   1. `window.__ARC_LIVE__` makes tauri.ts hit `fetch` instead of its
//      in-process mock.
//   2. `page.route` intercepts fetch and responds:
//   3. The UI proves request order and renders either authenticated community
//      verification/settlement or, if every direct seed safely fails, the
//      sharded-consensus fallback.

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
  profile_bound: true,
  quorum_verified: true,
  deterministic: true,
  execution_profile:
    "INT8 integer (per-row, cross-platform deterministic)",
  engine:
    "INT8 integer (per-row, cross-platform deterministic) sharded pipeline (k-of-n consensus)",
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

const COMMUNITY_PAYLOAD = {
  success: true,
  routed_via: `community:0x${"11".repeat(32)}`,
  inference: {
    input: "Biggest planet?",
    output: " Jupiter.",
    output_hash: `0x${"22".repeat(32)}`,
    model_hash: `0x${"33".repeat(32)}`,
    tokens_generated: 3,
    inference_ms: 1_240,
    deterministic: true,
    engine: "integer community worker",
  },
  attestation: {
    status: "worker_certificate_handled_by_settlement",
  },
  verification: {
    method: "authenticated_shard_quorum_2_of_3_per_range",
    profile_bound: true,
    quorum_verified: true,
    execution_profile: "arc-reward-inference-v3-canonical",
    ranges: 6,
    range_position_quorums: 18,
    signatures_required_per_quorum: 2,
    replicas_contacted_per_quorum: 3,
  },
  settlement: {
    status: "pending_mined_receipt",
    tx_type: "0x25",
    tx_hash: `0x${"44".repeat(32)}`,
    job_id: `0x${"55".repeat(32)}`,
    submitted: true,
    included: false,
    confirmed: false,
    reward_arc: 2.5,
    receipt_url: `/community/reward_receipt/0x${"44".repeat(32)}`,
  },
};

test.describe("Inference - community-first coordinator routing", () => {
  test("free prompts explain requester escrow separately from receipt-backed worker rewards", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    const postUrls: string[] = [];
    page.on("request", (request) => {
      if (request.method() === "POST") postUrls.push(request.url());
    });
    await page.route("**/inference/run", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          inference: {
            input: "Free path",
            output: "Still available.",
            output_hash: "0xaaaa",
            model_hash: "0xbbbb",
            tokens_generated: 2,
            inference_ms: 10,
            deterministic: true,
            engine: "local-int16",
          },
          attestation: { tx_hash: "" },
        }),
      }),
    );
    await page.route("**/health", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          peers: 1,
          uptime_secs: 60,
          version: "test",
          dag_round: 1,
          dag_committed: 1,
          height: 1,
          validators: 1,
        }),
      }),
    );

    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    const unavailable = page.getByTestId("paid-mode-unavailable");
    await expect(unavailable).toContainText("Prompts are free; worker rewards are separate");
    await expect(unavailable).toContainText("does not sign or submit a paid requester escrow");
    await expect(unavailable).toContainText("eligible community worker");
    await expect(unavailable).toContainText("successful mined receipt");
    await expect(unavailable).toContainText("submitting the prompt is neither charged nor rewarded");
    await expect(unavailable).toContainText(
      "VRF or replica selection alone is not payment approval",
    );
    await expect(page.getByTestId("paid-mode-toggle")).toHaveCount(0);
    await expect(page.getByTestId("inference-max-fee")).toHaveCount(0);
    await expect(page.getByTestId("btn-run-inference")).toContainText(
      "Run inference",
    );
    await expect(page.getByTestId("inference-model-policy")).toHaveText(
      "model identity: reported with response",
    );
    await expect(page.getByTestId("inference-screen")).not.toContainText(
      "model: Llama-2-7B-Chat Q4",
    );
    await expect(page.getByRole("button", { name: "The largest planet is" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Water boils at" })).toBeVisible();
    await expect(page.getByRole("button", { name: /Explain zero-knowledge/ })).toHaveCount(0);

    await page.getByTestId("inference-prompt").fill("Free path");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByTestId("inference-output")).toContainText(
      "Still available",
    );
    expect(postUrls).toHaveLength(1);
    expect(new URL(postUrls[0]).pathname).toBe("/inference/run");
    expect(postUrls.some((url) => url.includes("submit_signed"))).toBe(false);
    expect(postUrls.some((url) => url.includes("/inference/onchain/"))).toBe(
      false,
    );
  });

  test("observer routes to a community worker before standalone consensus and shows proof separately from payment", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    const inferencePosts: string[] = [];
    await page.route("**/inference/run", (route) => {
      const url = new URL(route.request().url());
      inferencePosts.push(url.href);
      if (url.hostname === "127.0.0.1") {
        return route.fulfill({
          status: 503,
          contentType: "application/json",
          body: JSON.stringify({ error: "No model loaded" }),
        });
      }
      return route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(COMMUNITY_PAYLOAD),
      });
    });
    let consensusHits = 0;
    await page.route("**/inference/run_consensus", (route) => {
      consensusHits++;
      return route.fulfill({ status: 500, body: "must not be called" });
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
    await page.getByTestId("inference-prompt").fill("Biggest planet?");
    await page.getByTestId("btn-run-inference").click();

    await expect(page.getByTestId("inference-output")).toContainText("Jupiter");
    await expect(page.getByTestId("inference-community-worker")).toContainText(
      "community worker",
    );
    await expect(page.getByTestId("inference-coordinator")).toHaveText("NYC");
    const evidence = page.getByTestId("inference-consensus");
    await expect(evidence).toContainText(
      "independently checked with authenticated 2-of-3 range quorums",
    );
    await expect(evidence).toContainText("exact execution profile bound");
    await expect(evidence).toContainText("authenticated quorum verified");
    const settlement = page.getByTestId("community-settlement");
    await expect(settlement).toContainText(
      "0x25 submitted; not earned until a successful mined receipt",
    );
    await expect(page.getByTestId("inference-result")).toContainText(
      "0x25 reward tx",
    );
    await expect(page.getByTestId("inference-result")).not.toContainText(
      "0x16 claim tx",
    );
    await expect(page.getByTestId("btn-lookup-reward")).toContainText(
      "Track reward receipt",
    );
    await expect(page.getByTestId("btn-lookup-tx")).toHaveCount(0);

    expect(consensusHits).toBe(0);
    expect(inferencePosts).toHaveLength(2);
    expect(new URL(inferencePosts[0]).hostname).toBe("127.0.0.1");
    expect(new URL(inferencePosts[1]).pathname).toBe("/inference/run");
    expect(new URL(inferencePosts[1]).hostname).not.toBe("127.0.0.1");

    await page.getByTestId("btn-lookup-reward").click();
    await expect(page.getByTestId("network-screen")).toBeVisible();
    await expect(page.getByTestId("tx-lookup-input")).toHaveValue(
      `0x${"44".repeat(32)}`,
    );
  });

  test("a non-0x25 settlement cannot be credited or offered as a reward receipt", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });
    await page.route("**/inference/run", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          ...COMMUNITY_PAYLOAD,
          settlement: {
            ...COMMUNITY_PAYLOAD.settlement,
            status: "mined_success",
            tx_type: "0x16",
            included: true,
            confirmed: true,
          },
        }),
      }),
    );

    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.getByTestId("inference-prompt").fill("The largest planet is");
    await page.getByTestId("btn-run-inference").click();

    await expect(page.getByTestId("community-settlement")).toContainText(
      "unrecognized settlement type 0x16; no community reward credited",
    );
    await expect(page.getByTestId("community-settlement")).not.toContainText(
      "confirmed for the serving worker",
    );
    await expect(page.getByTestId("inference-result")).not.toContainText(
      "0x25 reward tx",
    );
    await expect(page.getByTestId("btn-lookup-reward")).toHaveCount(0);
  });

  test("a complete mined 0x25 receipt is shown as the exact confirmed protocol reward", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });
    await page.route("**/inference/run", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          ...COMMUNITY_PAYLOAD,
          settlement: {
            ...COMMUNITY_PAYLOAD.settlement,
            status: "mined_success",
            included: true,
            confirmed: true,
          },
        }),
      }),
    );

    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.getByTestId("inference-prompt").fill("The largest planet is");
    await page.getByTestId("btn-run-inference").click();

    await expect(page.getByTestId("community-settlement")).toContainText(
      "2.5 ARC confirmed for the serving worker by a successful mined 0x25 receipt",
    );
  });

  for (const invalid of [
    { label: "non-mined status", patch: { status: "pending_mined_receipt" } },
    { label: "missing submitted flag", patch: { submitted: false } },
    { label: "wrong protocol amount", patch: { reward_arc: 25 } },
    { label: "malformed transaction hash", patch: { tx_hash: "0x44" } },
    { label: "malformed job identity", patch: { job_id: "not-a-job-hash" } },
  ]) {
    test(`an internally inconsistent 0x25 receipt fails closed: ${invalid.label}`, async ({
      page,
    }) => {
      await seedOnboarded(page);
      await page.addInitScript(() => {
        (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
      });
      await page.route("**/inference/run", (route) =>
        route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({
            ...COMMUNITY_PAYLOAD,
            settlement: {
              ...COMMUNITY_PAYLOAD.settlement,
              status: "mined_success",
              included: true,
              confirmed: true,
              ...invalid.patch,
            },
          }),
        }),
      );

      await page.goto("/");
      await page.getByTestId("nav-inference").click();
      await page.getByTestId("inference-prompt").fill("The largest planet is");
      await page.getByTestId("btn-run-inference").click();

      await expect(page.getByTestId("community-settlement")).not.toContainText(
        "confirmed for the serving worker",
      );
    });
  }

  test("a claimed community assignment that may still settle never starts duplicate consensus", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    let remoteDirectHits = 0;
    let consensusHits = 0;
    await page.route("**/inference/run", (route) => {
      const isLocal = new URL(route.request().url()).hostname === "127.0.0.1";
      if (isLocal) {
        return route.fulfill({
          status: 503,
          contentType: "application/json",
          body: JSON.stringify({ error: "No model loaded" }),
        });
      }
      remoteDirectHits++;
      return route.fulfill({
        status: 504,
        contentType: "application/json",
        body: JSON.stringify({
          error:
            "Community inference did not complete within its verified dispatch budget. The assignment may still settle; query its job status rather than starting duplicate local work.",
        }),
      });
    });
    await page.route("**/inference/run_consensus", (route) => {
      consensusHits++;
      return route.fulfill({ status: 500, body: "must not be called" });
    });
    await page.route("**/health", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ peers: 6, uptime_secs: 1, validators: 6 }),
      }),
    );

    await page.goto("/");
    await page.getByTestId("nav-inference").click();
    await page.getByTestId("inference-prompt").fill("Do this once");
    await page.getByTestId("btn-run-inference").click();

    await expect(page.getByTestId("inference-error")).toContainText(
      "may still settle",
    );
    expect(remoteDirectHits).toBe(1);
    expect(consensusHits).toBe(0);
  });

  test("standalone consensus runs only after every direct coordinator safely returns 503", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
    });

    // Local node rejects inference (observer role, no model loaded).
    let remoteDirectFailures = 0;
    await page.route("**/inference/run", (route) => {
      if (new URL(route.request().url()).hostname !== "127.0.0.1") {
        remoteDirectFailures++;
      }
      return route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({
          error: "Coordinator needs a tokenizer loaded.",
        }),
      });
    });

    // Only after direct /inference/run has failed safely on every configured
    // seed may the first seed answer via standalone sharded consensus.
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
    await expect(banner).toContainText("coordinator reports");
    await expect(banner).toContainText("exact execution profile bound");
    await expect(banner).toContainText("authenticated quorum verified");
    // This payload has no claim transaction. Coordinator agreement is
    // recomputation evidence; the UI must not invent a claim or payment.
    await expect(page.getByTestId("inference-result")).not.toContainText(
      /reward|paid|ARC/,
    );

    // Output text is the coordinator's real answer, not the mock stub.
    await expect(page.getByTestId("inference-output")).toContainText(
      "Jupiter",
    );

    // Only one coordinator was hit (NYC succeeded first - no retry).
    expect(coordHitCount).toBe(1);
    expect(remoteDirectFailures).toBe(6);
  });

  test("local node healthy (returns real output) - no coordinator fallback", async ({
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

    // If any fetch hits run_consensus, we fail - local should have served it.
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

    // The provenance banner is now always shown - it reports WHICH machine
    // served the request, not merely that consensus happened. For a locally
    // served run it must say so, and the consensus details (k=, votes) must
    // be absent. This is a stronger assertion than the previous
    // `toHaveCount(0)`, which only proved the banner was missing.
    const banner = page.getByTestId("inference-consensus");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("your node");
    await expect(banner).not.toContainText("k=");
    await expect(banner).toContainText(
      "no independent replica-agreement evidence returned",
    );
    await expect(page.getByTestId("inference-result")).toContainText(
      "serving host reports deterministic",
    );
    expect(hitConsensus).toBe(false);
  });
});
