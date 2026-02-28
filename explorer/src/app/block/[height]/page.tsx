"use client";

import { use, useState, useCallback } from "react";
import Link from "next/link";
import {
  Blocks,
  ArrowLeft,
  CheckCircle,
  XCircle,
  ArrowRight,
  Copy,
  Check,
  Box,
} from "lucide-react";
import { getBlock } from "@/lib/mock-data";
import {
  truncateHash,
  formatNumber,
  formatARC,
  timeAgo,
  txTypeColor,
  txTypeBg,
} from "@/lib/format";

function CopyableHash({
  value,
  chars = 10,
  className = "",
}: {
  value: string;
  chars?: number;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch { /* silent */ }
  }, [value]);

  return (
    <span className={`inline-flex items-center gap-1.5 min-w-0 ${className}`}>
      <span className="font-hash text-[12px] break-all leading-relaxed" title={value}>
        {truncateHash(value, chars)}
      </span>
      <button
        type="button"
        onClick={handleCopy}
        className="relative shrink-0 p-0.5 rounded hover:bg-[var(--border-light)] transition-colors cursor-pointer"
        aria-label={copied ? "Copied" : `Copy ${value}`}
      >
        {copied ? (
          <Check className="w-3.5 h-3.5 text-[var(--shield-green)]" />
        ) : (
          <Copy className="w-3.5 h-3.5 text-[var(--text-tertiary)] hover:text-[var(--text-secondary)]" />
        )}
        {copied && (
          <span className="absolute -top-7 left-1/2 -translate-x-1/2 whitespace-nowrap rounded bg-[var(--text)] text-[var(--bg)] text-[10px] px-1.5 py-0.5 pointer-events-none animate-slide-up">
            Copied!
          </span>
        )}
      </button>
    </span>
  );
}

export default function BlockDetailPage({
  params,
}: {
  params: Promise<{ height: string }>;
}) {
  const { height } = use(params);
  const block = getBlock(parseInt(height, 10));

  return (
    <div className="flex flex-col gap-6">
      {/* Back link */}
      <Link
        href="/blocks"
        className="inline-flex items-center gap-1.5 text-[12px] text-[var(--text-secondary)] hover:text-[var(--accent)] transition-colors w-fit"
      >
        <ArrowLeft className="w-3.5 h-3.5" />
        Back to Blocks
      </Link>

      {/* Block header */}
      <div className="card-surface p-6">
        <div className="flex items-center gap-3 mb-5">
          <div className="w-11 h-11 rounded-xl flex items-center justify-center" style={{ background: 'var(--gradient-arc)' }}>
            <Box className="w-5 h-5 text-white" />
          </div>
          <div>
            <h1 className="text-[20px] font-bold tracking-headline">
              Block #{formatNumber(block.height)}
            </h1>
            <p className="text-[12px] text-[var(--text-tertiary)]">
              {timeAgo(block.timestamp)}
            </p>
          </div>
        </div>

        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
          {[
            { label: "Block Hash", value: block.hash },
            { label: "Parent Hash", value: block.parent_hash },
            { label: "State Root", value: block.state_root },
            { label: "Tx Root", value: block.tx_root },
          ].map(({ label, value }) => (
            <div key={label} className="flex flex-col gap-1 min-w-0">
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium">
                {label}
              </span>
              <CopyableHash value={value} chars={10} className="text-[var(--text-secondary)]" />
            </div>
          ))}
          <div className="flex flex-col gap-1">
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium">
              Transactions
            </span>
            <span className="text-[15px] font-bold text-[var(--accent)]">
              {formatNumber(block.tx_count)}
            </span>
          </div>
          <div className="flex flex-col gap-1 min-w-0">
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium">
              Producer
            </span>
            <CopyableHash value={block.producer} chars={8} className="text-[var(--accent)]" />
          </div>
        </div>
      </div>

      {/* Transaction list */}
      <div className="card-surface overflow-hidden">
        <div className="px-5 py-4 border-b border-[var(--border)]">
          <span className="text-[14px] font-semibold tracking-tight">
            Transactions ({block.transactions.length} shown of{" "}
            {formatNumber(block.tx_count)})
          </span>
        </div>
        <div className="divide-y divide-[var(--border-light)]">
          {block.transactions.map((tx, i) => (
            <div
              key={tx.hash + i}
              className="flex items-center gap-3 px-5 py-3.5 hover:bg-[var(--bg-secondary)]/50 transition-all duration-200 animate-fade-slide-in"
              style={{ animationDelay: `${Math.min(i * 30, 300)}ms` }}
            >
              <div className="shrink-0">
                {tx.success ? (
                  <CheckCircle className="w-4 h-4 text-[var(--shield-green)]" />
                ) : (
                  <XCircle className="w-4 h-4 text-[var(--shield-red)]" />
                )}
              </div>
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <Link
                    href={`/tx/${tx.hash}`}
                    className="text-[12px] font-hash text-[var(--accent)] hover:underline"
                  >
                    {truncateHash(tx.hash, 6)}
                  </Link>
                  <span
                    className={`text-[10px] px-1.5 py-0.5 rounded-md font-medium ${txTypeColor(tx.tx_type)} ${txTypeBg(tx.tx_type)}`}
                  >
                    {tx.tx_type}
                  </span>
                </div>
                <div className="flex items-center gap-1 mt-0.5 text-[11px] text-[var(--text-secondary)]">
                  <span className="font-hash">{truncateHash(tx.from, 4)}</span>
                  <ArrowRight className="w-3 h-3 text-[var(--text-tertiary)]" />
                  <span className="font-hash">{truncateHash(tx.to, 4)}</span>
                </div>
              </div>
              <div className="text-right shrink-0">
                <div className="text-[13px] font-semibold">{formatARC(tx.amount)}</div>
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
