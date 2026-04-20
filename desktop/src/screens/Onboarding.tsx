import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowRight,
  Check,
  Copy,
  Cpu,
  HardDrive,
  Loader2,
  Monitor,
  Network,
  Server,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import clsx from "clsx";
import { useAppStore } from "../lib/store";
import { api } from "../lib/tauri";
import { LogoMark, Tagline } from "../components/Logo";
import type { HardwareInfo, Identity, NodeRole } from "../lib/types";

const STEPS = ["welcome", "hardware", "role", "identity", "launch"] as const;
type Step = (typeof STEPS)[number];

const ROLE_META: Record<
  NodeRole,
  { title: string; description: string; icon: typeof Server; badge?: string }
> = {
  worker: {
    title: "Inference Worker",
    description:
      "Use your GPU to serve AI inference. Earn ARC for every verified request.",
    icon: Sparkles,
    badge: "Recommended",
  },
  validator: {
    title: "Full Validator",
    description:
      "Run consensus, produce blocks, attest to inference. Requires stake.",
    icon: Server,
  },
  verifier: {
    title: "Light Verifier",
    description:
      "Validate attestations without serving inference. Lowest resource footprint.",
    icon: ShieldCheck,
  },
};

const fadeSlide = {
  initial: { opacity: 0, y: 16 },
  animate: { opacity: 1, y: 0 },
  exit: { opacity: 0, y: -12 },
  transition: { duration: 0.32, ease: [0.22, 1, 0.36, 1] as const },
};

export function Onboarding() {
  const [step, setStep] = useState<Step>("welcome");
  const [hardware, setHardware] = useState<HardwareInfo | null>(null);
  const [role, setRole] = useState<NodeRole>("worker");
  const [identity, setIdentity] = useState<Identity | null>(null);
  const [copied, setCopied] = useState(false);
  const [launching, setLaunching] = useState(false);
  const [seedShown, setSeedShown] = useState(false);

  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const setStoreIdentity = useAppStore((s) => s.setIdentity);
  const setStoreConfig = useAppStore((s) => s.setConfig);

  const stepIndex = STEPS.indexOf(step);
  const next = () => setStep(STEPS[Math.min(stepIndex + 1, STEPS.length - 1)]);
  const back = () => setStep(STEPS[Math.max(stepIndex - 1, 0)]);

  useEffect(() => {
    if (step === "hardware" && !hardware) {
      api.detectHardware().then(setHardware);
    }
    if (step === "identity" && !identity) {
      api.generateIdentity().then(setIdentity);
    }
  }, [step, hardware, identity]);

  useEffect(() => {
    if (hardware) setRole(hardware.recommendedRole);
  }, [hardware]);

  const finish = async () => {
    if (!identity) return;
    setLaunching(true);
    const config = {
      role,
      modelPath: null,
      rpcPort: 9090,
      p2pPort: 9091,
      autoStart: true,
      autoUpdate: true,
      dataDir: "~/.arc",
    };
    setStoreIdentity(identity);
    setStoreConfig(config);
    await api.saveConfig(config);
    try {
      await api.startNode(config);
    } catch {
      /* show on dashboard */
    }
    // brief "spinning up" moment for polish
    await new Promise((r) => setTimeout(r, 900));
    setOnboarded(true);
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
                  Run a verifiable AI node on your machine.
                  Contribute compute. Earn from the network.
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
                      title: "Earn ARC continuously",
                      desc: "Every verified inference pays you in ARC tokens.",
                    },
                    {
                      icon: ShieldCheck,
                      title: "Cryptographically verifiable",
                      desc: "Your outputs are attested on-chain. No trust required.",
                    },
                    {
                      icon: Network,
                      title: "Join 1,283+ nodes",
                      desc: "Part of a real network, not a centralized provider.",
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

            {step === "hardware" && (
              <div data-testid="step-hardware">
                <h1 className="onboarding-title">Your machine</h1>
                <p className="onboarding-subtitle">
                  We checked what you have. Here's what you can run.
                </p>

                {!hardware ? (
                  <div style={{ display: "grid", gap: "var(--space-3)" }}>
                    {[0, 1, 2].map((i) => (
                      <div
                        key={i}
                        className="shimmer"
                        style={{ height: 78 }}
                      />
                    ))}
                  </div>
                ) : (
                  <>
                    <div className="hardware-summary">
                      <div className="hw-item">
                        <Cpu
                          size={16}
                          style={{
                            color: "var(--text-muted)",
                            margin: "0 auto var(--space-2)",
                          }}
                        />
                        <div className="hw-value">{hardware.cpuCores}</div>
                        <div className="hw-label">CPU cores</div>
                      </div>
                      <div className="hw-item highlight">
                        <HardDrive
                          size={16}
                          style={{
                            color: "var(--indigo-300)",
                            margin: "0 auto var(--space-2)",
                          }}
                        />
                        <div className="hw-value">{hardware.ramGb} GB</div>
                        <div className="hw-label">RAM</div>
                      </div>
                      <div className="hw-item">
                        <Monitor
                          size={16}
                          style={{
                            color: "var(--text-muted)",
                            margin: "0 auto var(--space-2)",
                          }}
                        />
                        <div className="hw-value">
                          {hardware.gpuVramGb ? `${hardware.gpuVramGb} GB` : "—"}
                        </div>
                        <div className="hw-label">GPU</div>
                      </div>
                    </div>

                    <div
                      style={{
                        padding: "var(--space-5)",
                        background: "var(--surface)",
                        border: "1px solid var(--border)",
                        borderRadius: "var(--radius-lg)",
                        marginBottom: "var(--space-4)",
                      }}
                    >
                      <div
                        style={{
                          fontSize: "var(--text-xs)",
                          fontWeight: 600,
                          letterSpacing: "var(--tracking-wider)",
                          color: "var(--text-muted)",
                          textTransform: "uppercase",
                          marginBottom: "var(--space-2)",
                        }}
                      >
                        Recommended model
                      </div>
                      <div
                        style={{
                          fontSize: "var(--text-lg)",
                          fontWeight: 600,
                          color: "var(--text)",
                          marginBottom: "var(--space-1)",
                        }}
                      >
                        {hardware.recommendedModel}
                      </div>
                      <div
                        style={{
                          fontSize: "var(--text-sm)",
                          color: "var(--text-muted)",
                        }}
                      >
                        Est. earnings:{" "}
                        <span
                          style={{
                            color: "var(--success)",
                            fontVariantNumeric: "tabular-nums",
                            fontWeight: 600,
                          }}
                        >
                          ~{hardware.estimatedDailyArc.toLocaleString()} ARC / day
                        </span>
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
                    disabled={!hardware}
                    data-testid="btn-continue-hardware"
                  >
                    Continue <ArrowRight size={16} />
                  </button>
                </div>
              </div>
            )}

            {step === "role" && (
              <div data-testid="step-role">
                <h1 className="onboarding-title">Pick your role</h1>
                <p className="onboarding-subtitle">
                  You can change this later in Settings.
                </p>

                <div className="role-grid">
                  {(Object.keys(ROLE_META) as NodeRole[]).map((r) => {
                    const meta = ROLE_META[r];
                    const Icon = meta.icon;
                    return (
                      <button
                        key={r}
                        className={clsx(
                          "role-card",
                          role === r && "selected",
                        )}
                        onClick={() => setRole(r)}
                        data-testid={`role-${r}`}
                        aria-pressed={role === r}
                      >
                        <div className="role-icon">
                          <Icon size={18} />
                        </div>
                        <div className="role-info">
                          <div className="role-title">
                            {meta.title}
                            {meta.badge && (
                              <span className="role-badge">{meta.badge}</span>
                            )}
                          </div>
                          <div className="role-description">{meta.description}</div>
                        </div>
                        {role === r && (
                          <Check
                            size={18}
                            style={{ color: "var(--indigo-300)", flexShrink: 0 }}
                          />
                        )}
                      </button>
                    );
                  })}
                </div>

                <div className="onboarding-actions">
                  <button className="btn btn-ghost" onClick={back}>
                    Back
                  </button>
                  <button
                    className="btn btn-primary"
                    onClick={next}
                    data-testid="btn-continue-role"
                  >
                    Continue <ArrowRight size={16} />
                  </button>
                </div>
              </div>
            )}

            {step === "identity" && (
              <div data-testid="step-identity">
                <h1 className="onboarding-title">Your identity</h1>
                <p className="onboarding-subtitle">
                  This is your node's on-chain address. Keep the recovery phrase
                  safe — it's the only way to restore this identity.
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
                  {launching ? "starting your node" : "ready to launch"}
                </h1>
                <p className="onboarding-subtitle">
                  {launching
                    ? "Connecting to the ARC testnet…"
                    : "You'll be connected as a "}
                  {!launching && (
                    <strong style={{ color: "var(--text)" }}>
                      {ROLE_META[role].title}
                    </strong>
                  )}
                  {!launching && ". Auto-update and persistent service will be enabled."}
                </p>

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
                      Launch node <Sparkles size={16} />
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
