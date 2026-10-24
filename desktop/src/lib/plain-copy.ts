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
    quitArc: "Quit ARC",
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
    /** Running, and the connection is not known yet (older status shapes). */
    running: "Your node is running.",
    /**
     * Running, and a peer or a coordinator answered. The desktop's own probe
     * is what reached the coordinator, so the connection is ARC's, not a
     * claim about the node's P2P (an observer never talks to coordinators).
     */
    runningConnected: "Your node is running, and ARC is connected to the network.",
    /** Running, and nothing on the network has answered yet. */
    runningConnecting: "Your node is running, and ARC is connecting to the network…",
    /** Running, and nothing has answered for a while: calm, no red, nothing to press. */
    runningUnreachable:
      "Your node is running, but ARC can't reach the network yet. Check your internet connection.",
    starting: "Your node is starting. Loading the AI model can take a few minutes.",
    downloadingModel: (amount: string) =>
      `Your node is getting ready: downloading the AI model (${amount}).`,
    off: "Your node is off.",
    /** Off, and no coordinator answered either: the computer is offline, not the node broken. */
    offUnreachable:
      "Your node is off, and ARC can't reach the network. Check your internet connection.",
    crashed: "Your node stopped unexpectedly.",
    updateAvailable: "A new version of ARC is ready.",
    updating: "Updating ARC. It will restart by itself in a moment.",
    /** A relaunch or install the updater couldn't finish; only a reopen clears it. */
    updateFailed: "ARC couldn't finish an update. Quit ARC and open it again.",
  },
  /**
   * How the node reaches the network, in plain words. A community node has no
   * peers by design (arc-net admits only the validators), so it is connected
   * when a coordinator answers, and "0 peers" is normal, not a fault.
   */
  connection: {
    labels: {
      checking: "Checking",
      starting: "Starting",
      off: "Node off",
      unreachable: "No connection",
      connecting: "Connecting",
      connected: "Connected",
    },
    checking: "Reading your node's status…",
    starting: "Your node is starting.",
    off: "Your node is off.",
    unreachable:
      "Your node is off, and ARC can't reach the network. Check your internet connection.",
    connecting: "Your node is running, and ARC is connecting to the network…",
    /** Too long without an answer from any coordinator. */
    connectingLong:
      "ARC can't reach the network yet. Check your internet connection; it keeps trying.",
    /** The node itself has a peer (health "live"), so the node is connected. */
    connected: "Your node is connected to the ARC network.",
    connectedPeers: (peers: string) =>
      `Your node is connected to the ARC network with ${peers}.`,
    /**
     * A coordinator answered the desktop's own probe. That is ARC's
     * connection; the node has no peers, and an observer never talks to a
     * coordinator, so the sentence does not say the node is connected.
     */
    connectedCoordinator: (host: string) =>
      `Your node is running, and ARC is connected to the network through ${host}. ARC reaches the network through coordinators rather than node to node, so 0 peers is normal.`,
    /** The sidebar's second line while running. */
    viaPeers: (peers: string) => peers,
    viaCoordinator: (host: string) => `via ${host}`,
    viaConnecting: "connecting…",
    /** The Peers tile's explanation. */
    peersInfo:
      "Other nodes your node has a direct connection to. Community nodes connect through coordinators and normally show 0 peers; the number says nothing about work or rewards.",
  },
  /** Ask ARC: what the thread says while a question is out and when it isn't answered. */
  ask: {
    sending: "Sending…",
    askingNode: "Asking your node…",
    askingNetwork: "Asking the ARC network…",
    stoppedWaiting: "You stopped waiting",
    /** Added to the waiting note after a minute. */
    slow: "The network is taking longer than usual.",
    waitNote: "You can keep using ARC while you wait, or stop waiting below. A late answer still appears here.",
    stoppedNote: "A late answer still appears here.",
    /** No answer came back, and nothing says the request is still running. */
    noAnswer: "The network didn't answer this time.",
    noAnswerNote: "Try again in a moment.",
    /** The request was sent and may still be running somewhere. */
    unknownOutcome: "The network didn't answer, and the request may still be running.",
    unknownOutcomeNote: "Sending it again starts a second job. Waiting a minute first is safer.",
    /** No request was sent at all. */
    notSent: "Nothing could take this question right now.",
    notSentNote: "Nothing was sent, so trying again is safe.",
    sendAgainAnyway: "Send again anyway",
    /** The composer's button while a question is out. */
    asking: "Asking…",
  },
  dashboard: {
    subtitleChecking: "Reading your node's status…",
    subtitleExternal: "Your node is running under launchd or systemd.",
    subtitleStarting: "Your node is starting. Loading the AI model can take a few minutes.",
    subtitleOff: "Your node is off. Start it to join the ARC network.",
    startingBanner: (elapsed: string) =>
      `Your node is starting${elapsed}. Loading the AI model can take a few minutes; Logs shows its progress.`,
    stopFailed: "ARC couldn't stop your node.",
    stopFailedBody: "It is still running. The details below say what went wrong.",
    crashedBody: "Start it again when you're ready. The details below say what the node reported.",
    /** The node reported a problem that is not a crash: the start it made by itself failed. */
    reportedBody: "The details below say what the node reported.",
  },
  wallet: {
    balanceUnavailable: "Balance unavailable right now. ARC keeps trying.",
    /** Refused before or by the host: nothing can have been included. */
    faucetFailed: "The faucet claim didn't go through.",
    sendFailed: "The transfer didn't go through.",
    /**
     * The request went out and no usable answer came back. The host may have
     * accepted it, so the next step is to look before sending again.
     */
    faucetUnconfirmed:
      "ARC didn't get a confirmation for this faucet claim. Check your balance before claiming again.",
    sendUnconfirmed:
      "ARC didn't get a confirmation for this transfer. Check your balance before sending again.",
  },
  setup: {
    welcomeTitle: "Welcome to ARC",
    alwaysReady:
      "It can start when ARC opens, and ARC installs updates automatically unless you turn that off in Settings. Work arrives only when assigned.",
    /** Linux .deb and .rpm installs, which ARC can't update itself. */
    alwaysReadyPackageManaged:
      "It can start when ARC opens. Update ARC with the package manager you installed it with. Work arrives only when assigned.",
    readyTitle: "Ready to start",
    readyWithModel: (size: string | null) =>
      `ARC downloads its AI model${size ? ` (${size})` : ""} and starts your node. This takes a few minutes.`,
    readyWithoutModel:
      "ARC starts your node. It joins the ARC network but doesn't run AI jobs.",
    testnet:
      "Testnet: jobs and rewards aren't guaranteed, and testnet ARC has no monetary value.",
    startButton: "Start my node",
    failedHint: "Your node didn't start, but you can still use ARC. Home shows how to fix it.",
    connectingTitle: "Connecting to the ARC network",
    connecting: "Connecting to the ARC network…",
    connectingStill: "Still connecting…",
    connectingLong: "Taking longer than usual. ARC keeps trying for a little longer.",
    /** A coordinator answered ARC (named), or the node itself found a peer (null). */
    connected: (via: string | null) =>
      via ? `ARC is connected to the network via ${via} ✓` : "Your node is connected to the ARC network ✓",
  },
  updates: {
    waitingForLastSession: (version: string) =>
      `Version ${version} installs as soon as ARC finishes closing your node's last session.`,
    manualInstallOnly: (version: string) =>
      `Version ${version} can't install from inside ARC on this computer. Download it from the release page and install it by hand.`,
    autoInstallDescription:
      "On by default. ARC downloads updates in the background and installs them when no jobs are running, then restarts. Updates can install even when your node can't start.",
    /** The check itself failed, usually a connection. ARC retries on its own. */
    checkFailed:
      "ARC couldn't check for updates right now. It tries again later, and you can check again here.",
    /** An install that did not complete; the node may have been stopped for it. */
    installFailed: "The update didn't install. You can try again here.",
  },
} as const;
