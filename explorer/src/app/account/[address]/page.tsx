"use client";

import { use } from "react";
import Link from "next/link";
import {
  ArrowLeft,
  Wallet,
  Code,
  CheckCircle,
  XCircle,
  ArrowRight,
  Copy,
} from "lucide-react";
import { getAccount, getRecentTransactions } from "@/lib/mock-data";
import {
  truncateHash,
  formatARC,
  formatNumber,
  timeAgo,
  txTypeColor,
  txTypeBg,
} from "@/lib/format";

export default function AccountDetailPage({
  params,
}: {
  params: Promise<{ address: string }>;
}) {
  const { address } = use(params);
  const account = getAccount(address);
  const txs = getRecentTransactions(15);

  return (
    <div className="flex flex-col gap-6">
      <Link
        href="/accounts"
        className="inline-flex items-center gap-1.5 text-[12px] text-[var(--text-secondary)] hover:text-[var(--accent)] transition-colors w-fit"
      >
        <ArrowLeft className="w-3.5 h-3.5" />
        Back to Accounts
      </Link>

      {/* Account header */}
      <div className="card-surface p-6">
        <div className="flex items-center gap-3 mb-5">
          <div className="w-11 h-11 rounded-xl bg-[var(--accent-light)] flex items-center justify-center">
            {account.is_contract ? (
              <Code className="w-5 h-5 text-[var(--accent)]" />
            ) : (
              <Wallet className="w-5 h-5 text-[var(--accent)]" />
            )}
          </div>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <h1 className="text-[18px] font-bold tracking-headline">
                {account.is_contract ? "Contract" : "Account"}
              </h1>
              {account.is_contract && (
                <span className="text-[10px] px-2 py-0.5 rounded-md bg-[var(--accent-light)] text-[var(--code-highlight-2)] font-medium">
                  WASM Contract
                </span>
              )}
            </div>
            <div className="flex items-center gap-2 mt-1">
              <span className="text-[12px] font-hash text-[var(--text-secondary)] break-all">
                {address}
              </span>
              <button
                className="shrink-0 p-1 rounded hover:bg-[var(--border-light)] transition-colors"
                onClick={() => navigator.clipboard?.writeText(address)}
              >
                <Copy className="w-3 h-3 text-[var(--text-tertiary)]" />
              </button>
            </div>
          </div>
        </div>

        <div className="grid grid-cols-2 md:grid-cols-4 gap-5">
          <div>
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
              Balance
            </span>
            <span className="text-[18px] font-bold text-[var(--accent)]">
              {formatARC(account.balance)}
            </span>
          </div>
          <div>
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
              Nonce
            </span>
            <span className="text-[18px] font-bold">
              {formatNumber(account.nonce)}
            </span>
          </div>
          <div>
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
              Transactions
            </span>
            <span className="text-[18px] font-bold">
              {formatNumber(account.tx_count)}
            </span>
          </div>
          {account.code_hash && (
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
                Code Hash
              </span>
              <span className="text-[12px] font-hash text-[var(--text-secondary)]">
                {truncateHash(account.code_hash, 8)}
              </span>
            </div>
          )}
        </div>
      </div>

      {/* Recent transactions */}
      <div className="card-surface overflow-hidden">
        <div className="px-5 py-4 border-b border-[var(--border)]">
          <span className="text-[14px] font-semibold tracking-tight">Recent Transactions</span>
        </div>
        <div className="divide-y divide-[var(--border-light)]">
          {txs.map((tx, i) => (
            <div
              key={tx.hash + i}
              className="flex items-center gap-3 px-5 py-3.5 hover:bg-[var(--bg-secondary)]/50 transition-all duration-200 animate-fade-slide-in"
              style={{ animationDelay: `${Math.min(i * 30, 300)}ms` }}
            >
              {tx.success ? (
                <CheckCircle className="w-4 h-4 text-[var(--shield-green)] shrink-0" />
              ) : (
                <XCircle className="w-4 h-4 text-[var(--shield-red)] shrink-0" />
              )}
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
                <div className="text-[10px] text-[var(--text-tertiary)]">
                  {timeAgo(tx.timestamp)}
                </div>
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
