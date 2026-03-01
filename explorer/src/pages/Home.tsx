import { useState, useEffect, useCallback, useRef } from 'react';
import { Link } from 'react-router-dom';
import { getStats, getBlocks, getHealth, getInfo } from '../api';
import type { StatsResponse, BlockSummary, HealthResponse, InfoResponse } from '../types';
import StatsGrid from '../components/StatsGrid';
import BlocksTable from '../components/BlocksTable';

export default function Home() {
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [info, setInfo] = useState<InfoResponse | null>(null);
  const [blocks, setBlocks] = useState<BlockSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [tps, setTps] = useState(0);
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

      // Calculate TPS from recent blocks (timestamps are unix millis)
      const recentBlocks = blocksData.blocks;
      if (recentBlocks.length >= 2) {
        const newest = recentBlocks[0];
        const oldest = recentBlocks[recentBlocks.length - 1];
        const timeSpanMs = newest.timestamp - oldest.timestamp;
        if (timeSpanMs > 0) {
          const totalTxs = recentBlocks.reduce((sum, b) => sum + b.tx_count, 0);
          setTps(totalTxs / (timeSpanMs / 1000));
        }
      }

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
      value: tps > 0 ? tps.toLocaleString(undefined, { maximumFractionDigits: 0 }) : '0',
      suffix: 'tx/s',
      loading,
    },
    {
      label: 'Total Transactions',
      value: stats?.total_receipts ?? 0,
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
            ? `${stats.chain} v${stats.version} — ${stats.total_receipts.toLocaleString()} transactions processed`
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
