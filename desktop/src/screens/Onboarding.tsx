import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowRight,
  Check,
  Copy,
  Loader2,
  Network,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import clsx from "clsx";
import { useAppStore } from "../lib/store";
import { api } from "../lib/tauri";
import { LogoMark, Tagline } from "../components/Logo";
import type { Identity } from "../lib/types";

const STEPS = ["welcome", "identity", "launch"] as const;
type Step = (typeof STEPS)[number];

const fadeSlide = {
  initial: { opacity: 0, y: 16 },
  animate: { opacity: 1, y: 0 },
  exit: { opacity: 0, y: -12 },
  transition: { duration: 0.32, ease: [0.22, 1, 0.36, 1] as const },
};

export function Onboarding() {
  const [step, setStep] = useState<Step>("welcome");
  const [identity, setIdentity] = useState<Identity | null>(null);
  const [copied, setCopied] = useState(false);
  const [launching, setLaunching] = useState(false);
  const [launchStage, setLaunchStage] = useState<
    "idle" | "downloading" | "starting" | "connecting" | "claiming"
  >("idle");
  const [launchError, setLaunchError] = useState<string | null>(null);
  const [seedShown, setSeedShown] = useState(false);

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

  const finish = async () => {
    if (!identity) return;
    setLaunching(true);
    setLaunchError(null);
    // "observer" role = join consensus + validate blocks without needing a
    // 4 GB model download. Users can flip to full inference-worker mode
    // later via Settings → "Become an inference worker".
    const config = {
      role: "observer" as const,
      modelPath: null,
      rpcPort: 9090,
      p2pPort: 9091,
      autoStart: true,
      autoUpdate: true,
      dataDir: "~/.arc",
    };
    setStoreIdentity(identity);
    setStoreConfig(config);
    try {
      setLaunchStage("downloading");
      await api.ensureBinary();
      await api.saveConfig(config);
      setLaunchStage("starting");
      await api.startNode(config);
      // Poll /health until the node reports at least one peer — that's
      // when it's actually on testnet, not just a running binary.
      setLaunchStage("connecting");
      const joined = await waitForPeer({ timeoutMs: 90_000 });
      if (joined) {
        // One free top-up so the wallet isn't empty when they land on
        // the dashboard. Non-fatal if it fails (e.g. already-claimed).
        setLaunchStage("claiming");
        try {
          await api.faucetClaim();
        } catch {
          /* faucet is a best-effort welcome gift; log-silent here */
        }
      }
      setOnboarded(true);
    } catch (err) {
      setLaunchError(
        err instanceof Error
          ? err.message
          : typeof err === "string"
            ? err
            : "Unknown error starting arc-node",
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
                  Run a node on your machine. Help secure the network. Get
                  testnet ARC.
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
                      title: "One click",
                      desc: "Download, open, you're on testnet. No config, no downloads to pick.",
                    },
                    {
                      icon: ShieldCheck,
                      title: "Your identity, on-chain",
                      desc: "A BIP-39 recovery phrase generated locally — never leaves your machine.",
                    },
                    {
                      icon: Network,
                      title: "Keeps running",
                      desc: "Lives in the menu bar, starts on login, auto-updates. Nothing to babysit.",
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
                  phrase — it's the only way to restore this identity.
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
                          onClick={async () => {
                            await navigator.clipboard.writeText(
                              identity.seedPhrase,
                            );
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
                        {identity.seedPhrase.split(" ").map((word, i) => (
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
                              style={{ color: "var(--text-faint)", minWidth: 14 }}
                            >
                              {i + 1}
                            </span>
                            {word}
                          </div>
                        ))}
                        {!seedShown && (
                          <button
                            className="btn btn-secondary btn-sm"
                            onClick={() => setSeedShown(true)}
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
                      <Loader2
                        size={26}
                        style={{ animation: "spin 1s linear infinite" }}
                      />
                    </div>
                  ) : (
                    <LogoMark size={64} radius={18} variant="gradient" />
                  )}
                </div>
                <h1 className="onboarding-title">
                  {!launching
                    ? "ready to join"
                    : launchStage === "downloading"
                      ? "downloading arc-node"
                      : launchStage === "starting"
                        ? "starting your node"
                        : launchStage === "connecting"
                          ? "joining the network"
                          : launchStage === "claiming"
                            ? "claiming welcome tokens"
                            : "finishing up"}
                </h1>
                <p className="onboarding-subtitle">
                  {!launching &&
                    "We'll download the node binary, start it, and drop some testnet ARC into your wallet."}
                  {launching && launchStage === "downloading" &&
                    "Fetching the latest arc-node for your platform. ~45 MB, one-time."}
                  {launching && launchStage === "starting" &&
                    "Launching your local node."}
                  {launching && launchStage === "connecting" &&
                    "Waiting for peers — usually takes a few seconds."}
                  {launching && launchStage === "claiming" &&
                    "Asking the testnet faucet for your starter balance."}
                </p>

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
                    <div style={{ color: "var(--text-muted)", whiteSpace: "pre-wrap" }}>
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

      <style>{`
        @keyframes spin { to { transform: rotate(360deg); } }
      `}</style>
    </div>
  );
}

// Poll node_status until we see peers ≥ 1 or we time out. Returns true
// when the node has actually joined the testnet. Used in onboarding's
// launch step to gate the faucet claim on "we're actually on the chain",
// not just "arc-node's process is alive".
async function waitForPeer({ timeoutMs }: { timeoutMs: number }): Promise<boolean> {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      const s = await api.nodeStatus();
      if (s.running && s.peers >= 1) return true;
    } catch {
      /* retry */
    }
    await new Promise((r) => setTimeout(r, 2000));
  }
  return false;
}
