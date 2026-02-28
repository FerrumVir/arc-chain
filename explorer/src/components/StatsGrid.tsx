"use client";

import { Activity, Blocks, Users, Zap, Shield, Clock } from "lucide-react";
import { formatNumber, formatTPS } from "@/lib/format";
import type { NetworkStats } from "@/lib/types";

interface StatCardProps {
  label: string;
  value: string;
  sub?: string;
  icon: React.ElementType;
  accent?: boolean;
  delay?: number;
}

function StatCard({ label, value, sub, icon: Icon, accent, delay = 0 }: StatCardProps) {
  return (
    <div
      className={`animate-slide-up card-surface p-4 flex flex-col gap-2 hover:border-[var(--accent)]/30 ${accent ? "border-l-2 border-l-[var(--accent)]" : ""}`}
      style={{ animationDelay: `${delay}ms` }}
    >
      <div className="flex items-center justify-between">
        <span className="text-[11px] uppercase tracking-label text-[var(--text-tertiary)] font-medium">
          {label}
        </span>
        <Icon
          className={`w-4 h-4 ${accent ? "text-[var(--accent)]" : "text-[var(--text-tertiary)]"}`}
        />
      </div>
      <div className="flex items-baseline gap-1.5">
        <span
          className={`text-[22px] font-semibold tracking-tight ${
            accent ? "text-[var(--accent)]" : ""
          }`}
        >
          {value}
        </span>
        {sub && (
          <span className="text-[11px] text-[var(--text-tertiary)]">{sub}</span>
        )}
      </div>
    </div>
  );
}

export default function StatsGrid({ stats }: { stats: NetworkStats }) {
  const uptime = Math.floor(stats.uptime_seconds / 86400);

  return (
    <div className="grid grid-cols-2 md:grid-cols-3 lg:grid-cols-6 gap-3">
      <StatCard
        label="Current TPS"
        value={formatTPS(stats.tps_current)}
        sub="tx/sec"
        icon={Zap}
        accent
        delay={0}
      />
      <StatCard
        label="Peak TPS"
        value={formatTPS(stats.tps_peak)}
        sub="tx/sec"
        icon={Activity}
        delay={50}
      />
      <StatCard
        label="Block Height"
        value={formatNumber(stats.chain_height)}
        icon={Blocks}
        delay={100}
      />
      <StatCard
        label="Total Txns"
        value={formatNumber(stats.total_transactions)}
        icon={Shield}
        delay={150}
      />
      <StatCard
        label="Accounts"
        value={formatNumber(stats.total_accounts)}
        icon={Users}
        delay={200}
      />
      <StatCard
        label="Uptime"
        value={`${uptime}d`}
        sub={`${stats.node_count} node${stats.node_count > 1 ? "s" : ""}`}
        icon={Clock}
        delay={250}
      />
    </div>
  );
}
