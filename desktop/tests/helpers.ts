import type { Page } from "@playwright/test";

// Seed the app into a "post-onboarding" state so dashboard tests don't need to
// walk the wizard. Writes the same shape the zustand store persists.
export async function seedOnboarded(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem(
      "arc-desktop-state-v1",
      JSON.stringify({
        onboarded: true,
        identity: {
          // No seedPhrase: the store no longer accepts one, and
          // scrubIdentity() would strip it on load anyway. See
          // seedOnboardedLegacy() for a fixture that still has it.
          address: "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p",
          publicKey:
            "0x7c31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
          createdAt: Date.now(),
        },
        config: {
          role: "worker",
          modelPath: null,
          // Match the real defaults (9090/9091), not the old 9944/9945.
          rpcPort: 9090,
          p2pPort: 9091,
          autoStart: true,
          autoUpdate: true,
          dataDir: "~/.arc",
          workerThreads: null,
        },
        inferenceMode: "coordinator",
      }),
    );
  });
}

/**
 * A state blob written by an older build — recovery phrase and all — for
 * testing that the scrub-on-load path actually evicts it.
 */
export async function seedOnboardedLegacy(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem(
      "arc-desktop-state-v1",
      JSON.stringify({
        onboarded: true,
        identity: {
          address: "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p",
          publicKey:
            "0x7c31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
          seedPhrase:
            "galaxy stellar quantum horizon crystal ember aurora silent mirror ocean celestial fragment",
          createdAt: Date.now(),
        },
        config: {
          role: "worker",
          modelPath: null,
          rpcPort: 9090,
          p2pPort: 9091,
          autoStart: true,
          autoUpdate: true,
          dataDir: "~/.arc",
          workerThreads: null,
        },
        // The removed on-chain mode, to exercise the coercion on load.
        inferenceMode: "onchain",
      }),
    );
  });
}

/**
 * Force specific mock command results, so the degraded paths are reachable.
 *
 * The endpoints behind the projection and Network screens are newer than the
 * deployed seed binaries, so their real behaviour today is a 404 that has to
 * degrade to a stated reason. A test cannot make a Rust process 404 on demand,
 * and the honest-degradation copy is the part most worth locking down — so the
 * mock layer takes per-command overrides (see `mockOverride` in lib/tauri.ts).
 *
 * Keys are Tauri command names, e.g. `fetch_earnings_projection`.
 */
export async function seedMockOverrides(
  page: Page,
  overrides: Record<string, unknown>,
) {
  await page.addInitScript((o) => {
    (window as unknown as { __ARC_MOCK__?: unknown }).__ARC_MOCK__ = o;
  }, overrides);
}

/** A `/worker/earnings` 404, phrased the way the fetch layer phrases it. */
export const PROJECTION_404 = {
  sourceHost: "http://140.82.16.112:9090",
  unavailable:
    "http://140.82.16.112:9090 does not serve /worker/earnings/abc (HTTP 404).",
  rewardPerAttestation: null,
  rewardRateSource: "unknown",
  communityRewardsEnabled: null,
  projectedDailyArc: null,
  projectedDailyUnavailableReason: null,
  rewardPolicyHash: null,
  rewardBudgetEpoch: null,
  rewardsRemainingThisEpoch: null,
  workerRewardsRemainingThisEpoch: null,
  coordinatorRewardsRemainingThisEpoch: null,
  issuanceReadyForWorker: null,
  rewardProgram: null,
  rewardIsCustomerDemand: null,
  attestationsTotal: 0,
  firstAttestationBlock: null,
  attestationsPerDay: null,
  rateUnavailableReason: null,
  observedOverBlocks: null,
  rateCaveat: null,
};

/**
 * The endpoint answered, but has no attestation history to measure a rate
 * from. This is the state a brand-new node is in, and the one where inventing
 * a projection would be easiest and worst.
 */
export const PROJECTION_NO_HISTORY = {
  sourceHost: "http://140.82.16.112:9090",
  unavailable: null,
  rewardPerAttestation: 2.5,
  rewardRateSource: "chain",
  communityRewardsEnabled: true,
  projectedDailyArc: null,
  projectedDailyUnavailableReason:
    "No successful mined reward receipts are retained, so the coordinator withheld a forecast.",
  rewardPolicyHash: "0xpolicy",
  rewardBudgetEpoch: 1,
  rewardsRemainingThisEpoch: 40,
  workerRewardsRemainingThisEpoch: 8,
  coordinatorRewardsRemainingThisEpoch: 16,
  issuanceReadyForWorker: true,
  rewardProgram: "protocol-capped testnet promotional compute subsidy",
  rewardIsCustomerDemand: false,
  attestationsTotal: 0,
  firstAttestationBlock: null,
  attestationsPerDay: null,
  rateUnavailableReason:
    "No attestations credited to this address yet, so there is no history to measure a rate from.",
  observedOverBlocks: null,
  rateCaveat: null,
};

/**
 * A `/economics/rewards` 404 — no treasury figures and, importantly, no
 * community-certificate bond terms.
 *
 * Losing this endpoint costs the projection its ceiling and certificate
 * terms, so the assumptions line must say no deduction was assumed.
 */
export const ECONOMICS_404 = {
  sourceHost: "http://140.82.16.112:9090",
  unavailable:
    "http://140.82.16.112:9090 does not serve /economics/rewards (HTTP 404).",
  rewardPerAttestation: null,
  treasuryBalanceArc: null,
  treasuryBalanceUnavailableReason: null,
  attestationsRemaining: null,
  attestationsRemainingUnavailableReason: null,
  treasuryIsFinite: null,
  bondPerAttestation: null,
  challengePeriodBlocks: null,
  bondRefundedAfterChallengePeriod: null,
  fundingDetail: null,
};

/**
 * The treasury endpoint answered, but could not read the treasury account.
 *
 * A distinct state from a 404: the endpoint exists and gave a reason, which
 * must be shown instead of a figure.
 */
export const ECONOMICS_NO_BALANCE = {
  ...ECONOMICS_404,
  unavailable: null,
  rewardPerAttestation: 2.5,
  treasuryBalanceUnavailableReason:
    "Treasury account 0xtreasury… is not present in this host's state.",
  attestationsRemainingUnavailableReason:
    "Cannot compute a remaining count without a treasury balance.",
  treasuryIsFinite: true,
  bondPerAttestation: 0,
  challengePeriodBlocks: null,
  bondRefundedAfterChallengePeriod: null,
};

export async function clearState(page: Page) {
  await page.addInitScript(() => {
    localStorage.clear();
  });
}

/**
 * Onboarded, but with NO stored node config.
 *
 * This is a real state — an install whose store.json predates the config
 * block, or one whose config failed to persist. It is the case where the
 * Settings screen has to fall back to defaults, and where `save()` used to
 * `return` early and silently do nothing.
 */
export async function seedOnboardedWithoutConfig(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem(
      "arc-desktop-state-v1",
      JSON.stringify({
        onboarded: true,
        identity: {
          address: "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p",
          publicKey:
            "0x7c31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
          createdAt: Date.now(),
        },
        config: null,
        inferenceMode: "coordinator",
      }),
    );
  });
}

/**
 * Walk the onboarding wizard from a clean state to the launch step.
 *
 * The wizard has FOUR steps — welcome → identity → **model** → launch. It
 * gained the model picker in v0.6.0, when "did the user download a model?"
 * became the thing that decides worker vs observer. Several specs still
 * walked three steps and asserted `step-launch` straight after identity, so
 * they stalled on "Pick your model" and failed.
 *
 * Picks "skip" (verifier-only), which keeps the walk fast and implies no
 * GGUF download.
 */
export async function walkToLaunch(page: Page) {
  await page.getByTestId("btn-continue-welcome").click();
  await page.getByTestId("btn-reveal-seed").click();
  await page.getByTestId("btn-continue-identity").click();
  await page.getByTestId("tier-skip").click();
  await page.getByTestId("btn-continue-model").click();
}
