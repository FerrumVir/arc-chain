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
    startNode: "Start node",
    startAgain: "Start again",
    updateArc: "Update ARC",
    continueToArc: "Continue to ARC",
  },
  /** Home: the chat, renamed, and the screen ARC opens on. */
  home: {
    navLabel: "Ask ARC",
    title: "Ask ARC",
    /** The proposal's line B: true today. */
    promise: "Ask AI, and see who answered and whether another computer checked it.",
    composerLabel: "Ask ARC",
    placeholder: "Ask ARC anything…",
    ask: "Ask",
    emptyTitle: "Ask ARC anything.",
  },
  /** The one-line node strip on Home: one sentence, at most one button. */
  nodeStrip: {
    checking: "Checking your node…",
    running: "Your node is running.",
    starting: "Your node is starting. Loading the AI model can take a few minutes.",
    downloadingModel: (amount: string) =>
      `Your node is getting ready: downloading the AI model (${amount}).`,
    off: "Your node is off.",
    crashed: "Your node stopped unexpectedly.",
    updateAvailable: "A new version of ARC is ready.",
    updating: "Updating ARC. It will restart by itself in a moment.",
  },
  setup: {
    welcomeTitle: "Welcome to ARC",
    alwaysReady:
      "It can start when ARC opens, and ARC keeps itself up to date. Work arrives only when assigned.",
    readyTitle: "Ready to start",
    readyWithModel: (size: string | null) =>
      `ARC downloads its AI model${size ? ` (${size})` : ""} and starts your node. This takes a few minutes.`,
    readyWithoutModel:
      "ARC starts your node. It helps run the network but doesn't run AI jobs.",
    testnet:
      "Testnet: jobs and rewards aren't guaranteed, and testnet ARC has no monetary value.",
    startButton: "Start my node",
    failedHint: "Your node didn't start, but you can still use ARC. Home shows how to fix it.",
  },
  updates: {
    waitingForLastSession: (version: string) =>
      `Version ${version} installs as soon as ARC finishes closing your node's last session.`,
    manualInstallOnly: (version: string) =>
      `Version ${version} can't install from inside ARC on this computer. Download it from the release page and install it by hand.`,
    autoInstallDescription:
      "On by default. ARC downloads updates in the background and installs them when no jobs are running, then restarts. Updates can install even when your node can't start.",
  },
} as const;
