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
