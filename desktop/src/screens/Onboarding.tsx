import { useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowRight,
  Check,
  CheckCircle2,
  Copy,
  Cpu,
  HardDrive,
  Loader2,
  Network,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import clsx from "clsx";
import { useAppStore } from "../lib/store";
import { api, isTauri } from "../lib/tauri";
import { LogoMark, Tagline } from "../components/Logo";
import {
  DEFAULT_NODE_CONFIG,
  type Identity,
  type ModelDownloadProgress,
  type ModelTierInfo,
  type NodeConfig,
} from "../lib/types";

const STEPS = ["welcome", "identity", "model", "launch"] as const;
type Step = (typeof STEPS)[number];

/** Twelve dummy words, shown blurred before the user asks to reveal. */
const PLACEHOLDER_PHRASE =
  "•••••• •••••• •••••• •••••• •••••• •••••• •••••• •••••• •••••• •••••• •••••• ••••••";

const fadeSlide = {
  initial: { opacity: 0, y: 16 },
  animate: { opacity: 1, y: 0 },
  exit: { opacity: 0, y: -12 },
  transition: { duration: 0.32, ease: [0.22, 1, 0.36, 1] as const },
};

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

export function Onboarding() {
  const [step, setStep] = useState<Step>("welcome");
  const [identity, setIdentity] = useState<Identity | null>(null);
  const [copied, setCopied] = useState(false);
  const [seedShown, setSeedShown] = useState(false);
  // Held in component state only, for as long as this screen is mounted.
  // Never written to the zustand store, and therefore never persisted to
  // localStorage — see `scrubIdentity` in lib/store.ts for the history.
  const [seedPhrase, setSeedPhrase] = useState<string | null>(null);
  const [seedError, setSeedError] = useState<string | null>(null);

  // Model picker state — populated when entering the "model" step.
  const [tiers, setTiers] = useState<ModelTierInfo[]>([]);
  const [recommendedTier, setRecommendedTier] = useState<string>("standard");
  const [selectedTier, setSelectedTier] = useState<string | null>(null);

  const [launching, setLaunching] = useState(false);
  const [launchStage, setLaunchStage] = useState<
    "idle" | "model" | "downloading" | "starting" | "connecting" | "claiming"
  >("idle");
  const [modelProgress, setModelProgress] =
    useState<ModelDownloadProgress | null>(null);
  const [launchError, setLaunchError] = useState<string | null>(null);

  // Connecting stage: elapsed time + which seed was reached
  const [connectElapsed, setConnectElapsed] = useState(0);
  const [connectedVia, setConnectedVia] = useState<string | null>(null);
  const connectTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const setStoreIdentity = useAppStore((s) => s.setIdentity);
  const setStoreConfig = useAppStore((s) => s.setConfig);

  const stepIndex = STEPS.indexOf(step);
  const next = () => setStep(STEPS[Math.min(stepIndex + 1, STEPS.length - 1)]);
  const back = () => setStep(STEPS[Math.max(stepIndex - 1, 0)]);

  useEffect(() => {
    if (step === "identity" && !identity) {
      api.generateIdentity().then(setIdentity);
    }
  }, [step, identity]);

  // Load model tiers + the recommended-for-this-machine tier when we land
  // on the model step. Pre-select the recommended one so the typical user
  // can keep clicking through without thinking.
  useEffect(() => {
    if (step !== "model" || tiers.length > 0) return;
    let cancelled = false;
    Promise.all([api.listModelTiers(), api.recommendedTier()]).then(
      ([loadedTiers, rec]) => {
        if (cancelled) return;
        setTiers(loadedTiers);
        // "none" comes back when the machine isn't strong enough — pre-select
        // tiny so the user has a sensible default to override or skip.
        const safeRec = rec === "none" ? "tiny" : rec;
        setRecommendedTier(safeRec);
        setSelectedTier(safeRec);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [step, tiers.length]);

  // Subscribe to model-download-progress events from the Rust side. Outside
  // a Tauri shell (dev / browser preview) the mock just resolves the
  // download() call instantly without progress events, so the listener is a
  // no-op and the UI shows the "starting" stage straight after.
  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | undefined;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      const handle = await listen<ModelDownloadProgress>(
        "model-download-progress",
        (event) => {
          setModelProgress(event.payload);
        },
      );
      unlisten = handle;
    })();
    return () => {
      unlisten?.();
    };
  }, []);

  const finish = async () => {
    if (!identity) return;
    setLaunching(true);
    setLaunchError(null);
    setStoreIdentity(identity);

    const tier = selectedTier ?? recommendedTier;
    const wantsModel = tier !== "skip";

    try {
      // 1. Download the model first if the user picked one. Big tier is
      //    ~7.9 GB; we surface progress via the modelProgress event.
      let modelPath: string | null = null;
      if (wantsModel) {
        setLaunchStage("model");
        // If the user previously downloaded this tier (re-running onboarding,
        // re-install on same disk), skip straight to the path.
        const existing = await api.existingModelForTier(tier);
        if (existing) {
          modelPath = existing;
        } else {
          modelPath = await api.downloadModel(tier);
        }
      }

      // 2. Build the config now that we know whether we have a model. Worker
      //    role + modelPath set ⇒ node_manager passes --community-mode and the
      //    coordinator dispatches inference jobs. No model = observer
      //    (validates consensus, doesn't earn — only path for users who
      //    explicitly opt out).
      const config: NodeConfig = {
        ...DEFAULT_NODE_CONFIG,
        role: modelPath ? "worker" : "observer",
        modelPath,
      };
      setStoreConfig(config);

      // 3. Download the arc-node binary if it isn't already there.
      setLaunchStage("downloading");
      await api.ensureBinary();
      await api.saveConfig(config);

      // 4. Start the node + wait for either real peers OR a coordinator
      //    fallback (Lite mode survives residential UDP blocks).
      setLaunchStage("starting");
      await api.startNode(config);
      setLaunchStage("connecting");
      setConnectElapsed(0);
      setConnectedVia(null);
      connectTimerRef.current = setInterval(
        () => setConnectElapsed((s) => s + 1),
        1000,
      );
      const joinResult = await waitForPeer({ timeoutMs: 90_000 });
      if (connectTimerRef.current) {
        clearInterval(connectTimerRef.current);
        connectTimerRef.current = null;
      }
      if (joinResult) {
        setConnectedVia(joinResult.via ?? null);
      }
      if (joinResult) {
        setLaunchStage("claiming");
        try {
          await api.faucetClaim();
        } catch {
          /* faucet is a best-effort welcome gift; non-fatal */
        }
      }
      setOnboarded(true);
    } catch (err) {
      if (connectTimerRef.current) {
        clearInterval(connectTimerRef.current);
        connectTimerRef.current = null;
      }
      setLaunchError(
        err instanceof Error
          ? err.message
          : typeof err === "string"
            ? err
            : "Unknown error during onboarding",
      );
      setLaunching(false);
      setLaunchStage("idle");
    }
  };

  return (
    <div className="onboarding" data-testid="onboarding">
      <div className="onboarding-inner">
        <div className="onboarding-steps" aria-label="Progress">
          {STEPS.map((_, i) => (
            <span
              key={i}
              className={clsx(
                "step-dot",
                i === stepIndex && "active",
                i < stepIndex && "done",
              )}
              data-testid={`step-dot-${i}`}
            />
          ))}
        </div>

        <AnimatePresence mode="wait">
          <motion.div key={step} {...fadeSlide}>
            {step === "welcome" && (
              <div data-testid="step-welcome">
                <div
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    alignItems: "center",
                    gap: "var(--space-3)",
                    marginBottom: "var(--space-8)",
                  }}
                >
                  <LogoMark size={64} radius={18} variant="gradient" />
                  <Tagline size="sm" />
                </div>
                <h1 className="onboarding-title">welcome to arc</h1>
                <p className="onboarding-subtitle">
                  Run a node on your machine. Serve inference. Earn ARC.
                </p>

                <div
                  style={{
                    display: "grid",
                    gap: "var(--space-3)",
                    margin: "var(--space-8) 0",
                  }}
                >
                  {[
                    {
                      icon: Sparkles,
                      title: "One click setup",
                      desc: "Pick the model tier matching your hardware. We download it, configure your node, you start earning.",
                    },
                    {
                      icon: ShieldCheck,
                      title: "Your identity, on-chain",
                      desc: "A BIP-39 recovery phrase generated locally - never leaves your machine.",
                    },
                    {
                      icon: Network,
                      title: "Always on",
                      desc: "Lives in the menu bar, starts on login, auto-updates. Inference jobs land while you sleep.",
                    },
                  ].map(({ icon: Icon, title, desc }) => (
                    <div
                      key={title}
                      style={{
                        display: "flex",
                        gap: "var(--space-3)",
                        alignItems: "flex-start",
                        padding: "var(--space-4)",
                        background: "var(--surface)",
                        border: "1px solid var(--border)",
                        borderRadius: "var(--radius-md)",
                      }}
                    >
                      <div
                        style={{
                          width: 32,
                          height: 32,
                          borderRadius: "var(--radius-sm)",
                          background: "rgba(99, 102, 241, 0.12)",
                          color: "var(--indigo-300)",
                          display: "grid",
                          placeItems: "center",
                          flexShrink: 0,
                        }}
                      >
                        <Icon size={16} />
                      </div>
                      <div>
                        <div
                          style={{
                            fontWeight: 500,
                            color: "var(--text)",
                            marginBottom: 2,
                          }}
                        >
                          {title}
                        </div>
                        <div
                          style={{
                            fontSize: "var(--text-sm)",
                            color: "var(--text-muted)",
                            lineHeight: 1.5,
                          }}
                        >
                          {desc}
                        </div>
                      </div>
                    </div>
                  ))}
                </div>

                <div className="onboarding-actions">
                  <button
                    className="btn btn-primary btn-lg"
                    onClick={next}
                    data-testid="btn-continue-welcome"
                  >
                    Get started <ArrowRight size={16} />
                  </button>
                </div>
              </div>
            )}

            {step === "identity" && (
              <div data-testid="step-identity">
                <h1 className="onboarding-title">Your identity</h1>
                <p className="onboarding-subtitle">
                  This is your node's on-chain address. Save the recovery
                  phrase - it's the only way to restore this identity.
                </p>

                {!identity ? (
                  <div className="shimmer" style={{ height: 220 }} />
                ) : (
                  <>
                    <div
                      style={{
                        padding: "var(--space-4) var(--space-5)",
                        background: "var(--surface)",
                        border: "1px solid var(--border)",
                        borderRadius: "var(--radius-md)",
                        marginBottom: "var(--space-3)",
                      }}
                    >
                      <div
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--text-muted)",
                          textTransform: "uppercase",
                          letterSpacing: "var(--tracking-wider)",
                          fontWeight: 600,
                          marginBottom: "var(--space-2)",
                        }}
                      >
                        Address
                      </div>
                      <div
                        style={{
                          fontFamily: "var(--font-mono)",
                          fontSize: "var(--text-sm)",
                          color: "var(--text)",
                          wordBreak: "break-all",
                        }}
                        data-testid="identity-address"
                      >
                        {identity.address}
                      </div>
                    </div>

                    <div
                      style={{
                        padding: "var(--space-4) var(--space-5)",
                        background: "var(--surface)",
                        border: "1px solid var(--border)",
                        borderRadius: "var(--radius-md)",
                        marginBottom: "var(--space-4)",
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          alignItems: "center",
                          marginBottom: "var(--space-3)",
                        }}
                      >
                        <div
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-muted)",
                            textTransform: "uppercase",
                            letterSpacing: "var(--tracking-wider)",
                            fontWeight: 600,
                          }}
                        >
                          Recovery phrase
                        </div>
                        <button
                          className="btn btn-ghost btn-sm"
                          disabled={!seedPhrase}
                          onClick={async () => {
                            if (!seedPhrase) return;
                            await navigator.clipboard.writeText(seedPhrase);
                            setCopied(true);
                            setTimeout(() => setCopied(false), 1800);
                          }}
                          data-testid="btn-copy-seed"
                        >
                          {copied ? (
                            <>
                              <Check size={12} /> Copied
                            </>
                          ) : (
                            <>
                              <Copy size={12} /> Copy
                            </>
                          )}
                        </button>
                      </div>
                      <div
                        style={{
                          display: "grid",
                          gridTemplateColumns: "repeat(3, 1fr)",
                          gap: "var(--space-2)",
                          position: "relative",
                        }}
                      >
                        {/* Twelve blurred placeholders until the user asks
                            to see the phrase; the real words are fetched
                            from Rust at that moment. */}
                        {(seedPhrase ?? PLACEHOLDER_PHRASE)
                          .split(" ")
                          .map((word, i) => (
                          <div
                            key={i}
                            style={{
                              padding: "var(--space-2) var(--space-3)",
                              background: "var(--bg)",
                              borderRadius: "var(--radius-sm)",
                              fontFamily: "var(--font-mono)",
                              fontSize: "var(--text-sm)",
                              color: "var(--text)",
                              display: "flex",
                              gap: "var(--space-2)",
                              filter: seedShown ? "none" : "blur(6px)",
                              transition: "filter 0.2s ease",
                              userSelect: seedShown ? "text" : "none",
                            }}
                          >
                            <span
                              style={{
                                color: "var(--text-faint)",
                                minWidth: 14,
                              }}
                            >
                              {i + 1}
                            </span>
                            {word}
                          </div>
                        ))}
                        {!seedShown && (
                          <button
                            className="btn btn-secondary btn-sm"
                            onClick={async () => {
                              // Fetch on demand. The phrase crosses the IPC
                              // boundary exactly here, on an explicit user
                              // action, and goes no further.
                              try {
                                setSeedError(null);
                                setSeedPhrase(await api.revealSeedPhrase());
                                setSeedShown(true);
                              } catch (e) {
                                setSeedError(
                                  e instanceof Error ? e.message : String(e),
                                );
                              }
                            }}
                            data-testid="btn-reveal-seed"
                            style={{
                              position: "absolute",
                              inset: 0,
                              margin: "auto",
                              width: "fit-content",
                              height: "fit-content",
                            }}
                          >
                            Tap to reveal
                          </button>
                        )}
                      </div>
                      {seedError && (
                        <p
                          style={{
                            marginTop: "var(--space-2)",
                            fontSize: "var(--text-sm)",
                            color: "var(--danger)",
                          }}
                          data-testid="seed-error"
                        >
                          Could not read the recovery phrase: {seedError}
                        </p>
                      )}
                    </div>
                  </>
                )}

                <div className="onboarding-actions">
                  <button className="btn btn-ghost" onClick={back}>
                    Back
                  </button>
                  <button
                    className="btn btn-primary"
                    onClick={next}
                    disabled={!identity || !seedShown}
                    data-testid="btn-continue-identity"
                  >
                    I've saved it <ArrowRight size={16} />
                  </button>
                </div>
              </div>
            )}

            {step === "model" && (
              <div data-testid="step-model">
                <h1 className="onboarding-title">Pick your model</h1>
                <p className="onboarding-subtitle">
                  Your node serves inference for the network and earns ARC per
                  attestation. We pre-selected the tier that fits your machine.
                  Bigger model = more demand = more earnings.
                </p>

                <div
                  style={{
                    display: "grid",
                    gap: "var(--space-3)",
                    margin: "var(--space-6) 0",
                  }}
                >
                  {tiers.length === 0 && (
                    <div className="shimmer" style={{ height: 220 }} />
                  )}
                  {tiers.map((tier) => {
                    const isSelected = selectedTier === tier.id;
                    const isRecommended = recommendedTier === tier.id;
                    return (
                      <button
                        key={tier.id}
                        type="button"
                        onClick={() => setSelectedTier(tier.id)}
                        data-testid={`tier-${tier.id}`}
                        style={{
                          textAlign: "left",
                          padding: "var(--space-4)",
                          background: isSelected
                            ? "rgba(99, 102, 241, 0.10)"
                            : "var(--surface)",
                          border: `1px solid ${isSelected ? "var(--indigo-400)" : "var(--border)"}`,
                          borderRadius: "var(--radius-md)",
                          cursor: "pointer",
                          color: "var(--text)",
                          display: "flex",
                          gap: "var(--space-3)",
                          alignItems: "flex-start",
                          transition: "border-color 0.15s, background 0.15s",
                        }}
                      >
                        <div
                          style={{
                            width: 32,
                            height: 32,
                            borderRadius: "var(--radius-sm)",
                            background: "rgba(99, 102, 241, 0.12)",
                            color: "var(--indigo-300)",
                            display: "grid",
                            placeItems: "center",
                            flexShrink: 0,
                          }}
                        >
                          {tier.id === "tiny" ? (
                            <Cpu size={16} />
                          ) : tier.id === "big" ? (
                            <Sparkles size={16} />
                          ) : (
                            <HardDrive size={16} />
                          )}
                        </div>
                        <div style={{ flex: 1 }}>
                          <div
                            style={{
                              display: "flex",
                              gap: "var(--space-2)",
                              alignItems: "center",
                              marginBottom: 2,
                            }}
                          >
                            <span style={{ fontWeight: 500 }}>
                              {tier.displayName}
                            </span>
                            {isRecommended && (
                              <span
                                style={{
                                  fontSize: "var(--text-xs)",
                                  color: "var(--indigo-300)",
                                  fontWeight: 600,
                                  letterSpacing:
                                    "var(--tracking-wider)",
                                  textTransform: "uppercase",
                                }}
                              >
                                Recommended
                              </span>
                            )}
                          </div>
                          <div
                            style={{
                              fontSize: "var(--text-sm)",
                              color: "var(--text-muted)",
                              lineHeight: 1.5,
                            }}
                          >
                            {formatBytes(tier.sizeBytes)} download · one-time
                          </div>
                        </div>
                        <div
                          style={{
                            width: 18,
                            height: 18,
                            borderRadius: "50%",
                            border: `2px solid ${isSelected ? "var(--indigo-400)" : "var(--border)"}`,
                            background: isSelected
                              ? "var(--indigo-400)"
                              : "transparent",
                            display: "grid",
                            placeItems: "center",
                            flexShrink: 0,
                          }}
                        >
                          {isSelected && <Check size={10} color="white" />}
                        </div>
                      </button>
                    );
                  })}
                </div>

                <button
                  type="button"
                  onClick={() => setSelectedTier("skip")}
                  data-testid="tier-skip"
                  style={{
                    background: "none",
                    border: "none",
                    color:
                      selectedTier === "skip"
                        ? "var(--indigo-300)"
                        : "var(--text-muted)",
                    fontSize: "var(--text-sm)",
                    cursor: "pointer",
                    padding: "var(--space-2) 0",
                    textDecoration:
                      selectedTier === "skip" ? "underline" : "none",
                  }}
                >
                  Skip — run as a verifier only (no inference earnings)
                </button>

                <div className="onboarding-actions">
                  <button className="btn btn-ghost" onClick={back}>
                    Back
                  </button>
                  <button
                    className="btn btn-primary"
                    onClick={next}
                    disabled={!selectedTier}
                    data-testid="btn-continue-model"
                  >
                    Continue <ArrowRight size={16} />
                  </button>
                </div>
              </div>
            )}

            {step === "launch" && (
              <div data-testid="step-launch" style={{ textAlign: "center" }}>
                <div
                  style={{
                    display: "flex",
                    justifyContent: "center",
                    marginBottom: "var(--space-8)",
                  }}
                >
                  {launching ? (
                    <div
                      style={{
                        width: 64,
                        height: 64,
                        borderRadius: 18,
                        background: "var(--arc-gradient)",
                        display: "grid",
                        placeItems: "center",
                        color: "white",
                        boxShadow: "var(--shadow-glow-strong)",
                      }}
                    >
                      <Loader2 size={26} className="spin" />
                    </div>
                  ) : (
                    <LogoMark size={64} radius={18} variant="gradient" />
                  )}
                </div>
                <h1 className="onboarding-title">
                  {!launching
                    ? "Ready to join"
                    : launchStage === "model"
                      ? "Downloading model"
                      : launchStage === "downloading"
                        ? "Downloading arc-node"
                        : launchStage === "starting"
                          ? "Starting your node"
                          : launchStage === "connecting"
                            ? "Joining the network"
                            : launchStage === "claiming"
                              ? "Claiming welcome tokens"
                              : "Finishing up"}
                </h1>
                <p className="onboarding-subtitle">
                  {!launching &&
                    "We'll fetch the model, download the node binary, start it, and drop testnet ARC into your wallet."}
                  {launching && launchStage === "model" && modelProgress && (
                    <>
                      {formatBytes(modelProgress.downloadedBytes)} of{" "}
                      {formatBytes(modelProgress.totalBytes)} (
                      {modelProgress.totalBytes > 0
                        ? Math.floor(
                            (modelProgress.downloadedBytes /
                              modelProgress.totalBytes) *
                              100,
                          )
                        : 0}
                      %) — Hugging Face is fast, this is the bulk of the wait.
                    </>
                  )}
                  {launching &&
                    launchStage === "model" &&
                    !modelProgress &&
                    "Connecting to Hugging Face mirror..."}
                  {launching &&
                    launchStage === "downloading" &&
                    "Fetching the latest arc-node for your platform. ~45 MB."}
                  {launching &&
                    launchStage === "starting" &&
                    "Launching your local node."}
                  {launching && launchStage === "connecting" && (
                    <ConnectingStatus
                      elapsed={connectElapsed}
                      connectedVia={connectedVia}
                    />
                  )}
                  {launching &&
                    launchStage === "claiming" &&
                    "Asking the testnet faucet for your starter balance."}
                </p>

                {launching && launchStage === "model" && modelProgress && (
                  <div
                    style={{
                      width: "100%",
                      height: 8,
                      borderRadius: 4,
                      background: "var(--surface)",
                      border: "1px solid var(--border)",
                      overflow: "hidden",
                      margin: "var(--space-4) 0",
                    }}
                  >
                    <div
                      style={{
                        width:
                          modelProgress.totalBytes > 0
                            ? `${Math.min(100, (modelProgress.downloadedBytes / modelProgress.totalBytes) * 100)}%`
                            : "0%",
                        height: "100%",
                        background: "var(--arc-gradient)",
                        transition: "width 0.25s linear",
                      }}
                    />
                  </div>
                )}

                {launchError && (
                  <div
                    data-testid="launch-error"
                    style={{
                      padding: "var(--space-4)",
                      background: "rgba(248, 113, 113, 0.08)",
                      border: "1px solid rgba(248, 113, 113, 0.3)",
                      borderRadius: "var(--radius-md)",
                      marginBottom: "var(--space-4)",
                      color: "var(--text)",
                      fontSize: "var(--text-sm)",
                    }}
                  >
                    <div style={{ fontWeight: 600, marginBottom: 4 }}>
                      Couldn't start arc-node
                    </div>
                    <div
                      style={{
                        color: "var(--text-muted)",
                        whiteSpace: "pre-wrap",
                      }}
                    >
                      {launchError}
                    </div>
                  </div>
                )}

                {!launching && (
                  <div className="onboarding-actions">
                    <button className="btn btn-ghost" onClick={back}>
                      Back
                    </button>
                    <button
                      className="btn btn-primary btn-lg"
                      onClick={finish}
                      data-testid="btn-launch"
                    >
                      {launchError ? "Retry" : "Join the network"}{" "}
                      <Sparkles size={16} />
                    </button>
                  </div>
                )}
              </div>
            )}
          </motion.div>
        </AnimatePresence>
      </div>

    </div>
  );
}

const SEEDS = [
  { label: "NYC", ip: "149.28.32.76" },
  { label: "LAX", ip: "140.82.16.112" },
  { label: "AMS", ip: "136.244.109.1" },
  { label: "LHR", ip: "104.238.171.11" },
  { label: "NRT", ip: "202.182.107.41" },
  { label: "SGP", ip: "149.28.153.31" },
];

function connectingPhaseMessage(elapsed: number): string {
  if (elapsed < 10) return "Handshaking with seed nodes…";
  if (elapsed < 25) return "Waiting for QUIC peers to respond…";
  if (elapsed < 45) return "Still connecting — trying all 6 data centers…";
  return "Taking longer than usual. Falling back to coordinator mode…";
}

function ConnectingStatus({
  elapsed,
  connectedVia,
}: {
  elapsed: number;
  connectedVia: string | null;
}) {
  const activeSeedIdx = Math.floor(elapsed / 3) % SEEDS.length;

  if (connectedVia) {
    const label =
      SEEDS.find((s) => connectedVia.includes(s.ip))?.label ?? connectedVia;
    return (
      <span style={{ color: "var(--success)" }}>
        Connected via {label} coordinator ✓
      </span>
    );
  }

  return (
    <span>
      <span style={{ display: "block", marginBottom: "var(--space-3)" }}>
        {connectingPhaseMessage(elapsed)}
        <span
          style={{
            marginLeft: "var(--space-2)",
            fontFamily: "var(--font-mono)",
            color: "var(--text-muted)",
          }}
        >
          {elapsed}s
        </span>
      </span>

      {/* Seed node status row */}
      <span
        style={{
          display: "flex",
          gap: "var(--space-2)",
          justifyContent: "center",
          flexWrap: "wrap",
        }}
      >
        {SEEDS.map((seed, i) => {
          const isActive = i === activeSeedIdx && elapsed > 0;
          const isPast = elapsed > (i + 1) * 3;
          return (
            <span
              key={seed.label}
              style={{
                display: "inline-flex",
                alignItems: "center",
                gap: 4,
                padding: "2px 8px",
                borderRadius: 999,
                fontSize: "var(--text-xs)",
                fontFamily: "var(--font-mono)",
                border: `1px solid ${isActive ? "var(--indigo-400)" : "var(--border)"}`,
                background: isActive
                  ? "rgba(99,102,241,0.12)"
                  : "var(--surface)",
                color: isActive
                  ? "var(--indigo-300)"
                  : isPast
                    ? "var(--text-muted)"
                    : "var(--text-faint)",
                transition: "all 0.3s ease",
              }}
            >
              {isActive && <Loader2 size={9} className="spin" />}
              {isPast && !isActive && <CheckCircle2 size={9} />}
              {seed.label}
            </span>
          );
        })}
      </span>
    </span>
  );
}

/**
 * Poll until the LOCAL node is genuinely online.
 *
 * This used to return on the first poll, every time. `node_status` resolved
 * to a remote seed reporting 8 peers, so `s.running && s.peers >= 1` was
 * satisfied instantly — typically before arc-node had even bound its RPC
 * port. Onboarding then reported "Connected via p2p" and claimed the faucet
 * regardless of whether the local node had started at all, so a user whose
 * node failed outright still finished setup with a green checkmark. The 90s
 * timeout, the seed-cycling animation and the coordinator-fallback branch
 * were all unreachable.
 *
 * `s.running` is now genuinely local, and success additionally requires
 * either a real peer or a reachable coordinator — the local RPC answering on
 * its own only means the process started, not that it joined anything.
 */
async function waitForPeer({
  timeoutMs,
}: {
  timeoutMs: number;
}): Promise<{ via?: string } | null> {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      const s = await api.nodeStatus();
      if (s.running) {
        if (s.peers >= 1) return { via: "p2p" };
        // Local node is up but unpeered. A reachable public seed still
        // makes the app fully usable (client mode), so that counts as
        // joined — but only alongside a running local node.
        if (s.coordinatorUrl) return { via: s.coordinatorUrl };
      }
    } catch {
      /* retry */
    }
    await new Promise((r) => setTimeout(r, 2000));
  }
  return null;
}
