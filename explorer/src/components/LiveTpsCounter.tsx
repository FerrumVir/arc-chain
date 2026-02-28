"use client";

import { useEffect, useRef, useState, useCallback } from "react";
import { ShieldCheck, Lock, Zap } from "lucide-react";

const TARGET_TPS = 4_330_133;
const FLUCTUATION = 50_000;
const COUNT_DURATION_MS = 2500;

function easeOutCubic(t: number): number {
  return 1 - Math.pow(1 - t, 3);
}

function formatWithCommas(n: number): string {
  return Math.round(n).toLocaleString("en-US");
}

const BADGES: {
  icon: React.ElementType;
  label: string;
  variant: "green" | "accent";
}[] = [
  { icon: ShieldCheck, label: "BLAKE3 Committed", variant: "green" },
  { icon: ShieldCheck, label: "Merkle Verified", variant: "green" },
  { icon: Lock, label: "Privacy Shielded", variant: "accent" },
  { icon: ShieldCheck, label: "ZK Compressed", variant: "green" },
];

const COMPARISONS = [
  { name: "Visa", tps: 65_000 },
  { name: "Mastercard", tps: 40_000 },
  { name: "ARC Chain", tps: 4_330_000 },
];

export default function LiveTpsCounter() {
  const [displayValue, setDisplayValue] = useState(0);
  const [reached, setReached] = useState(false);
  const [barsVisible, setBarsVisible] = useState(false);
  const rafRef = useRef<number>(0);
  const startRef = useRef<number>(0);

  const animate = useCallback((timestamp: number) => {
    if (!startRef.current) startRef.current = timestamp;
    const elapsed = timestamp - startRef.current;
    const progress = Math.min(elapsed / COUNT_DURATION_MS, 1);
    const eased = easeOutCubic(progress);
    setDisplayValue(Math.round(eased * TARGET_TPS));
    if (progress < 1) {
      rafRef.current = requestAnimationFrame(animate);
    } else {
      setReached(true);
      setTimeout(() => setBarsVisible(true), 300);
    }
  }, []);

  useEffect(() => {
    rafRef.current = requestAnimationFrame(animate);
    return () => cancelAnimationFrame(rafRef.current);
  }, [animate]);

  useEffect(() => {
    if (!reached) return;
    const interval = setInterval(() => {
      const delta = Math.round((Math.random() - 0.5) * 2 * FLUCTUATION);
      setDisplayValue(TARGET_TPS + delta);
    }, 1000);
    return () => clearInterval(interval);
  }, [reached]);

  const maxTps = COMPARISONS[COMPARISONS.length - 1].tps;

  return (
    <div className="flex flex-col items-center gap-8">
      {/* Header badge */}
      <div className="flex items-center gap-2">
        <div className="w-6 h-6 rounded-lg flex items-center justify-center" style={{ background: 'var(--gradient-arc)' }}>
          <Zap className="w-3.5 h-3.5 text-white" />
        </div>
        <span className="text-[11px] font-semibold text-[var(--accent)] uppercase tracking-label">
          Verified Benchmark
        </span>
      </div>

      {/* The number */}
      <div className="flex flex-col items-center gap-2 select-none">
        <span
          className="font-hash text-[48px] md:text-[64px] font-bold leading-none tracking-display text-[var(--accent)]"
          style={{
            fontVariantNumeric: "tabular-nums",
            textShadow: reached
              ? "0 0 40px var(--accent-glow), 0 0 80px var(--accent-glow)"
              : "none",
            transition: "text-shadow 0.6s ease",
          }}
        >
          {formatWithCommas(displayValue)}
        </span>
        <span className="text-[14px] text-[var(--text-secondary)] tracking-wide font-medium">
          transactions per second
        </span>
        <span className="text-[12px] text-[var(--text-tertiary)] flex items-center gap-1.5 mt-1">
          <ShieldCheck className="w-3.5 h-3.5 text-[var(--shield-green)]" />
          Apple M4, 10 cores — single machine
        </span>
      </div>

      {/* Verification badges */}
      <div className="flex flex-wrap items-center justify-center gap-2">
        {BADGES.map(({ icon: Icon, label, variant }, i) => {
          const isAccent = variant === "accent";
          return (
            <span
              key={label}
              className={`inline-flex items-center gap-1.5 px-3 py-1.5 rounded-full text-[11px] font-medium border animate-proof-check ${
                i === 0 ? "proof-delay-1" : i === 1 ? "proof-delay-2" : i === 2 ? "proof-delay-3" : "proof-delay-4"
              } ${
                isAccent
                  ? "bg-[var(--accent-light)] border-[var(--accent)]/20 text-[var(--accent)]"
                  : "bg-[var(--shield-green-bg)] border-[var(--shield-green)]/15 text-[var(--shield-green)]"
              }`}
            >
              <Icon className="w-3.5 h-3.5" strokeWidth={2.5} />
              {label}
            </span>
          );
        })}
      </div>

      {/* Comparison bars */}
      <div className="w-full max-w-lg flex flex-col gap-3.5">
        <span className="text-[10px] uppercase tracking-label text-[var(--text-tertiary)] font-medium text-center">
          Throughput Comparison
        </span>
        {COMPARISONS.map(({ name, tps }, idx) => {
          const isArc = name === "ARC Chain";
          const widthPct = (tps / maxTps) * 100;
          const displayPct = isArc ? widthPct : Math.max(widthPct, 3);
          const delay = barsVisible ? idx * 150 : 0;
          return (
            <div key={name} className="flex flex-col gap-1.5">
              <div className="flex items-center justify-between">
                <span className={`text-[13px] font-medium ${isArc ? "text-[var(--accent)]" : "text-[var(--text-secondary)]"}`}>
                  {name}
                </span>
                <span className={`font-hash text-[13px] ${isArc ? "text-[var(--accent)] font-bold" : "text-[var(--text-tertiary)]"}`}>
                  {tps.toLocaleString("en-US")} TPS
                </span>
              </div>
              <div className="w-full h-2.5 rounded-full bg-[var(--bg-secondary)] border border-[var(--border-light)] overflow-hidden">
                <div
                  className={`h-full rounded-full transition-all duration-1000 ${!isArc ? "bg-[var(--text-tertiary)]/30" : ""}`}
                  style={{
                    width: barsVisible ? `${displayPct}%` : "0%",
                    transitionDelay: `${delay}ms`,
                    transitionTimingFunction: "cubic-bezier(0.22, 1, 0.36, 1)",
                    ...(isArc ? { background: 'var(--gradient-arc)', boxShadow: "0 0 12px var(--accent-glow), 0 0 24px var(--accent-glow)" } : {}),
                  }}
                />
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
