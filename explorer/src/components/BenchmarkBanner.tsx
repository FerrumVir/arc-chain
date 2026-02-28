"use client";

import { Zap, ArrowRight, Shield } from "lucide-react";

const COMPARISONS = [
  { chain: "Ethereum", tps: "30", color: "text-[var(--text-secondary)]" },
  { chain: "Solana", tps: "65K", color: "text-[var(--text-secondary)]" },
  { chain: "Sui", tps: "120K", color: "text-[var(--text-secondary)]" },
  { chain: "ARC Chain", tps: "4.3M", color: "text-[var(--accent)]", bold: true },
];

export default function BenchmarkBanner() {
  return (
    <div className="relative overflow-hidden rounded-2xl border border-[var(--accent)]/20 bg-gradient-to-br from-[var(--accent-light)] to-[var(--bg-secondary)]">
      {/* Content */}
      <div className="relative z-10 p-6 md:p-8">
        <div className="flex flex-col md:flex-row md:items-center gap-6">
          {/* Left: message */}
          <div className="flex-1">
            <div className="flex items-center gap-2 mb-2">
              <div className="w-6 h-6 rounded-md bg-[var(--accent)] flex items-center justify-center">
                <Zap className="w-3.5 h-3.5 text-white" />
              </div>
              <span className="text-[11px] uppercase tracking-label text-[var(--accent)] font-semibold">
                Verified Benchmark
              </span>
            </div>
            <h2 className="text-[20px] md:text-[24px] font-bold tracking-tight leading-tight">
              <span className="animate-pulse-text inline-block">4.3 Million TPS</span>
            </h2>
            <p className="text-[13px] text-[var(--text-secondary)] mt-1 max-w-md">
              Full pipeline: BLAKE3 commit + parallel state sharding + Merkle
              tree + ZK aggregate proof. Every transaction cryptographically
              verifiable.
            </p>
            <div className="flex items-center gap-1.5 mt-3 text-[12px] text-[var(--shield-green)] font-medium">
              <Shield className="w-3.5 h-3.5" />
              <span>Apple M4, 10 cores — single machine</span>
            </div>
          </div>

          {/* Right: comparison */}
          <div className="flex flex-col gap-2 min-w-[200px]">
            {COMPARISONS.map(({ chain, tps, color, bold }) => (
              <div
                key={chain}
                className={`flex items-center justify-between gap-4 px-3 rounded-lg transition-all ${
                  bold
                    ? "py-3 bg-[var(--accent)]/10 border border-[var(--accent)]/30 shadow-[0_0_15px_rgba(197,113,75,0.3)] animate-pulse-glow"
                    : "py-2 bg-[var(--bg)]/50"
                }`}
              >
                <span
                  className={`${bold ? "text-[13px] font-bold text-[var(--accent)]" : "text-[12px] text-[var(--text-secondary)]"}`}
                >
                  {chain}
                </span>
                <div className="flex items-center gap-1">
                  <span
                    className={`font-semibold font-hash ${color} ${bold ? "text-[15px]" : "text-[13px]"}`}
                  >
                    {tps}
                  </span>
                  <span className={`text-[var(--text-tertiary)] ${bold ? "text-[11px]" : "text-[10px]"}`}>
                    TPS
                  </span>
                </div>
              </div>
            ))}
          </div>
        </div>

        {/* CTA */}
        <div className="mt-6 flex items-center gap-3">
          <button
            onClick={(e) => e.preventDefault()}
            className="inline-flex items-center gap-1.5 px-4 py-2 rounded-lg bg-[var(--accent)] text-white text-[12px] font-medium hover:opacity-90 transition-opacity cursor-pointer"
          >
            View Full Benchmark
            <ArrowRight className="w-3.5 h-3.5" />
          </button>
          <a
            href="/join"
            className="inline-flex items-center gap-1.5 px-4 py-2 rounded-lg border border-[var(--border)] text-[12px] text-[var(--text-secondary)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-colors"
          >
            Run a Node
          </a>
        </div>
      </div>
    </div>
  );
}
