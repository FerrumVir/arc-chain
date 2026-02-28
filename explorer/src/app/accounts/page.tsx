"use client";

import { useState, useEffect } from "react";
import Link from "next/link";
import { Users, Code, Wallet } from "lucide-react";
import { truncateHash, formatARC, formatNumber } from "@/lib/format";

interface Account {
  address: string;
  balance: number;
  nonce: number;
  is_contract: boolean;
  tx_count: number;
}

function generateAccounts(count: number): Account[] {
  const accounts: Account[] = [];
  const chars = "0123456789abcdef";
  for (let i = 0; i < count; i++) {
    let addr = "0x";
    for (let j = 0; j < 40; j++) addr += chars[Math.floor(Math.random() * 16)];
    const isContract = Math.random() > 0.75;
    accounts.push({
      address: addr,
      balance: Math.floor(Math.random() * 10_000_000) / 100,
      nonce: Math.floor(Math.random() * 5000),
      is_contract: isContract,
      tx_count: Math.floor(Math.random() * 10000),
    });
  }
  return accounts.sort((a, b) => b.balance - a.balance);
}

export default function AccountsPage() {
  const [accounts, setAccounts] = useState<Account[]>([]);

  useEffect(() => {
    setAccounts(generateAccounts(50));
  }, []);

  return (
    <div className="flex flex-col gap-6">
      {/* Page header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="w-8 h-8 rounded-xl bg-[var(--accent-light)] flex items-center justify-center">
            <Users className="w-4 h-4 text-[var(--accent)]" />
          </div>
          <div>
            <h1 className="text-[20px] font-bold tracking-headline">Top Accounts</h1>
            <p className="text-[12px] text-[var(--text-tertiary)]">By balance (testnet)</p>
          </div>
        </div>
      </div>

      {/* Desktop table */}
      <div className="hidden md:block card-surface overflow-hidden">
        <div className="grid grid-cols-[40px_minmax(0,1fr)_140px_80px_80px] gap-3 px-5 py-3 border-b border-[var(--border)] text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium">
          <span>#</span>
          <span>Address</span>
          <span className="text-right">Balance</span>
          <span className="text-right">Nonce</span>
          <span className="text-right">Txns</span>
        </div>

        <div className="divide-y divide-[var(--border-light)]">
          {accounts.map((acc, i) => (
            <div
              key={acc.address}
              className="grid grid-cols-[40px_minmax(0,1fr)_140px_80px_80px] gap-3 px-5 py-3 items-center hover:bg-[var(--bg-secondary)]/50 transition-all duration-200 animate-fade-slide-in"
              style={{ animationDelay: `${Math.min(i * 20, 400)}ms` }}
            >
              <span className="text-[12px] text-[var(--text-tertiary)] font-medium">
                {i + 1}
              </span>
              <div className="flex items-center gap-2 min-w-0">
                {acc.is_contract ? (
                  <Code className="w-3.5 h-3.5 text-[var(--code-highlight-2)] shrink-0" />
                ) : (
                  <Wallet className="w-3.5 h-3.5 text-[var(--text-tertiary)] shrink-0" />
                )}
                <Link
                  href={`/account/${acc.address}`}
                  className="text-[12px] font-hash text-[var(--accent)] hover:underline truncate"
                >
                  {truncateHash(acc.address, 12)}
                </Link>
                {acc.is_contract && (
                  <span className="text-[10px] px-1.5 py-0.5 rounded-md bg-[var(--accent-light)] text-[var(--code-highlight-2)] font-medium shrink-0 whitespace-nowrap">
                    Contract
                  </span>
                )}
              </div>
              <span className="text-[12px] font-medium text-right whitespace-nowrap">
                {formatARC(acc.balance)}
              </span>
              <span className="text-[12px] text-[var(--text-secondary)] text-right">
                {formatNumber(acc.nonce)}
              </span>
              <span className="text-[12px] text-[var(--text-secondary)] text-right">
                {formatNumber(acc.tx_count)}
              </span>
            </div>
          ))}
        </div>
      </div>

      {/* Mobile card layout */}
      <div className="flex flex-col gap-2.5 md:hidden">
        {accounts.map((acc, i) => (
          <div
            key={acc.address}
            className="card-surface px-4 py-3.5 animate-fade-slide-in"
            style={{ animationDelay: `${Math.min(i * 20, 400)}ms` }}
          >
            <div className="flex items-center gap-2 min-w-0">
              <span className="text-[12px] text-[var(--text-tertiary)] tabular-nums w-5 shrink-0 font-medium">
                {i + 1}
              </span>
              {acc.is_contract ? (
                <Code className="w-3.5 h-3.5 text-[var(--code-highlight-2)] shrink-0" />
              ) : (
                <Wallet className="w-3.5 h-3.5 text-[var(--text-tertiary)] shrink-0" />
              )}
              <Link
                href={`/account/${acc.address}`}
                className="text-[13px] font-hash text-[var(--accent)] hover:underline truncate min-w-0"
              >
                {truncateHash(acc.address, 6)}
              </Link>
              {acc.is_contract && (
                <span className="text-[10px] ml-auto px-1.5 py-0.5 rounded-md bg-[var(--accent-light)] text-[var(--code-highlight-2)] font-medium shrink-0 whitespace-nowrap">
                  Contract
                </span>
              )}
            </div>

            <div className="mt-2 pl-7 flex items-center justify-between">
              <span className="text-[13px] font-semibold">
                {formatARC(acc.balance)}
              </span>
              <span className="text-[11px] text-[var(--text-tertiary)]">
                {formatNumber(acc.tx_count)} txns
              </span>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
