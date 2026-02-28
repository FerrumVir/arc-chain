"use client";

import { useState, useEffect, useCallback, useMemo } from "react";
import Link from "next/link";
import {
  Blocks,
  ArrowRightLeft,
  Clock,
  Activity,
  Box,
  ChevronRight,
  Copy,
  Hash,
  Eye,
  Zap,
  Shield,
  ShieldCheck,
  Users,
} from "lucide-react";
import VerifyButton from "@/components/VerifyButton";
import LiveTpsCounter from "@/components/LiveTpsCounter";
import AgentActivityFeed from "@/components/AgentActivityFeed";
import { verifyBlake3Hash, computeBlake3Hash } from "@/lib/verify";
import {
  checkDataSource,
  getStats,
  getRecentBlocks,
  type DataSource,
  type Stats,
  type Block,
} from "@/lib/chain-client";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fmt = new Intl.NumberFormat("en-US");

function shortHash(hash: string, chars = 8): string {
  if (hash.length <= chars * 2 + 2) return hash;
  return `${hash.slice(0, chars + 2)}...${hash.slice(-chars)}`;
}

function timeAgo(ts: number): string {
  const diff = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (diff < 5) return "just now";
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

function fakeHex(bytes: number): string {
  const chars = "0123456789abcdef";
  let out = "";
  for (let i = 0; i < bytes * 2; i++) out += chars[Math.floor(Math.random() * 16)];
  return out;
}

// ---------------------------------------------------------------------------
// Mock transaction generator (demo mode)
// ---------------------------------------------------------------------------

interface RecentTx {
  hash: string;
  from: string;
  to: string;
  amount: string;
  txType: string;
  blockHeight: number;
  timestamp: number;
  preHashHex: string;
}

const TX_TYPES = ["Transfer", "Settle", "Swap", "Escrow", "Stake", "WasmCall"];

function generateMockTxs(count: number, baseBlock: number): RecentTx[] {
  const txs: RecentTx[] = [];
  for (let i = 0; i < count; i++) {
    const txType = TX_TYPES[Math.floor(Math.random() * TX_TYPES.length)];
    txs.push({
      hash: "0x" + fakeHex(32),
      from: "0x" + fakeHex(20),
      to: "0x" + fakeHex(20),
      amount: String(Math.floor(50 + Math.random() * 9950)),
      txType,
      blockHeight: baseBlock - Math.floor(Math.random() * 5),
      timestamp: Date.now() - i * 400 - Math.floor(Math.random() * 2000),
      preHashHex:
        "01" + fakeHex(32) + fakeHex(8) + fakeHex(32) + fakeHex(8),
    });
  }
  return txs;
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export default function Home() {
  const [dataSource, setDataSource] = useState<DataSource>("demo");
  const [stats, setStats] = useState<Stats | null>(null);
  const [blocks, setBlocks] = useState<Block[]>([]);
  const [txs, setTxs] = useState<RecentTx[]>([]);
  const [loading, setLoading] = useState(true);

  // Initial data fetch
  useEffect(() => {
    let cancelled = false;
    async function init() {
      const source = await checkDataSource();
      if (cancelled) return;
      setDataSource(source);

      const [statsRes, blocksRes] = await Promise.all([
        getStats(),
        getRecentBlocks(10),
      ]);

      if (cancelled) return;
      setStats(statsRes.data);
      setBlocks(Array.isArray(blocksRes.data) ? blocksRes.data : []);

      // Always generate demo txs for the transaction table — the live Rust
      // node doesn't stream individual txs yet.  Real blocks come through
      // `getRecentBlocks` regardless of mode.
      setTxs(generateMockTxs(10, statsRes.data.blockHeight || 148_293));
      setLoading(false);
    }
    init();
    return () => { cancelled = true; };
  }, []);

  // Poll stats every 3s when live
  useEffect(() => {
    if (dataSource !== "live") return;
    let cancelled = false;
    const poll = async () => {
      try {
        const { data, source } = await getStats();
        if (!cancelled && source === "live") setStats(data);
      } catch { /* keep last */ }
    };
    const id = setInterval(poll, 3000);
    return () => { cancelled = true; clearInterval(id); };
  }, [dataSource]);

  return (
    <div className="flex flex-col gap-8 pb-8">
      {/* ================================================================ */}
      {/* HERO SECTION — Jony Ive inspired, generous whitespace            */}
      {/* ================================================================ */}
      <section className="relative py-10 md:py-16 overflow-hidden">
        {/* Ambient glow decorations */}
        <div className="ambient-glow w-[400px] h-[400px] -top-48 -left-48 bg-[var(--accent)]" />
        <div className="ambient-glow w-[300px] h-[300px] -top-32 right-0 bg-[var(--accent-soft)]" />

        <div className="relative z-10 text-center max-w-[720px] mx-auto">
          {/* Tag */}
          <div className="inline-flex items-center gap-2 px-3.5 py-1.5 rounded-full bg-[var(--bg-secondary)] border border-[var(--border)] mb-6 animate-fade-slide-in">
            <div className="w-1.5 h-1.5 rounded-full bg-[var(--shield-green)] animate-shield-pulse" />
            <span className="text-[11px] font-medium text-[var(--text-secondary)] tracking-label uppercase">
              Agent Runtime Chain
            </span>
          </div>

          {/* Main headline */}
          <h1
            className="text-[36px] md:text-[52px] font-bold tracking-display leading-[1.05] mb-4 animate-fade-slide-in"
            style={{ animationDelay: "100ms" }}
          >
            Verifiable L1
            <br />
            <span className="bg-clip-text text-transparent" style={{ backgroundImage: 'var(--gradient-arc)' }}>
              Block Explorer
            </span>
          </h1>

          {/* Subtitle */}
          <p
            className="text-[15px] md:text-[17px] text-[var(--text-secondary)] leading-relaxed max-w-[560px] mx-auto mb-8 animate-fade-slide-in"
            style={{ animationDelay: "200ms" }}
          >
            Every transaction on ARC Chain is cryptographically verifiable.
            Explore blocks, verify proofs, and audit the chain — independently.
          </p>

          {/* Search bar */}
          <div className="animate-fade-slide-in" style={{ animationDelay: "300ms" }}>
            <SearchBar />
          </div>
        </div>
      </section>

      {/* ================================================================ */}
      {/* STATS BAR                                                        */}
      {/* ================================================================ */}
      <StatsBar stats={stats} dataSource={dataSource} loading={loading} />

      {/* ================================================================ */}
      {/* TPS SHOWCASE                                                     */}
      {/* ================================================================ */}
      <section className="card-surface p-6 md:p-8">
        <LiveTpsCounter />
      </section>

      {/* ================================================================ */}
      {/* LATEST BLOCKS + AGENT ACTIVITY                                   */}
      {/* ================================================================ */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <LatestBlocksTable blocks={blocks} loading={loading} />
        <div className="flex flex-col gap-6">
          <AgentActivityFeed />
        </div>
      </div>

      {/* ================================================================ */}
      {/* LATEST TRANSACTIONS                                              */}
      {/* ================================================================ */}
      <LatestTransactionsTable txs={txs} loading={loading} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Search Bar
// ---------------------------------------------------------------------------

function SearchBar() {
  const [query, setQuery] = useState("");

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const q = query.trim();
    if (!q) return;
    if (/^\d+$/.test(q)) {
      window.location.href = `/block/${q}`;
    } else if (q.startsWith("0x")) {
      window.location.href = q.length > 42 ? `/tx/${q}` : `/account/${q}`;
    }
    setQuery("");
  }

  return (
    <form onSubmit={handleSubmit} className="max-w-[600px] mx-auto w-full px-4">
      <div className="relative group">
        <input
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search by address, tx hash, or block number..."
          className="w-full h-[52px] pl-5 pr-28 rounded-2xl bg-[var(--bg-elevated)] border border-[var(--border)] text-[14px] placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] focus:shadow-[0_0_0_4px_var(--accent-glow),0_8px_32px_rgba(0,0,0,0.06)] transition-all duration-300 shadow-sm"
        />
        <button
          type="submit"
          className="absolute right-2 top-1/2 -translate-y-1/2 px-5 py-2.5 rounded-xl text-white text-[13px] font-medium interactive-press shadow-sm"
          style={{ background: 'var(--gradient-arc)' }}
        >
          Search
        </button>
      </div>
    </form>
  );
}

// ---------------------------------------------------------------------------
// Stats Bar
// ---------------------------------------------------------------------------

function StatsBar({
  stats,
  dataSource,
  loading,
}: {
  stats: Stats | null;
  dataSource: DataSource;
  loading: boolean;
}) {
  if (loading || !stats) {
    return (
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        {Array.from({ length: 4 }).map((_, i) => (
          <div key={i} className="card-surface p-5">
            <div className="h-3 w-16 shimmer rounded mb-3" />
            <div className="h-6 w-24 shimmer rounded" />
          </div>
        ))}
      </div>
    );
  }

  const items = [
    {
      icon: <Blocks className="w-4 h-4" />,
      label: "BLOCK HEIGHT",
      value: `#${fmt.format(stats.blockHeight)}`,
      color: "text-[var(--accent)]",
    },
    {
      icon: <ArrowRightLeft className="w-4 h-4" />,
      label: "TRANSACTIONS",
      value: stats.totalTxs > 1_000_000
        ? `${(stats.totalTxs / 1_000_000).toFixed(2)}M`
        : fmt.format(stats.totalTxs),
      sub: dataSource === "live" ? `${fmt.format(Math.round(stats.tps))} TPS` : undefined,
      color: "text-[var(--text)]",
    },
    {
      icon: <Clock className="w-4 h-4" />,
      label: "AVG BLOCK TIME",
      value: stats.avgBlockTimeMs < 1000
        ? `${stats.avgBlockTimeMs}ms`
        : `${(stats.avgBlockTimeMs / 1000).toFixed(1)}s`,
      color: "text-[var(--text)]",
    },
    {
      icon: <Activity className="w-4 h-4" />,
      label: "NETWORK",
      value: dataSource === "live" ? "Active" : "Demo",
      sub: dataSource === "live"
        ? `${stats.nodeCount} node${stats.nodeCount > 1 ? "s" : ""}`
        : "Simulated data",
      color: dataSource === "live" ? "text-[var(--shield-green)]" : "text-[var(--shield-yellow)]",
    },
  ];

  return (
    <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
      {items.map((item, i) => (
        <div
          key={item.label}
          className="card-surface p-5 animate-fade-slide-in"
          style={{ animationDelay: `${i * 80}ms` }}
        >
          <div className="flex items-center justify-between mb-3">
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label font-medium">
              {item.label}
            </span>
            <div className="w-7 h-7 rounded-lg bg-[var(--bg-secondary)] flex items-center justify-center text-[var(--text-tertiary)]">
              {item.icon}
            </div>
          </div>
          <p className={`text-[20px] font-bold tracking-headline leading-none ${item.color}`}>
            {item.value}
          </p>
          {item.sub && (
            <p className="text-[11px] text-[var(--text-tertiary)] mt-1.5">{item.sub}</p>
          )}
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Latest Blocks Table
// ---------------------------------------------------------------------------

function LatestBlocksTable({
  blocks,
  loading,
}: {
  blocks: Block[];
  loading: boolean;
}) {
  return (
    <div className="card-surface overflow-hidden">
      <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border)]">
        <h2 className="text-[15px] font-semibold flex items-center gap-2.5 tracking-tight">
          <div className="w-7 h-7 rounded-lg flex items-center justify-center" style={{ background: 'var(--gradient-arc)' }}>
            <Box className="w-3.5 h-3.5 text-white" />
          </div>
          Latest Blocks
        </h2>
        <Link
          href="/blocks"
          className="text-[12px] font-medium text-[var(--accent)] hover:underline flex items-center gap-1"
        >
          View all
          <ChevronRight className="w-3 h-3" />
        </Link>
      </div>

      <div className="divide-y divide-[var(--border-light)]">
        {loading
          ? Array.from({ length: 6 }).map((_, i) => (
              <div key={i} className="px-5 py-4">
                <div className="flex items-center gap-4">
                  <div className="w-10 h-10 shimmer rounded-xl" />
                  <div className="flex-1">
                    <div className="h-3.5 w-24 shimmer rounded mb-2" />
                    <div className="h-3 w-40 shimmer rounded" />
                  </div>
                </div>
              </div>
            ))
          : blocks.length === 0
            ? (
              <div className="px-5 py-10 text-center">
                <Box className="w-8 h-8 text-[var(--text-tertiary)] mx-auto mb-3 opacity-40" />
                <p className="text-[13px] text-[var(--text-secondary)] font-medium">No blocks yet</p>
                <p className="text-[11px] text-[var(--text-tertiary)] mt-1">The chain is initializing…</p>
              </div>
            )
            : blocks.slice(0, 6).map((block, i) => (
              <div
                key={block.height}
                className="px-5 py-3.5 flex items-center gap-4 hover:bg-[var(--bg-secondary)]/50 transition-all duration-200 animate-fade-slide-in"
                style={{ animationDelay: `${i * 60}ms` }}
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
                      {block.height === 0 ? "Genesis" : fmt.format(block.height)}
                    </Link>
                    <span className="text-[11px] text-[var(--text-tertiary)]">
                      {block.timestamp > 0 ? timeAgo(block.timestamp) : "Origin"}
                    </span>
                  </div>
                  <p className="text-[11px] text-[var(--text-tertiary)] mt-0.5 font-hash">
                    {shortHash(block.hash, 6)}
                  </p>
                </div>

                <div className="text-right shrink-0">
                  <span className="inline-flex items-center px-2.5 py-1 rounded-lg bg-[var(--bg-secondary)] text-[12px] font-medium text-[var(--text-secondary)] border border-[var(--border-light)]">
                    {fmt.format(block.txCount)} txns
                  </span>
                </div>
              </div>
            ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Latest Transactions Table
// ---------------------------------------------------------------------------

function LatestTransactionsTable({
  txs,
  loading,
}: {
  txs: RecentTx[];
  loading: boolean;
}) {
  const [expandedHash, setExpandedHash] = useState<string | null>(null);

  return (
    <div className="card-surface overflow-hidden">
      <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border)]">
        <h2 className="text-[15px] font-semibold flex items-center gap-2.5 tracking-tight">
          <div className="w-7 h-7 rounded-lg bg-[var(--shield-green-bg)] flex items-center justify-center">
            <ArrowRightLeft className="w-3.5 h-3.5 text-[var(--shield-green)]" />
          </div>
          Latest Transactions
        </h2>
        <Link
          href="/blocks"
          className="text-[12px] font-medium text-[var(--accent)] hover:underline flex items-center gap-1"
        >
          View all
          <ChevronRight className="w-3 h-3" />
        </Link>
      </div>

      <div className="divide-y divide-[var(--border-light)]">
        {loading
          ? Array.from({ length: 6 }).map((_, i) => (
              <div key={i} className="px-5 py-4">
                <div className="flex items-center gap-4">
                  <div className="w-10 h-10 shimmer rounded-xl" />
                  <div className="flex-1">
                    <div className="h-3.5 w-32 shimmer rounded mb-2" />
                    <div className="h-3 w-48 shimmer rounded" />
                  </div>
                </div>
              </div>
            ))
          : txs.slice(0, 6).map((tx, i) => (
              <div key={tx.hash}>
                <div
                  className="px-5 py-3.5 flex items-center gap-4 hover:bg-[var(--bg-secondary)]/50 transition-all duration-200 cursor-pointer animate-fade-slide-in"
                  style={{ animationDelay: `${i * 60}ms` }}
                  onClick={() =>
                    setExpandedHash((prev) =>
                      prev === tx.hash ? null : tx.hash
                    )
                  }
                >
                  <div className="w-10 h-10 rounded-xl bg-[var(--bg-secondary)] flex items-center justify-center shrink-0">
                    <ArrowRightLeft className="w-4 h-4 text-[var(--text-tertiary)]" />
                  </div>

                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2">
                      <Link
                        href={`/tx/${tx.hash}`}
                        className="text-[13px] font-hash font-medium text-[var(--accent)] hover:underline"
                        onClick={(e) => e.stopPropagation()}
                      >
                        {shortHash(tx.hash, 8)}
                      </Link>
                      <span className="text-[11px] text-[var(--text-tertiary)]">
                        {timeAgo(tx.timestamp)}
                      </span>
                    </div>
                    <p className="text-[11px] text-[var(--text-secondary)] mt-0.5 truncate">
                      <span className="text-[var(--text-tertiary)]">From</span>{" "}
                      <span className="font-hash">{shortHash(tx.from, 6)}</span>{" "}
                      <span className="text-[var(--text-tertiary)]">→</span>{" "}
                      <span className="font-hash">{shortHash(tx.to, 6)}</span>
                    </p>
                  </div>

                  <div className="text-right shrink-0">
                    <span className="text-[13px] font-semibold">
                      {fmt.format(Number(tx.amount))}
                    </span>
                    <span className="text-[11px] text-[var(--text-tertiary)] ml-1">ARC</span>
                    <p className="text-[10px] text-[var(--text-tertiary)] mt-0.5 font-medium">
                      {tx.txType}
                    </p>
                  </div>
                </div>

                {expandedHash === tx.hash && <TxProofDrawer tx={tx} />}
              </div>
            ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Tx Proof Drawer (inline BLAKE3 verification)
// ---------------------------------------------------------------------------

function TxProofDrawer({ tx }: { tx: RecentTx }) {
  const [copiedField, setCopiedField] = useState<string | null>(null);

  const displayBlake3 = useMemo(
    () => computeBlake3Hash(tx.preHashHex, "ARC-chain-tx-v1"),
    [tx.preHashHex],
  );

  const copyToClipboard = useCallback((text: string, field: string) => {
    navigator.clipboard.writeText(text).then(() => {
      setCopiedField(field);
      setTimeout(() => setCopiedField(null), 1500);
    });
  }, []);

  return (
    <div
      className="border-t border-[var(--border-light)] px-5 py-4 bg-[var(--bg-secondary)]/40 animate-slide-up"
      style={{ animationDuration: "0.2s" }}
    >
      <div className="flex items-center gap-2 mb-3">
        <Eye className="w-3.5 h-3.5 text-[var(--accent)]" />
        <span className="text-[11px] font-semibold text-[var(--accent)] uppercase tracking-label">
          Cryptographic Proof
        </span>
      </div>

      <div className="rounded-xl bg-[var(--bg-elevated)] border border-[var(--border)] p-4 shadow-sm">
        <div className="flex items-center justify-between mb-2">
          <div className="flex items-center gap-1.5">
            <Hash className="w-3.5 h-3.5 text-[var(--shield-green)]" />
            <span className="text-[11px] font-semibold">BLAKE3 Hash</span>
          </div>
          <span className="text-[9px] font-bold uppercase tracking-wider text-[var(--shield-green)] bg-[var(--shield-green-bg)] px-2 py-0.5 rounded-full">
            Committed
          </span>
        </div>

        <button
          onClick={() => copyToClipboard(displayBlake3, "blake3")}
          className="w-full text-left font-hash text-[11px] text-[var(--text-secondary)] truncate hover:text-[var(--accent)] transition-colors cursor-pointer block"
          title={displayBlake3}
        >
          {copiedField === "blake3" ? (
            <span className="text-[var(--shield-green)]">Copied!</span>
          ) : (
            displayBlake3
          )}
        </button>

        <div className="mt-3 flex items-center justify-between">
          <span className="text-[10px] text-[var(--text-tertiary)]">
            Domain: ARC-chain-tx-v1
          </span>
          <VerifyButton
            size="sm"
            onVerify={async () =>
              verifyBlake3Hash({
                preHashHex: tx.preHashHex,
                claimedHash: displayBlake3,
                domain: "ARC-chain-tx-v1",
              })
            }
          />
        </div>
      </div>

      <div className="mt-3 flex items-center gap-3 text-[11px] text-[var(--text-tertiary)]">
        <span>
          Block{" "}
          <Link
            href={`/block/${tx.blockHeight}`}
            className="font-hash text-[var(--accent)] hover:underline"
          >
            #{fmt.format(tx.blockHeight)}
          </Link>
        </span>
        <span>&middot;</span>
        <span>{tx.txType}</span>
        <button
          onClick={() => copyToClipboard(tx.hash, "txHash")}
          className="ml-auto flex items-center gap-1 text-[var(--accent)] hover:underline cursor-pointer"
        >
          <Copy className="w-3 h-3" />
          {copiedField === "txHash" ? "Copied!" : "Copy Tx Hash"}
        </button>
      </div>
    </div>
  );
}
