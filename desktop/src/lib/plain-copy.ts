export const PLAIN_COPY = {
  startFailure: {
    otherDesktop: {
      title: "ARC is already open on this computer.",
      body: "Look for the ARC icon near the clock (Windows) or in the menu bar (Mac) and open it from there. You can close this window.",
    },
    closing: { title: "ARC is closing", body: "Please wait while your node closes." },
    reconciling: {
      title: "ARC is finishing your node's last session",
      body: "This can take a few minutes after the computer restarts. Your node starts when it's done.",
    },
    blocked: {
      title: "ARC can't start your node yet",
      body: "Its last session didn't close cleanly, and ARC couldn't finish closing it. Nothing was deleted.",
    },
    needsRepair: {
      title: "ARC needs something fixed before it can start your node",
      bodyThenRestart:
        "Fix what the details below describe, then restart ARC. Nothing was deleted.",
      bodyThenRetry:
        "Fix what the details below describe, then try again. Nothing was deleted.",
    },
    network: {
      title: "ARC couldn't download what it needs",
      body: "Check your internet connection, then try again.",
    },
    unknown: {
      title: "ARC couldn't start your node",
      body: "Nothing was deleted. The details below say what went wrong.",
    },
    updateUnfinished: {
      title: "ARC couldn't finish an update. Quit ARC and open it again.",
      body: "Your node stays stopped until then. Nothing was deleted.",
    },
    updateReady: "A newer version is ready, and it may fix this.",
  },
  actions: {
    installUpdate: "Install update",
    installing: "Installing…",
    restartArc: "Restart ARC",
    quitArc: "Quit ARC",
    closeCopy: "Close this copy",
    tryAgain: "Try again",
    showDetails: "Show details",
    hideDetails: "Hide details",
    copyDetails: "Copy details",
    copied: "Copied",
    copyFailed: "Couldn't copy",
    startNode: "Start node",
    shareComputer: "Share my computer",
    startAgain: "Start again",
    updateArc: "Update ARC",
    continueToArc: "Continue to ARC",
  },
  home: {
    navLabel: "Ask ARC",
    title: "Ask ARC",
    promise: "ARC is a network of computers that answer AI questions for each other.",
    explanation: "Ask a question and one of them answers. Share your computer and it answers for others.",
    askTitle: "Ask for free",
    askBody: "Type a question. A computer on the network works out the answer.",
    proofTitle: "See who answered",
    proofBody: "ARC shows which computer answered, and whether a second computer checked it.",
    shareTitle: "Share if you want",
    shareBody: "Your computer can answer questions for other people. You can turn this off at any time.",
    independentQuestions: "Each question is answered on its own. Earlier messages are not sent with it.",
    composerLabel: "Ask ARC",
    placeholder: "Ask ARC anything…",
    ask: "Ask",
    emptyTitle: "Ask ARC anything.",
  },
  nodeStrip: {
    ready: "Your computer is ready to answer questions.",
    computing: "Your computer is answering a question right now.",
    preparing: "Your computer is getting ready to answer questions.",
    sharingOff: "Your computer is not sharing.",
    sharingIneligible: "This computer cannot share: it has less than 16 GB of memory.",
    shareDownload: "Sharing downloads the AI model (3.80 GB) if it is not already on this computer.",
    enablingSharing: "ARC is preparing to share. It will download the AI model (3.80 GB) if needed.",
    reconnectingWorker: "Your computer is reconnecting before it can answer questions.",
    partialStarting: "Your computer is starting shared AI work.",
    partialLoading: "Your computer is loading part of the AI model.",
    partialReady: "Your computer is ready to help with shared AI steps (no rewards).",
    partialSteps: (n: number) =>
      `Your computer has helped with ${n} ${n === 1 ? "shared AI step" : "shared AI steps"} since it started (no rewards).`,
    partialComputing: "Your computer is helping with a shared AI step right now (no rewards).",
    partialReconnecting: "Your computer is reconnecting before it can help with shared AI steps.",
    partialFailed: "Shared AI work stopped because of an error.",
    partialStopped: "Shared AI work is stopped.",
    workerStatusUnavailable: "Your computer's sharing status could not be confirmed.",
    answered: (n: number) => `Your computer has answered ${n} ${n === 1 ? "question" : "questions"} since it started.`,
    online: (n: number) => `${n} ${n === 1 ? "computer is" : "computers are"} online.`,
    registered: (n: number) => `${n} ${n === 1 ? "computer has" : "computers have"} registered.`,
    checking: "Checking your node…",
    running: "Your node is running.",
    runningConnected: "Your node is running, and ARC is connected to the network.",
    runningConnecting: "Your node is running, and ARC is connecting to the network…",
    runningUnreachable:
      "Your node is running, but ARC can't reach the network yet. Check your internet connection.",
    starting: "Your node is starting. Loading the AI model can take a few minutes.",
    downloadingModel: (amount: string) =>
      `Your node is getting ready: downloading the AI model (${amount}).`,
    modelWaitingBattery: "The AI model download is waiting; plug this computer in and ARC will try again.",
    modelWaitingSpace: (amount: string) =>
      `The AI model download needs ${amount} of free space; ARC will check again automatically.`,
    modelWaitingFailed: "The last AI model download failed; ARC will try again automatically.",
    modelWaitingMetered:
      "The AI model download is waiting for a connection without a data limit; ARC will check again automatically.",
    off: "Your node is off.",
    offUnreachable:
      "Your node is off, and ARC can't reach the network. Check your internet connection.",
    crashed: "Your node stopped unexpectedly.",
    crashRetry: (minutes: number) =>
      `Your node stopped unexpectedly; ARC will try again in ${minutes} ${minutes === 1 ? "minute" : "minutes"}.`,
    updateAvailable: "A new version of ARC is ready.",
    updating: "Updating ARC. It will restart by itself in a moment.",
    updateFailed: "ARC couldn't finish an update. Quit ARC and open it again.",
  },
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
    connectingLong:
      "ARC can't reach the network yet. Check your internet connection; it keeps trying.",
    connected: "Your node is connected to the ARC network.",
    connectedPeers: (peers: string) =>
      `Your node is connected to the ARC network with ${peers}.`,
    connectedCoordinator: (host: string) =>
      `Your node is running, and ARC is connected to the network through ${host}. ARC reaches the network through coordinators rather than node to node, so 0 peers is normal.`,
    viaPeers: (peers: string) => peers,
    viaCoordinator: (host: string) => `via ${host}`,
    viaConnecting: "connecting…",
    peersInfo:
      "Other nodes your node has a direct connection to. Community nodes connect through coordinators and normally show 0 peers; the number says nothing about work or rewards.",
  },
  ask: {
    answerLength: "Answer length",
    sending: "Sending…",
    askingNode: "Asking your node…",
    askingNetwork: "Asking the ARC network…",
    stoppedWaiting: "You stopped waiting",
    slow: "The network is taking longer than usual.",
    waitNote: "You can keep using ARC while you wait, or stop waiting below. A late answer still appears here.",
    stoppedNote: "A late answer still appears here.",
    noAnswer: "The network didn't answer this time.",
    noAnswerNote: "Try again in a moment.",
    unknownOutcome: "The network didn't answer, and the request may still be running.",
    unknownOutcomeNote: "Sending it again starts a second job. Waiting a minute first is safer.",
    notSent: "Nothing could take this question right now.",
    notSentNote: "Nothing was sent, so trying again is safe.",
    sendAgainAnyway: "Send again anyway",
    asking: "Asking…",
  },
  answer: {
    own: "Your own computer answered this.",
    network: "A computer on the ARC network answered this.",
    arc: "ARC's own computers answered this.",
    served: { own: "your computer", arc: "ARC's computers", network: "the ARC network" },
    checked: "A second computer got the same answer.",
    reported: "ARC could not confirm a second computer checked this answer.",
    unchecked: "No second-computer check was reported.",
    recorded: "Recorded",
    proof: "Show the proof",
  },
  dashboard: {
    questions: "Questions answered",
    questionsInfo: "Answers accepted since this node started. These are not reward receipts.",
    checked: "Checked by the network",
    checkedUnavailable: "The total is not available. The node reports some checks, but does not count checks on practice questions here.",
    online: "Computers online",
    onlineInfo: "The latest count from the network. If the registry may include old entries, no online count is shown.",
    uptime: "Running for",
    uptimeInfo: "Time since this node started.",
    advanced: "Advanced",
    reportedChecks: "Reported checks",
    reportedChecksInfo: "Checks reported by the network since this node started. Checks on practice questions are not included.",
    subtitleChecking: "Reading your node's status…",
    subtitleExternal: "Your node is running under launchd or systemd.",
    subtitleStarting: "Your node is starting. Loading the AI model can take a few minutes.",
    subtitleOff: "Your node is off. Start it to join the ARC network.",
    startingBanner: (elapsed: string) =>
      `Your node is starting${elapsed}. Loading the AI model can take a few minutes; Logs shows its progress.`,
    stopFailed: "ARC couldn't stop your node.",
    stopFailedBody: "It is still running. The details below say what went wrong.",
    crashedBody: "Start it again when you're ready. The details below say what the node reported.",
    reportedBody: "The details below say what the node reported.",
  },
  wallet: {
    balanceUnavailable: "Balance unavailable right now. ARC keeps trying.",
    faucetFailed: "The faucet claim didn't go through.",
    sendFailed: "The transfer didn't go through.",
    faucetUnconfirmed:
      "ARC didn't get a confirmation for this faucet claim. Check your balance before claiming again.",
    sendUnconfirmed:
      "ARC didn't get a confirmation for this transfer. Check your balance before sending again.",
  },
  setup: {
    welcomeTitle: "Welcome to ARC",
    compute: {
      title: "Share your computer's spare power",
      body: "ARC Node can run AI jobs for the ARC network on this computer. It uses up to a quarter of your processor, at low priority so your own apps come first, and about 8 GB of memory while it is ready for jobs. It keeps running on battery. First it downloads the ARC model, about 4 GB, in the background. You can turn this off here or at any time in Settings.",
      control: "Contribute compute",
      belowMemoryFloor:
        "This computer has less than 16 GB of memory, which the ARC model needs, so this starts off. If you turn it on, setup downloads the model (about 4 GB) before your node starts. ARC Node can then run AI jobs for the ARC network on this computer. It uses up to a quarter of your processor, at low priority so your own apps come first, and about 8 GB of memory while it is ready for jobs. It keeps running on battery.",
      turnOn: "Turn on",
      notNow: "Not now",
    },
    readyTitle: "Ready to start",
    readyWithModel: (size: string | null) =>
      `ARC downloads its AI model${size ? ` (${size})` : ""} and starts your node. This takes a few minutes.`,
    readyWithModelInBackground: (size: string | null) =>
      `ARC starts your node, then downloads its AI model${size ? ` (${size})` : ""} in the background. Your node can run AI jobs once the model is ready.`,
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
    connected: (via: string | null) =>
      via ? `ARC is connected to the network via ${via} ✓` : "Your node is connected to the ARC network ✓",
  },
  settings: {
    packageUpdates: "ARC cannot install updates for this package. Update ARC with the package manager you installed it with.",
    startupTitle: "Starting with your computer",
    startupOn: "ARC opens when you log in and starts your node.",
    startupOff: "ARC does not start your node by itself. You can turn that on below.",
    startupUnregistered: "ARC is not in your login items, so it will not open by itself after a restart.",
    startupChecking: "ARC is checking whether it can open when you log in.",
    startupUnknown: "ARC could not confirm whether it can open when you log in.",
    startupModel: "Your node can answer questions after its model is ready.",
    startupNoModel: "Your node needs a model before it can answer questions.",
    computeShare: (threads: number | null, cores: number) => {
      const share =
        threads === null || threads >= cores
          ? "your whole processor"
          : threads * 4 <= cores
            ? "a quarter of your processor"
            : `${threads} of your ${cores} processor cores`;
      return `Uses up to ${share} at low priority, and about 8 GB of memory while ARC Node is running. It keeps running on battery. Turn this off to stop ARC jobs and free that memory.`;
    },
  },
  previousInstall: {
    sentence: (path: string) =>
      `ARC found node data from an earlier install and set it aside (${path}). If you had a wallet, restore it with your 12 words; your node will sync again from the start.`,
    dismiss: "Dismiss this notice",
    dismissFailed: "Could not dismiss this notice:",
  },
  updates: {
    waitingForLastSession: (version: string) =>
      `Version ${version} installs as soon as ARC finishes closing your node's last session.`,
    manualInstallOnly: (version: string) =>
      `Version ${version} can't install from inside ARC on this computer. Download it from the release page and install it by hand.`,
    wrongLocation:
      "ARC can't update itself from where it's running. Quit ARC, drag ARC Node into your Applications folder, then open it from there.",
    autoInstallDescription:
      "On by default. ARC downloads updates in the background and installs them when no jobs are running, then restarts. Updates can install even when your node can't start.",
    checkFailed:
      "ARC couldn't check for updates right now. It tries again later, and you can check again here.",
    installFailed: "The update didn't install. You can try again here.",
  },
  workerJobs: {
    wholeStates: {
      polling: "Ready: waiting for a job",
      computing: "Computing a job now",
      reconnecting: "Reconnecting to coordinators",
      running: "Running",
      notTakingJobs: "Not taking jobs",
    },
    wholeStarting: "The worker is starting.",
    wholeNotContributing:
      "Compute contribution is off. Turn it on in Settings to take ARC jobs on this computer.",
    partialStates: {
      starting: "Starting shared work",
      loading: "Loading part of the AI model",
      ready: "Ready for shared steps (no rewards)",
      computing: "Running a shared step (no rewards)",
      reconnecting: "Reconnecting for shared work",
      failed: "Shared work failed",
      stopped: "Shared work stopped",
    },
    partialSummaries: {
      starting: "Shared AI work is starting on this computer.",
      loading: "This computer is loading part of the AI model.",
      ready: "This computer is ready to help with shared AI steps.",
      computing: "This computer is helping with a shared AI step right now.",
      reconnecting: "Reconnecting before this computer can take shared AI steps.",
      reconnectingExpired:
        "The previous assignment expired. Reconnecting before this computer can take shared AI steps.",
      failed: "Shared AI work stopped because of an error.",
      stopped: "Shared AI work is stopped.",
    },
    invalidState: "Status could not be confirmed",
    invalidSummary:
      "ARC could not confirm this computer's sharing status. No work is counted as earning rewards.",
    unknownModeSummary:
      "This computer reported a sharing mode that this version of ARC does not recognize. No work is counted as earning rewards.",
    partialNoRewards: "Shared steps do not earn rewards and are not counted as completed questions.",
    partialNoStepsYet: "No shared step sent since the node started.",
    partialStepsRunLabel: "Steps run",
    partialStepsSentLabel: "Steps sent",
    partialAssignedPartLabel: "Assigned part",
    partialAssignedPart: (layers: number, totalLayers: number) =>
      `${layers} of ${totalLayers} layers`,
  },
} as const;
