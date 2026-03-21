import { useState, useEffect, useCallback, useRef } from 'react';
import { Link } from 'react-router-dom';
import { getStats, getBlocks, getHealth, getInfo, getBlock } from '../api';
import type { StatsResponse, BlockSummary, HealthResponse, InfoResponse } from '../types';
import StatsGrid from '../components/StatsGrid';
import BlocksTable from '../components/BlocksTable';
import TxTable from '../components/TxTable';
import { formatNumber } from '../utils';

export default function Home() {
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [info, setInfo] = useState<InfoResponse | null>(null);
  const [blocks, setBlocks] = useState<BlockSummary[]>([]);
  const [latestTxHashes, setLatestTxHashes] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [tps, setTps] = useState(0);

  // Sparkline history (last 10 snapshots)
  const [tpsHistory, setTpsHistory] = useState<number[]>([]);
  const [blockTimeHistory, setBlockTimeHistory] = useState<number[]>([]);

  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchData = useCallback(async () => {
    try {
      const [statsData, healthData, infoData, blocksData] = await Promise.all([
        getStats(),
        getHealth().catch(() => null),
        getInfo().catch(() => null),
        getBlocks(0, 10000, 10),
      ]);
      setStats(statsData);
      setHealth(healthData);
      setInfo(infoData);
      setBlocks(blocksData.blocks);

      // Fetch latest block's transactions for the "Latest Transactions" section
      if (blocksData.blocks.length > 0) {
        const latestHeight = blocksData.blocks[blocksData.blocks.length - 1].height;
        try {
          const latestBlock = await getBlock(latestHeight);
          setLatestTxHashes(latestBlock.tx_hashes.slice(0, 10));
        } catch {
          // Non-critical — just skip latest TXs
        }
      }

      // Calculate TPS from recent blocks (timestamps are unix millis).
      const recentBlocks = blocksData.blocks.filter(b => b.timestamp > 0);
      let currentTps = 0;
      let avgBlockTime = 0;

      if (recentBlocks.length >= 2) {
        const oldest = recentBlocks[0];
        const newest = recentBlocks[recentBlocks.length - 1];
        const timeSpanMs = newest.timestamp - oldest.timestamp;
        if (timeSpanMs > 0) {
          const totalTxs = recentBlocks.reduce((sum, b) => sum + b.tx_count, 0);
          currentTps = totalTxs / (timeSpanMs / 1000);
          avgBlockTime = timeSpanMs / (recentBlocks.length - 1) / 1000;
        }
      }

      setTps(currentTps);
      setTpsHistory((prev) => [...prev.slice(-9), currentTps]);
      setBlockTimeHistory((prev) => [...prev.slice(-9), avgBlockTime]);

      setError('');
    } catch (err) {
      setError(
        err instanceof Error ? err.message : 'Failed to connect to node'
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    document.title = 'ARC scan — Chain Overview';
    fetchData();
    intervalRef.current = setInterval(fetchData, 5000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [fetchData]);

  const statCards = [
    {
      label: 'Live TPS',
      value: tps > 0
        ? tps >= 1_000_000
          ? (tps / 1_000_000).toFixed(1) + 'M'
          : tps.toLocaleString(undefined, { maximumFractionDigits: 0 })
        : '0',
      suffix: 'tx/s',
      loading,
      sparkline: tpsHistory,
      sparkColor: '#2563EB',
    },
    {
      label: 'Total Transactions',
      value: stats?.total_transactions ?? 0,
      loading,
    },
    {
      label: 'Network Nodes',
      value: (health?.peers ?? 0) + 1,
      loading,
    },
    {
      label: 'Block Height',
      value: stats?.block_height ?? 0,
      loading,
      sparkline: blockTimeHistory,
      sparkColor: '#60A5FA',
    },
  ];

  // Format GPU info if available
  const gpuInfo = info?.gpu;
  const gpuName = typeof gpuInfo === 'object' && gpuInfo !== null
    ? (gpuInfo as Record<string, unknown>).name as string
    : typeof gpuInfo === 'string' ? gpuInfo : null;

  return (
    <div className="space-y-8">
      {/* ─── Hero ────────────────────────────────────────────── */}
      <div className="space-y-2">
        <h1 className="text-3xl font-medium tracking-tight">
          <span className="text-gradient">ARC</span>{' '}
          <span className="text-arc-white">scan</span>
        </h1>
        <p className="text-sm text-arc-grey-600">
          {stats
            ? `${stats.chain} v${stats.version} — ${formatNumber(stats.total_transactions)} transactions processed`
            : 'Connecting to node...'}
        </p>
      </div>

      {/* ─── Error Banner ────────────────────────────────────── */}
      {error && (
        <div className="bg-arc-error/5 border border-arc-error/20 px-4 py-3 text-sm text-arc-error">
          {error}
        </div>
      )}

      {/* ─── Stats Grid ──────────────────────────────────────── */}
      <StatsGrid stats={statCards} />

      {/* ─── Latest Blocks ───────────────────────────────────── */}
      <section>
        <div className="flex items-center justify-between mb-4">
          <h2 className="text-lg font-medium text-arc-white">Latest Blocks</h2>
          <Link to="/blocks" className="btn-arc-outline text-xs">
            View all blocks
          </Link>
        </div>
        <div className="border border-arc-border bg-arc-surface">
          <BlocksTable blocks={blocks} loading={loading} compact />
        </div>
      </section>

      {/* ─── Latest Transactions ─────────────────────────────── */}
      {latestTxHashes.length > 0 && (
        <section>
          <div className="flex items-center justify-between mb-4">
            <h2 className="text-lg font-medium text-arc-white">Latest Transactions</h2>
          </div>
          <div className="border border-arc-border bg-arc-surface">
            <TxTable txHashes={latestTxHashes} loading={loading} compact />
          </div>
        </section>
      )}

      {/* ─── Get Started CTA ─────────────────────────────────── */}
      <section>
        <h2 className="text-lg font-medium text-arc-white mb-4">Get Started</h2>
        <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
          {/* Run a Node */}
          <a
            href="https://build-two-tau-96.vercel.app/docs/quickstart"
            target="_blank"
            rel="noopener noreferrer"
            className="group border border-arc-border bg-arc-surface-raised p-6 hover:border-arc-aquarius/50 transition-all duration-200"
          >
            <div className="flex items-center gap-3 mb-3">
              <div className="w-10 h-10 flex items-center justify-center bg-arc-aquarius/10 text-arc-aquarius">
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <rect x="2" y="2" width="20" height="8" rx="2" ry="2" />
                  <rect x="2" y="14" width="20" height="8" rx="2" ry="2" />
                  <line x1="6" y1="6" x2="6.01" y2="6" />
                  <line x1="6" y1="18" x2="6.01" y2="18" />
                </svg>
              </div>
              <h3 className="text-sm font-medium text-arc-white group-hover:text-arc-aquarius transition-colors">
                Run a Node
              </h3>
            </div>
            <p className="text-xs text-arc-grey-600 leading-relaxed">
              Join ARC Chain as a validator. Earn rewards and help power the AI-native network.
            </p>
          </a>

          {/* Build on ARC */}
          <a
            href="https://build-two-tau-96.vercel.app/docs/architecture"
            target="_blank"
            rel="noopener noreferrer"
            className="group border border-arc-border bg-arc-surface-raised p-6 hover:border-arc-blue/50 transition-all duration-200"
          >
            <div className="flex items-center gap-3 mb-3">
              <div className="w-10 h-10 flex items-center justify-center bg-arc-blue/10 text-arc-blue">
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <polyline points="16 18 22 12 16 6" />
                  <polyline points="8 6 2 12 8 18" />
                </svg>
              </div>
              <h3 className="text-sm font-medium text-arc-white group-hover:text-arc-blue transition-colors">
                Build on ARC
              </h3>
            </div>
            <p className="text-xs text-arc-grey-600 leading-relaxed">
              Deploy smart contracts with WASM support. Full SDK, CLI tools, and comprehensive docs.
            </p>
          </a>

          {/* Join Community */}
          <a
            href="https://x.com/arcreactorai"
            target="_blank"
            rel="noopener noreferrer"
            className="group border border-arc-border bg-arc-surface-raised p-6 hover:border-arc-success/50 transition-all duration-200"
          >
            <div className="flex items-center gap-3 mb-3">
              <div className="w-10 h-10 flex items-center justify-center bg-arc-success/10 text-arc-success">
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                  <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" />
                  <circle cx="9" cy="7" r="4" />
                  <path d="M23 21v-2a4 4 0 0 0-3-3.87" />
                  <path d="M16 3.13a4 4 0 0 1 0 7.75" />
                </svg>
              </div>
              <h3 className="text-sm font-medium text-arc-white group-hover:text-arc-success transition-colors">
                Join Community
              </h3>
            </div>
            <p className="text-xs text-arc-grey-600 leading-relaxed">
              Follow @arcreactorai for updates. Join the testnet and run a node.
            </p>
          </a>
        </div>
      </section>

      {/* ─── Node Info ───────────────────────────────────────── */}
      {(health || info) && (
        <section className="border border-arc-border bg-arc-surface-raised p-5">
          <h3 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-3">
            Node Status
          </h3>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-sm">
            {health?.status && (
              <div>
                <p className="text-arc-grey-600 text-xs mb-1">Status</p>
                <p className="text-arc-success font-medium">{health.status}</p>
              </div>
            )}
            <div>
              <p className="text-arc-grey-600 text-xs mb-1">Version</p>
              <p className="text-arc-white">
                {health?.version || info?.version || '—'}
              </p>
            </div>
            {gpuName && (
              <div>
                <p className="text-arc-grey-600 text-xs mb-1">GPU</p>
                <p className="text-arc-white">{gpuName}</p>
              </div>
            )}
            {typeof health?.uptime_secs === 'number' && (
              <div>
                <p className="text-arc-grey-600 text-xs mb-1">Uptime</p>
                <p className="text-arc-white">
                  {Math.floor(health.uptime_secs / 3600)}h{' '}
                  {Math.floor((health.uptime_secs % 3600) / 60)}m
                </p>
              </div>
            )}
            {typeof health?.peers === 'number' && (
              <div>
                <p className="text-arc-grey-600 text-xs mb-1">Peers</p>
                <p className="text-arc-white">{health.peers}</p>
              </div>
            )}
          </div>
        </section>
      )}
    </div>
  );
}
