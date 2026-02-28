"use client";

import Link from "next/link";
import { ArrowRight, CheckCircle, XCircle, Activity } from "lucide-react";
import {
  truncateHash,
  formatARC,
  timeAgo,
  txTypeColor,
  txTypeBg,
} from "@/lib/format";
import type { Transaction } from "@/lib/types";

export default function TxList({
  transactions,
  compact = false,
}: {
  transactions: Transaction[];
  compact?: boolean;
}) {
  const displayTxs = compact ? transactions.slice(0, 8) : transactions;

  return (
    <div className="card-flat overflow-hidden">
      <div className="flex items-center justify-between px-4 py-3 border-b border-[var(--border)]">
        <div className="flex items-center gap-2">
          <Activity className="w-4 h-4 text-[var(--accent)]" />
          <span className="text-[13px] font-medium">Latest Transactions</span>
        </div>
        {compact && (
          <span className="text-[12px] text-[var(--text-tertiary)]">
            Latest {displayTxs.length} transactions
          </span>
        )}
      </div>

      <div className="divide-y divide-[var(--border-light)]">
        {displayTxs.map((tx, i) => (
          <div
            key={tx.hash + i}
            className="animate-block-pop flex items-center gap-3 px-4 py-3 hover:bg-[var(--bg)] transition-colors"
            style={{ animationDelay: `${i * 30}ms` }}
          >
            {/* Status icon */}
            <div className="shrink-0">
              {tx.success ? (
                <CheckCircle className="w-4 h-4 text-[var(--shield-green)]" />
              ) : (
                <XCircle className="w-4 h-4 text-[var(--shield-red)]" />
              )}
            </div>

            {/* Tx hash + type */}
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

            {/* Amount + time */}
            <div className="text-right shrink-0">
              <div className="text-[12px] font-medium">{formatARC(tx.amount)}</div>
              <div className="text-[10px] text-[var(--text-tertiary)]">
                {timeAgo(tx.timestamp)}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
