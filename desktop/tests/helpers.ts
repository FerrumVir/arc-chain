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
          rpcPort: 9944,
          p2pPort: 9945,
          autoStart: true,
          autoUpdate: true,
          dataDir: "~/.arc",
        },
      }),
    );
  });
}

export async function clearState(page: Page) {
  await page.addInitScript(() => {
    localStorage.clear();
  });
}
