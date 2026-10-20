/**
 * PLACEHOLDER COPY. Every string in this module is a draft awaiting approval
 * from TJ and the CTO. Components take their wording from here and nowhere
 * else, so the approved copy replaces these values and no component changes.
 */
export const PLAIN_COPY = {
  startFailure: {
    /** Launch-time reconciliation is still closing the last session. */
    reconciling: {
      title: "ARC is finishing your node's last session",
      body: "This can take a few minutes after the computer restarts. Your node starts when it's done.",
    },
    /** The previous session's shutdown or its recovery did not finish. */
    blocked: {
      title: "ARC can't start your node yet",
      body: "Its last session didn't close cleanly, and ARC couldn't finish closing it. Nothing was deleted.",
    },
    /** A path, permission, migration or another desktop needs fixing. */
    needsRepair: {
      title: "ARC needs something fixed before it can start your node",
      bodyThenRestart:
        "Fix what the details below describe, then restart ARC. Nothing was deleted.",
      bodyThenRetry:
        "Fix what the details below describe, then try again. Nothing was deleted.",
    },
    /** The release or model download failed in transit. */
    network: {
      title: "ARC couldn't download what it needs",
      body: "Check your internet connection, then try again.",
    },
    unknown: {
      title: "ARC couldn't start your node",
      body: "Nothing was deleted. The details below say what went wrong.",
    },
    /** Added under the sentence when a newer version is ready. */
    updateReady: "A newer version is ready, and it may fix this.",
  },
  actions: {
    installUpdate: "Install update",
    installing: "Installing…",
    restartArc: "Restart ARC",
    tryAgain: "Try again",
    showDetails: "Show details",
    hideDetails: "Hide details",
    copyDetails: "Copy details",
    copied: "Copied",
    copyFailed: "Couldn't copy",
  },
  updates: {
    waitingForLastSession: (version: string) =>
      `Version ${version} installs as soon as ARC finishes closing your node's last session.`,
    autoInstallDescription:
      "On by default. ARC downloads updates in the background and installs them when no jobs are running, then restarts. Updates can install even when your node can't start.",
  },
} as const;
