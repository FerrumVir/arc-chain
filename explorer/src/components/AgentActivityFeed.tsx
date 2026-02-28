"use client";

import { useEffect, useState, useRef, useCallback } from "react";
import { CheckCircle, ArrowRight, Activity } from "lucide-react";
import { txTypeColor, txTypeBg } from "@/lib/format";
import type { TxType } from "@/lib/types";

/* ------------------------------------------------------------------ */
/*  Transaction template pool                                          */
/* ------------------------------------------------------------------ */

interface AgentTxTemplate {
  from: string;
  to: string;
  description: string;
  amount: number;
  txType: TxType;
  byteSize: number;
}

const TX_TEMPLATES: AgentTxTemplate[] = [
  { from: "PaymentBot-7", to: "MerchantGateway-12", description: "Payment processed", amount: 2_450, txType: "Transfer", byteSize: 768 },
  { from: "PayrollAgent-1", to: "EmployeeWallet-304", description: "Salary disbursed", amount: 6_200, txType: "Transfer", byteSize: 512 },
  { from: "RefundBot-3", to: "CustomerWallet-91", description: "Refund issued", amount: 185, txType: "Transfer", byteSize: 384 },
  { from: "DonationRouter-5", to: "CharityPool-Global", description: "Donation routed", amount: 50, txType: "Transfer", byteSize: 320 },
  { from: "SubscriptionBot-2", to: "SaaSVault-Pro", description: "Monthly subscription", amount: 99, txType: "Transfer", byteSize: 448 },
  { from: "SettlementEngine-4", to: "ClearingHouse-Alpha", description: "Batch settled", amount: 47_800, txType: "Settle", byteSize: 1_024 },
  { from: "InvoiceBot-11", to: "SupplierWallet-66", description: "Invoice settled", amount: 12_750, txType: "Settle", byteSize: 896 },
  { from: "SettlementEngine-4", to: "MerchantGateway-12", description: "End-of-day settled", amount: 234_100, txType: "Settle", byteSize: 1_280 },
  { from: "InsuranceClaims-8", to: "PolicyHolder-229", description: "Claim settled", amount: 18_400, txType: "Settle", byteSize: 960 },
  { from: "RoyaltyBot-6", to: "CreatorVault-42", description: "Royalties settled", amount: 3_120, txType: "Settle", byteSize: 640 },
  { from: "TradingEngine-Alpha", to: "LiquidityPool-Main", description: "Swap executed", amount: 50_000, txType: "Swap", byteSize: 1_536 },
  { from: "ArbitrageBot-9", to: "DEXPool-East", description: "Arb swap filled", amount: 127_400, txType: "Swap", byteSize: 1_792 },
  { from: "RebalanceAgent-3", to: "IndexFund-ARC20", description: "Portfolio rebalanced", amount: 85_600, txType: "Swap", byteSize: 2_048 },
  { from: "YieldOptimizer-7", to: "LiquidityPool-Main", description: "LP position rotated", amount: 41_200, txType: "Swap", byteSize: 1_408 },
  { from: "MarketMaker-Beta", to: "OrderBook-Central", description: "Limit order filled", amount: 9_750, txType: "Swap", byteSize: 1_152 },
  { from: "EscrowAgent-9", to: "FreelancerWallet-88", description: "Milestone released", amount: 8_500, txType: "Escrow", byteSize: 1_024 },
  { from: "EscrowAgent-14", to: "ContractorWallet-55", description: "Phase 2 released", amount: 22_000, txType: "Escrow", byteSize: 1_280 },
  { from: "BuyerAgent-21", to: "EscrowVault-7", description: "Funds locked", amount: 15_300, txType: "Escrow", byteSize: 896 },
  { from: "DisputeBot-4", to: "EscrowVault-7", description: "Dispute resolved", amount: 4_800, txType: "Escrow", byteSize: 1_536 },
  { from: "ValidatorNode-East", to: "StakingContract", description: "Staked", amount: 100_000, txType: "Stake", byteSize: 640 },
  { from: "DelegatorBot-19", to: "StakingContract", description: "Delegation added", amount: 250_000, txType: "Stake", byteSize: 576 },
  { from: "StakingContract", to: "ValidatorNode-West", description: "Rewards distributed", amount: 3_840, txType: "Stake", byteSize: 512 },
  { from: "UnstakeBot-2", to: "CooldownVault", description: "Unstake initiated", amount: 75_000, txType: "Stake", byteSize: 448 },
  { from: "DataOracle-West", to: "InsurancePool-3", description: "Price feed delivered", amount: 12, txType: "WasmCall", byteSize: 2_304 },
  { from: "AnalyticsBot-6", to: "DashboardContract", description: "Metrics pushed", amount: 4, txType: "WasmCall", byteSize: 3_072 },
  { from: "NFTMinter-11", to: "CollectionContract-8", description: "Batch minted", amount: 28, txType: "WasmCall", byteSize: 4_096 },
  { from: "BridgeRelay-3", to: "BridgeContract-ETH", description: "Cross-chain relay", amount: 1, txType: "WasmCall", byteSize: 2_816 },
  { from: "GovernanceBot", to: "Treasury-MultiSig", description: "Vote cast on Proposal #47", amount: 0, txType: "MultiSig", byteSize: 1_792 },
  { from: "Treasury-MultiSig", to: "GrantRecipient-12", description: "Grant approved", amount: 450_000, txType: "MultiSig", byteSize: 2_560 },
  { from: "SecurityBot-1", to: "Treasury-MultiSig", description: "Emergency pause signed", amount: 0, txType: "MultiSig", byteSize: 1_408 },
  { from: "UpgradeBot-2", to: "ProxyContract-Core", description: "Upgrade ratified", amount: 0, txType: "MultiSig", byteSize: 3_328 },
];

interface LiveEntry {
  id: string;
  template: AgentTxTemplate;
  timestamp: number;
}

function pickRandom(): AgentTxTemplate {
  const base = TX_TEMPLATES[Math.floor(Math.random() * TX_TEMPLATES.length)];
  const jitter = base.amount > 0
    ? Math.round(base.amount * (0.85 + Math.random() * 0.3))
    : 0;
  const bytes = Math.round(base.byteSize * (0.9 + Math.random() * 0.2));
  return { ...base, amount: jitter, byteSize: bytes };
}

const MAX_VISIBLE = 8;

export default function AgentActivityFeed() {
  const [entries, setEntries] = useState<LiveEntry[]>([]);
  const [totalTx, setTotalTx] = useState(847_291_038);
  const counterRef = useRef(totalTx);

  const seeded = useRef(false);
  useEffect(() => {
    if (seeded.current) return;
    seeded.current = true;
    const initial: LiveEntry[] = Array.from({ length: MAX_VISIBLE }, (_, i) => ({
      id: `seed-${i}-${Date.now()}`,
      template: pickRandom(),
      timestamp: Date.now() - (MAX_VISIBLE - i) * 400,
    }));
    setEntries(initial);
  }, []);

  const addEntry = useCallback(() => {
    const entry: LiveEntry = {
      id: `tx-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      template: pickRandom(),
      timestamp: Date.now(),
    };
    setEntries((prev) => [entry, ...prev].slice(0, MAX_VISIBLE));
    counterRef.current += 1;
    setTotalTx(counterRef.current);
  }, []);

  useEffect(() => {
    const interval = setInterval(addEntry, 400);
    return () => clearInterval(interval);
  }, [addEntry]);

  return (
    <div className="card-surface overflow-hidden">
      {/* Header */}
      <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border)]">
        <h2 className="text-[15px] font-semibold flex items-center gap-2.5 tracking-tight">
          <div className="relative">
            <div className="w-7 h-7 rounded-lg bg-[var(--shield-green-bg)] flex items-center justify-center">
              <Activity className="w-3.5 h-3.5 text-[var(--shield-green)]" />
            </div>
            {/* Pulsing live dot */}
            <span className="absolute -top-0.5 -right-0.5 flex h-2.5 w-2.5">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-[var(--shield-green)] opacity-60" />
              <span className="relative inline-flex h-2.5 w-2.5 rounded-full bg-[var(--shield-green)] border-2 border-[var(--bg-elevated)]" />
            </span>
          </div>
          Live Agent Activity
        </h2>
        <span className="text-[11px] font-hash text-[var(--text-tertiary)]">
          {totalTx.toLocaleString()} txns
        </span>
      </div>

      {/* Entry list */}
      <div className="divide-y divide-[var(--border-light)]">
        {entries.map((entry) => {
          const t = entry.template;
          return (
            <div
              key={entry.id}
              className="animate-slide-up flex items-center gap-3 px-5 py-2.5 hover:bg-[var(--bg-secondary)]/50 transition-colors"
            >
              <CheckCircle className="w-3.5 h-3.5 text-[var(--shield-green)] shrink-0" />

              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-1.5 flex-wrap">
                  <span className="text-[12px] font-semibold truncate">{t.from}</span>
                  <ArrowRight className="w-3 h-3 text-[var(--text-tertiary)] shrink-0" />
                  <span className="text-[12px] font-semibold truncate">{t.to}</span>
                </div>
                <div className="flex items-center gap-2 mt-0.5">
                  <span
                    className={`text-[9px] px-1.5 py-0.5 rounded-md font-semibold uppercase tracking-wider ${txTypeColor(t.txType)} ${txTypeBg(t.txType)}`}
                  >
                    {t.txType}
                  </span>
                  <span className="text-[10px] text-[var(--text-tertiary)] truncate">
                    {t.description}
                  </span>
                </div>
              </div>

              <div className="flex items-center gap-2 shrink-0">
                <span className="text-[12px] font-medium whitespace-nowrap">
                  {t.amount > 0 ? t.amount.toLocaleString() : "—"} <span className="text-[var(--text-tertiary)]">ARC</span>
                </span>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
