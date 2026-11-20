import { expect, type Locator, type Page } from "@playwright/test";

export async function openOn(page: Page, route: string) {
  await page.addInitScript((startRoute) => {
    (window as unknown as { __ARC_START_ROUTE__?: string }).__ARC_START_ROUTE__ = startRoute;
  }, route);
}

export async function seedOnboarded(page: Page, address = "99".repeat(32)) {
  await openOn(page, "dashboard");
  await page.addInitScript((workerAddress) => {
    localStorage.setItem(
      "arc-desktop-state-v1",
      JSON.stringify({
        onboarded: true,
        identity: {
          address: workerAddress,
          publicKey:
            "0x7c31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
          createdAt: Date.now(),
        },
        config: {
          role: "worker",
          modelPath: null,
          rpcPort: 9090,
          p2pPort: 9091,
          autoStart: true,
          autoUpdate: true,
          autoInstallUpdates: false,
          dataDir: "~/.arc/data-v3",
          workerThreads: null,
          computeConsent: false,
        },
        inferenceMode: "coordinator",
      }),
    );
  }, address);
}

export async function seedOnboardedLegacy(page: Page) {
  await openOn(page, "dashboard");
  await page.addInitScript(() => {
    localStorage.setItem(
      "arc-desktop-state-v1",
      JSON.stringify({
        onboarded: true,
        identity: {
          address: "99".repeat(32),
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
          autoInstallUpdates: false,
          dataDir: "~/.arc/data-v3",
          workerThreads: null,
        },
        inferenceMode: "onchain",
      }),
    );
  });
}

export async function seedMockOverrides(
  page: Page,
  overrides: Record<string, unknown>,
) {
  await page.addInitScript((o) => {
    (window as unknown as { __ARC_MOCK__?: unknown }).__ARC_MOCK__ = o;
  }, overrides);
}

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

export async function seedComputePrompt(page: Page) {
  await page.addInitScript(() => {
    (window as unknown as { __ARC_MOCK_COMPUTE_PROMPT__?: boolean }).__ARC_MOCK_COMPUTE_PROMPT__ = true;
    const stored = JSON.parse(localStorage.getItem("arc-desktop-state-v1") ?? "{}");
    if (stored.config) {
      stored.config.computeConsent = null;
      localStorage.setItem("arc-desktop-state-v1", JSON.stringify(stored));
    }
  });
}

export async function clearState(page: Page) {
  await page.addInitScript(() => {
    localStorage.clear();
  });
}

export async function seedOnboardedWithoutConfig(page: Page) {
  await openOn(page, "dashboard");
  await page.addInitScript(() => {
    localStorage.setItem(
      "arc-desktop-state-v1",
      JSON.stringify({
        onboarded: true,
        identity: {
          address: "99".repeat(32),
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

export async function walkToLaunch(page: Page) {
  await page.getByTestId("btn-continue-welcome").click();
  await page.getByTestId("btn-reveal-seed").click();
  await page.getByTestId("btn-continue-identity").click();
  await page.getByTestId("tier-skip").click();
  await page.getByTestId("btn-continue-model").click();
}

export async function wheelIntoView(page: Page, target: Locator) {
  const viewport = page.viewportSize();
  if (viewport) await page.mouse.move(viewport.width / 2, viewport.height / 2);
  await expect(target).toBeVisible();
  await expect(async () => {
    await page.mouse.wheel(0, 400);
    await expect(target).toBeInViewport({ ratio: 1, timeout: 200 });
  }).toPass({ intervals: [100], timeout: 6000 });
}
