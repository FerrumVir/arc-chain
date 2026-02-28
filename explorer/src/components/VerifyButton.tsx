"use client";

import { useState, useCallback } from "react";
import { Shield, ShieldCheck, ShieldX, Loader2 } from "lucide-react";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type VerifyState = "idle" | "running" | "verified" | "failed";

interface VerifyButtonProps {
  onVerify: () => Promise<{ valid: boolean; timeMs: number }>;
  size?: "sm" | "md";
  label?: string;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export default function VerifyButton({
  onVerify,
  size = "md",
  label = "ARC Verify",
}: VerifyButtonProps) {
  const [state, setState] = useState<VerifyState>("idle");
  const [timeMs, setTimeMs] = useState<number>(0);

  const handleClick = useCallback(async () => {
    if (state === "running") return;
    setState("running");
    setTimeMs(0);

    try {
      const result = await onVerify();
      setTimeMs(result.timeMs);
      setState(result.valid ? "verified" : "failed");
    } catch {
      setState("failed");
    }
  }, [onVerify, state]);

  const isSm = size === "sm";

  // --- State-driven classes ---

  const baseClasses = `
    relative overflow-hidden inline-flex items-center gap-1.5 rounded-lg
    font-medium transition-all duration-300 select-none
    ${isSm ? "px-2.5 py-1.5 text-[11px]" : "px-3.5 py-2 text-[12px]"}
    focus:outline-none focus-visible:ring-2 focus-visible:ring-[var(--accent)]/50
  `;

  const stateClasses: Record<VerifyState, string> = {
    idle: `
      border border-[var(--accent)]/40 bg-[var(--accent-light)]
      text-[var(--accent)] cursor-pointer
      hover:border-[var(--accent)] hover:shadow-[0_0_16px_var(--accent-glow)]
      active:scale-[0.97]
    `,
    running: `
      border border-[var(--accent)]/30 bg-[var(--accent-light)]
      text-[var(--accent)] cursor-wait
    `,
    verified: `
      border border-[var(--shield-green)]/40 bg-[var(--shield-green-bg)]
      text-[var(--shield-green)] cursor-default
    `,
    failed: `
      border border-[var(--shield-red)]/40 bg-[var(--shield-red-bg)]
      text-[var(--shield-red)] cursor-default
    `,
  };

  // Icon sizing
  const iconSize = isSm ? "w-3 h-3" : "w-3.5 h-3.5";

  return (
    <button
      onClick={handleClick}
      disabled={state === "running"}
      className={`${baseClasses} ${stateClasses[state]}`}
      aria-label={
        state === "idle"
          ? label
          : state === "running"
          ? "Verifying..."
          : state === "verified"
          ? "Cryptographically Verified"
          : "Verification Failed"
      }
    >
      {/* --- Scanning line overlay (running state only) --- */}
      {state === "running" && (
        <span
          className="absolute inset-0 pointer-events-none"
          aria-hidden="true"
        >
          {/* Sweeping scan line */}
          <span
            className="absolute top-0 bottom-0 w-8 animate-arc-scan"
            style={{
              background:
                "linear-gradient(90deg, transparent, var(--accent-glow), transparent)",
            }}
          />
          {/* Subtle full-width pulse */}
          <span className="absolute inset-0 animate-pulse-glow rounded-lg" />
        </span>
      )}

      {/* --- Verified glow ring --- */}
      {state === "verified" && (
        <span
          className="absolute inset-0 rounded-lg pointer-events-none"
          style={{
            boxShadow: "0 0 12px rgba(74, 158, 110, 0.25), inset 0 0 8px rgba(74, 158, 110, 0.08)",
          }}
          aria-hidden="true"
        />
      )}

      {/* --- Icon --- */}
      <span className="relative z-10 shrink-0 flex items-center justify-center">
        {state === "idle" && (
          <Shield className={iconSize} strokeWidth={2.5} />
        )}
        {state === "running" && (
          <Loader2
            className={`${iconSize} animate-spin`}
            strokeWidth={2.5}
          />
        )}
        {state === "verified" && (
          <ShieldCheck
            className={`${iconSize} animate-proof-check`}
            strokeWidth={2.5}
          />
        )}
        {state === "failed" && (
          <ShieldX className={iconSize} strokeWidth={2.5} />
        )}
      </span>

      {/* --- Label --- */}
      <span className="relative z-10 whitespace-nowrap">
        {state === "idle" && label}
        {state === "running" && "Verifying\u2026"}
        {state === "verified" && (
          <>
            Verified
            <span className="ml-1.5 opacity-70 font-hash">
              {timeMs < 1
                ? `${(timeMs * 1000).toFixed(0)}\u00B5s`
                : `${timeMs.toFixed(1)}ms`}
            </span>
          </>
        )}
        {state === "failed" && "Failed"}
      </span>
    </button>
  );
}
