"use client";

import Link from "next/link";
import { Blocks, Box } from "lucide-react";
import { truncateHash, formatNumber, timeAgo } from "@/lib/format";
import type { Block } from "@/lib/types";

export default function BlockList({
  blocks,
  compact = false,
}: {
  blocks: Block[];
  compact?: boolean;
}) {
  const displayBlocks = compact ? blocks.slice(0, 8) : blocks;

  return (
    <div className="card-surface overflow-hidden">
      <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border)]">
        <div className="flex items-center gap-2.5">
          <div className="w-7 h-7 rounded-lg flex items-center justify-center" style={{ background: 'var(--gradient-arc)' }}>
            <Blocks className="w-3.5 h-3.5 text-white" />
          </div>
          <span className="text-[15px] font-semibold tracking-tight">Latest Blocks</span>
        </div>
        {compact && (
          <Link
            href="/blocks"
            className="text-[12px] font-medium text-[var(--accent)] hover:underline"
          >
            View all
          </Link>
        )}
      </div>

      <div className="divide-y divide-[var(--border-light)]">
        {displayBlocks.map((block, i) => (
          <div
            key={block.height}
            className="animate-fade-slide-in flex items-center gap-4 px-5 py-3.5 hover:bg-[var(--bg-secondary)]/50 transition-all duration-200"
            style={{ animationDelay: `${i * 40}ms` }}
          >
            <div className="w-10 h-10 rounded-xl bg-[var(--accent-light)] flex items-center justify-center shrink-0">
              <Box className="w-4 h-4 text-[var(--accent)]" />
            </div>

            <div className="flex-1 min-w-0">
              <div className="flex items-center gap-2">
                <Link
                  href={`/block/${block.height}`}
                  className="text-[13px] font-semibold text-[var(--accent)] hover:underline"
                >
                  #{formatNumber(block.height)}
                </Link>
                <span className="text-[11px] text-[var(--text-tertiary)]">
                  {timeAgo(block.timestamp)}
                </span>
              </div>
              <div className="text-[11px] text-[var(--text-tertiary)] font-hash truncate mt-0.5">
                {truncateHash(block.hash, 8)}
              </div>
            </div>

            <div className="text-right shrink-0">
              <div className="text-[13px] font-semibold">
                {formatNumber(block.tx_count)}
              </div>
              <div className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label">txns</div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
