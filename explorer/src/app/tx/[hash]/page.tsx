"use client";

import { use } from "react";
import Link from "next/link";
import {
  ArrowLeft,
  CheckCircle,
  XCircle,
  ArrowRight,
  ArrowDown,
  Shield,
  Lock,
  Copy,
  ShieldCheck,
} from "lucide-react";
import { getRecentTransactions } from "@/lib/mock-data";
import {
  truncateHash,
  formatARC,
  formatNumber,
  timeAgo,
  txTypeColor,
  txTypeBg,
} from "@/lib/format";

export default function TxDetailPage({
  params,
}: {
  params: Promise<{ hash: string }>;
}) {
  const { hash } = use(params);
  const tx = getRecentTransactions(1)[0];
  const displayTx = { ...tx, hash };

  return (
    <div className="flex flex-col gap-6">
      <Link
        href="/"
        className="inline-flex items-center gap-1.5 text-[12px] text-[var(--text-secondary)] hover:text-[var(--accent)] transition-colors w-fit"
      >
        <ArrowLeft className="w-3.5 h-3.5" />
        Back to Explorer
      </Link>

      {/* Tx header */}
      <div className="card-surface p-6">
        <div className="flex items-center gap-3 mb-5">
          {displayTx.success ? (
            <div className="w-11 h-11 rounded-xl bg-[var(--shield-green-bg)] flex items-center justify-center">
              <CheckCircle className="w-5 h-5 text-[var(--shield-green)]" />
            </div>
          ) : (
            <div className="w-11 h-11 rounded-xl bg-[var(--shield-red-bg)] flex items-center justify-center">
              <XCircle className="w-5 h-5 text-[var(--shield-red)]" />
            </div>
          )}
          <div>
            <h1 className="text-[18px] font-bold tracking-headline">
              Transaction {displayTx.success ? "Success" : "Failed"}
            </h1>
            <p className="text-[12px] text-[var(--text-tertiary)]">
              {timeAgo(displayTx.timestamp)}
            </p>
          </div>
          <span
            className={`ml-auto text-[11px] px-2.5 py-1 rounded-lg font-medium ${txTypeColor(displayTx.tx_type)} ${txTypeBg(displayTx.tx_type)}`}
          >
            {displayTx.tx_type}
          </span>
        </div>

        <div className="flex flex-col gap-5">
          {/* Hash */}
          <div>
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1.5">
              Transaction Hash
            </span>
            <div className="flex items-center gap-2">
              <span className="text-[12px] font-hash text-[var(--text-secondary)] break-all">
                {displayTx.hash}
              </span>
              <button
                className="shrink-0 p-1 rounded hover:bg-[var(--border-light)] transition-colors"
                onClick={() => navigator.clipboard?.writeText(displayTx.hash)}
              >
                <Copy className="w-3 h-3 text-[var(--text-tertiary)]" />
              </button>
            </div>
          </div>

          {/* From → To */}
          <div className="flex flex-col sm:flex-row items-start sm:items-center gap-4">
            <div className="flex-1 min-w-0">
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1.5">
                From
              </span>
              <div className="flex items-center gap-2">
                <Link
                  href={`/account/${displayTx.from}`}
                  className="text-[12px] font-hash text-[var(--accent)] hover:underline break-all"
                >
                  {displayTx.from}
                </Link>
                <button
                  className="shrink-0 p-1 rounded hover:bg-[var(--border-light)] transition-colors"
                  onClick={() => navigator.clipboard?.writeText(displayTx.from)}
                >
                  <Copy className="w-3 h-3 text-[var(--text-tertiary)]" />
                </button>
              </div>
            </div>
            <ArrowDown className="w-4 h-4 text-[var(--text-tertiary)] shrink-0 block sm:hidden" />
            <ArrowRight className="w-4 h-4 text-[var(--text-tertiary)] shrink-0 hidden sm:block" />
            <div className="flex-1 min-w-0">
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1.5">
                To
              </span>
              <div className="flex items-center gap-2">
                <Link
                  href={`/account/${displayTx.to}`}
                  className="text-[12px] font-hash text-[var(--accent)] hover:underline break-all"
                >
                  {displayTx.to}
                </Link>
                <button
                  className="shrink-0 p-1 rounded hover:bg-[var(--border-light)] transition-colors"
                  onClick={() => navigator.clipboard?.writeText(displayTx.to)}
                >
                  <Copy className="w-3 h-3 text-[var(--text-tertiary)]" />
                </button>
              </div>
            </div>
          </div>

          {/* Details grid */}
          <div className="grid grid-cols-2 md:grid-cols-4 gap-5 pt-4 border-t border-[var(--border-light)]">
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
                Amount
              </span>
              <span className="text-[18px] font-bold text-[var(--accent)]">
                {formatARC(displayTx.amount)}
              </span>
            </div>
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
                Gas Used
              </span>
              <span className="text-[16px] font-bold">
                {formatNumber(displayTx.gas_used)}
              </span>
            </div>
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
                Nonce
              </span>
              <span className="text-[16px] font-bold">{displayTx.nonce}</span>
            </div>
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium block mb-1">
                Block
              </span>
              <Link
                href={`/block/${displayTx.block_height}`}
                className="text-[16px] font-bold text-[var(--accent)] hover:underline"
              >
                #{formatNumber(displayTx.block_height)}
              </Link>
            </div>
          </div>
        </div>
      </div>

      {/* Cryptographic proofs */}
      <div className="card-surface p-6">
        <div className="flex items-center justify-between mb-5">
          <div className="flex items-center gap-2.5">
            <div className="w-7 h-7 rounded-lg bg-[var(--shield-green-bg)] flex items-center justify-center">
              <Shield className="w-3.5 h-3.5 text-[var(--shield-green)]" />
            </div>
            <span className="text-[14px] font-semibold tracking-tight">Cryptographic Proofs</span>
          </div>
          <div className="relative group">
            <button
              disabled
              className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-lg border border-[var(--border)] text-[11px] text-[var(--text-tertiary)] font-medium cursor-not-allowed opacity-60"
            >
              <ShieldCheck className="w-3.5 h-3.5" />
              Verify On-Chain
            </button>
            <div className="absolute bottom-full right-0 mb-2 px-2.5 py-1.5 rounded-lg bg-[var(--text)] text-[var(--bg)] text-[10px] whitespace-nowrap opacity-0 pointer-events-none group-hover:opacity-100 transition-opacity shadow-lg">
              Coming soon
            </div>
          </div>
        </div>

        <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
          {[
            { icon: CheckCircle, title: "BLAKE3 Commitment", desc: "Domain-separated cryptographic hash verified", delay: "proof-delay-1" },
            { icon: CheckCircle, title: "Merkle Inclusion", desc: "Transaction included in block Merkle tree", delay: "proof-delay-2" },
            { icon: Lock, title: "Pedersen Privacy", desc: "Amount shielded with homomorphic commitment", delay: "proof-delay-3" },
          ].map(({ icon: Icon, title, desc, delay }) => (
            <div key={title} className="flex items-center gap-3 p-4 rounded-xl bg-[var(--shield-green-bg)] border border-[var(--shield-green)]/10">
              <Icon className={`w-4 h-4 text-[var(--shield-green)] shrink-0 animate-proof-check ${delay}`} />
              <div>
                <div className="text-[12px] font-semibold text-[var(--shield-green)]">{title}</div>
                <div className="text-[10px] text-[var(--text-secondary)]">{desc}</div>
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
