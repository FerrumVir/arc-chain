import { useState, useEffect, useCallback, useRef } from 'react';
import { Link } from 'react-router-dom';
import { fetchAgents, fetchAgentActions, getStats } from '../api';
import type { AgentInfo, AgentAction, StatsResponse } from '../types';
import { truncateHash, formatNumber, timeAgo, copyToClipboard } from '../utils';

/* ─── Simulated demo data (used when no agents are registered) ── */

const DEMO_AGENTS: AgentInfo[] = [
  {
    name: 'SentimentBot v1',
    address: 'a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890',
    status: 'active',
    model_type: 'Neural Net — 3 layers, 8K params',
    endpoint: 'http://localhost:8081',
    inferences: 1234,
    earned: 456,
    uptime_secs: 302400,
    last_action: "Classified 'Great product!' → Positive (92%)",
    last_action_timestamp: Math.floor(Date.now() / 1000) - 12,
  },
  {
    name: 'ToxicityGuard v2',
    address: 'b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f6789ab',
    status: 'active',
    model_type: 'Transformer — 6 layers, 42K params',
    endpoint: 'http://localhost:8082',
    inferences: 8921,
    earned: 2103,
    uptime_secs: 518400,
    last_action: "Flagged comment as Toxic (87%)",
    last_action_timestamp: Math.floor(Date.now() / 1000) - 3,
  },
  {
    name: 'PriceOracle v1',
    address: 'c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f6789abcd',
    status: 'paused',
    model_type: 'LSTM — 4 layers, 16K params',
    endpoint: 'http://localhost:8083',
    inferences: 421,
    earned: 89,
    uptime_secs: 43200,
    last_action: "Predicted ETH/USD ratio (±2.1%)",
    last_action_timestamp: Math.floor(Date.now() / 1000) - 3600,
  },
];

const DEMO_ACTIONS: AgentAction[] = [
  {
    timestamp: Math.floor(Date.now() / 1000) - 3,
    agent_name: 'ToxicityGuard v2',
    action: 'classified input',
    result: 'Toxic',
    confidence: 87,
    amount: 5,
    tx_hash: '3a8f1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f6',
  },
  {
    timestamp: Math.floor(Date.now() / 1000) - 12,
    agent_name: 'SentimentBot v1',
    action: 'classified input',
    result: 'Positive',
    confidence: 92,
    amount: 5,
    tx_hash: '4b9f2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67a',
  },
  {
    timestamp: Math.floor(Date.now() / 1000) - 25,
    agent_name: 'SentimentBot v1',
    action: 'classified input',
    result: 'Negative',
    confidence: 78,
    amount: 5,
    tx_hash: '5caf3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890',
  },
  {
    timestamp: Math.floor(Date.now() / 1000) - 41,
    agent_name: 'ToxicityGuard v2',
    action: 'classified input',
    result: 'Safe',
    confidence: 96,
    amount: 5,
    tx_hash: '6dbf4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890ab',
  },
  {
    timestamp: Math.floor(Date.now() / 1000) - 58,
    agent_name: 'PriceOracle v1',
    action: 'predicted price',
    result: 'ETH/USD ratio',
    confidence: 81,
    amount: 8,
    tx_hash: '7ecf5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890a1b2c3d4e5f67890abcd',
  },
];

/* ─── Helpers ───────────────────────────────────────────────────── */

function formatUptime(secs: number): string {
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  if (days > 0) return `${days}d ${hours}h`;
  const mins = Math.floor((secs % 3600) / 60);
  return `${hours}h ${mins}m`;
}

function statusColor(status: AgentInfo['status']): string {
  switch (status) {
    case 'active':
      return 'bg-arc-success';
    case 'paused':
      return 'bg-arc-grey-600';
    case 'terminated':
      return 'bg-arc-error';
  }
}

function statusLabel(status: AgentInfo['status']): string {
  return status.charAt(0).toUpperCase() + status.slice(1);
}

/* ─── Component ─────────────────────────────────────────────────── */

export default function Agents() {
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [actions, setActions] = useState<AgentAction[]>([]);
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [copiedAddr, setCopiedAddr] = useState<string | null>(null);
  const [newActionIdx, setNewActionIdx] = useState<number | null>(null);
  const [isDemo, setIsDemo] = useState(false);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchData = useCallback(async () => {
    try {
      const [agentsRes, actionsRes, statsRes] = await Promise.all([
        fetchAgents().catch(() => null),
        fetchAgentActions().catch(() => []),
        getStats().catch(() => null),
      ]);

      if (agentsRes && agentsRes.agents.length > 0) {
        setAgents(agentsRes.agents);
        setIsDemo(false);
      } else {
        setAgents(DEMO_AGENTS);
        setIsDemo(true);
      }

      if (actionsRes.length > 0) {
        setActions(actionsRes);
      } else if (isDemo || !agentsRes || agentsRes.agents.length === 0) {
        setActions(DEMO_ACTIONS);
      }

      setStats(statsRes);
    } catch {
      setAgents(DEMO_AGENTS);
      setActions(DEMO_ACTIONS);
      setIsDemo(true);
    } finally {
      setLoading(false);
    }
  }, [isDemo]);

  useEffect(() => {
    document.title = 'ARC scan — Synths';
    fetchData();
    intervalRef.current = setInterval(fetchData, 5000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [fetchData]);

  // Brief highlight animation when new actions come in
  useEffect(() => {
    if (actions.length > 0) {
      setNewActionIdx(0);
      const timer = setTimeout(() => setNewActionIdx(null), 1000);
      return () => clearTimeout(timer);
    }
  }, [actions]);

  const handleCopyAddress = async (address: string) => {
    const ok = await copyToClipboard(address.startsWith('0x') ? address : `0x${address}`);
    if (ok) {
      setCopiedAddr(address);
      setTimeout(() => setCopiedAddr(null), 2000);
    }
  };

  // Computed stats
  const totalSynths = agents.length;
  const activeSynths = agents.filter((a) => a.status === 'active').length;
  const totalInferences = agents.reduce((sum, a) => sum + a.inferences, 0);
  const totalEarned = agents.reduce((sum, a) => sum + a.earned, 0);

  /* ─── Empty state ───────────────────────────────────────────── */

  if (!loading && agents.length === 0) {
    return (
      <div className="space-y-8">
        <div className="space-y-2">
          <h1 className="text-3xl font-medium tracking-tight">
            <span className="text-gradient">Synths</span>
          </h1>
          <p className="text-sm text-arc-grey-600">
            Autonomous AI agents living on ARC Chain
          </p>
        </div>

        <div className="border border-arc-border bg-arc-surface-raised p-12 text-center space-y-4">
          <p className="text-lg text-arc-white">No Synths deployed yet</p>
          <p className="text-sm text-arc-grey-600">Be the first:</p>
          <code className="block text-sm font-mono text-arc-aquarius bg-arc-surface px-4 py-2 border border-arc-border inline-block">
            cargo run --release -p arc-agents --bin sentiment-agent
          </code>
          <div className="pt-4">
            <a
              href="https://build-two-tau-96.vercel.app/docs/agents/deploy-agent"
              target="_blank"
              rel="noopener noreferrer"
              className="btn-arc-outline text-xs"
            >
              Read the Agents README
              <svg
                className="inline-block ml-1 -mt-0.5"
                width="10"
                height="10"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
                <polyline points="15 3 21 3 21 9" />
                <line x1="10" y1="14" x2="21" y2="3" />
              </svg>
            </a>
          </div>
        </div>
      </div>
    );
  }

  /* ─── Main render ───────────────────────────────────────────── */

  return (
    <div className="space-y-8">
      {/* ─── Header ────────────────────────────────────────────── */}
      <div className="space-y-2">
        <h1 className="text-3xl font-medium tracking-tight">
          <span className="text-gradient">Synths</span>
        </h1>
        <p className="text-sm text-arc-grey-600">
          Autonomous AI agents living on ARC Chain
        </p>
      </div>

      {/* Demo banner */}
      {isDemo && (
        <div className="bg-arc-info/5 border border-arc-info/20 px-4 py-3 text-sm text-arc-info">
          Demo mode — showing simulated agents. Deploy a real agent to see live data.
        </div>
      )}

      {/* ─── Stats Bar ─────────────────────────────────────────── */}
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        {[
          { label: 'Total Synths', value: formatNumber(totalSynths) },
          { label: 'Active', value: formatNumber(activeSynths) },
          { label: 'Inferences Today', value: formatNumber(totalInferences) },
          { label: 'Total Settled', value: `${formatNumber(totalEarned)} ARC` },
        ].map((stat) => (
          <div
            key={stat.label}
            className="border border-arc-border bg-arc-surface-raised p-4"
          >
            <p className="text-xs text-arc-grey-600 mb-1">{stat.label}</p>
            <p className="text-lg font-medium text-arc-white stat-value">
              {loading ? (
                <span className="skeleton inline-block w-12 h-5" />
              ) : (
                stat.value
              )}
            </p>
          </div>
        ))}
      </div>

      {/* ─── Agent Cards ───────────────────────────────────────── */}
      <section>
        <h2 className="text-lg font-medium text-arc-white mb-4">
          Registered Synths
        </h2>
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
          {loading
            ? Array.from({ length: 3 }).map((_, i) => (
                <div
                  key={i}
                  className="border border-arc-border bg-arc-surface-raised p-5 space-y-4"
                >
                  <div className="skeleton h-5 w-40" />
                  <div className="skeleton h-3 w-56" />
                  <div className="skeleton h-3 w-32" />
                  <div className="skeleton h-3 w-48" />
                </div>
              ))
            : agents.map((agent) => (
                <div
                  key={agent.address}
                  className="card-glow border border-arc-border bg-arc-surface-raised p-5 space-y-3 hover:border-arc-aquarius/30 transition-all duration-200"
                >
                  {/* Name + Status */}
                  <div className="flex items-center justify-between">
                    <h3 className="text-sm font-medium text-arc-white">
                      {agent.name}
                    </h3>
                    <div className="flex items-center gap-1.5">
                      <span
                        className={`inline-block w-2 h-2 rounded-full ${statusColor(agent.status)} ${
                          agent.status === 'active' ? 'animate-pulse-dot' : ''
                        }`}
                      />
                      <span
                        className={`text-xs ${
                          agent.status === 'active'
                            ? 'text-arc-success'
                            : agent.status === 'paused'
                              ? 'text-arc-grey-600'
                              : 'text-arc-error'
                        }`}
                      >
                        {statusLabel(agent.status)}
                      </span>
                    </div>
                  </div>

                  {/* Model type */}
                  <p className="text-xs text-arc-grey-600">
                    {agent.model_type}
                  </p>

                  {/* Address (click to copy) */}
                  <button
                    onClick={() => handleCopyAddress(agent.address)}
                    className="text-xs font-mono text-arc-aquarius hover:text-arc-blue transition-colors cursor-pointer flex items-center gap-1"
                    title="Click to copy address"
                  >
                    {truncateHash(agent.address)}
                    {copiedAddr === agent.address ? (
                      <span className="text-arc-success text-[10px] animate-toast">
                        Copied!
                      </span>
                    ) : (
                      <svg
                        width="12"
                        height="12"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="2"
                        strokeLinecap="round"
                        strokeLinejoin="round"
                        className="opacity-40"
                      >
                        <rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
                        <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
                      </svg>
                    )}
                  </button>

                  {/* Stats row */}
                  <div className="flex items-center gap-4 text-xs text-arc-grey-500">
                    <span>
                      Inferences:{' '}
                      <span className="text-arc-white">
                        {formatNumber(agent.inferences)}
                      </span>
                    </span>
                    <span>
                      Earned:{' '}
                      <span className="text-arc-white">
                        {formatNumber(agent.earned)} ARC
                      </span>
                    </span>
                    <span>
                      Uptime:{' '}
                      <span className="text-arc-white">
                        {formatUptime(agent.uptime_secs)}
                      </span>
                    </span>
                  </div>

                  {/* Last action */}
                  <div className="border-t border-arc-border-subtle pt-3">
                    <p className="text-xs text-arc-grey-600 mb-1">
                      Last action
                    </p>
                    <p className="text-xs text-arc-grey-400">
                      {agent.last_action}
                    </p>
                    <p className="text-[10px] text-arc-grey-700 mt-1">
                      {timeAgo(agent.last_action_timestamp)}
                    </p>
                  </div>
                </div>
              ))}
        </div>
      </section>

      {/* ─── Live Activity Feed ────────────────────────────────── */}
      {actions.length > 0 && (
        <section>
          <h2 className="text-lg font-medium text-arc-white mb-4">
            Live Activity
          </h2>
          <div className="border border-arc-border bg-arc-surface-raised divide-y divide-arc-border-subtle max-h-96 overflow-y-auto">
            {actions.map((action, idx) => (
              <div
                key={`${action.tx_hash}-${idx}`}
                className={`px-4 py-3 text-xs transition-all duration-500 ${
                  idx === newActionIdx
                    ? 'bg-arc-aquarius/5'
                    : 'table-row-hover'
                }`}
                style={
                  idx === newActionIdx
                    ? {
                        animation: 'fade-in 500ms ease-out',
                      }
                    : undefined
                }
              >
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                  <span className="text-arc-grey-700 font-mono">
                    [{timeAgo(action.timestamp)}]
                  </span>
                  <span className="text-arc-white font-medium">
                    {action.agent_name}
                  </span>
                  <span className="text-arc-grey-500">
                    {action.action} &rarr;{' '}
                    <span className="text-arc-aquarius">
                      {action.result} ({action.confidence}%)
                    </span>
                  </span>
                  <span className="text-arc-grey-700">|</span>
                  <span className="text-arc-grey-500">
                    settled{' '}
                    <span className="text-arc-white">{action.amount} ARC</span>
                  </span>
                  <span className="text-arc-grey-700">|</span>
                  <span className="text-arc-grey-500">
                    TX:{' '}
                    <Link
                      to={`/tx/${action.tx_hash}`}
                      className="text-arc-aquarius hover:text-arc-blue transition-colors font-mono"
                    >
                      {truncateHash(action.tx_hash)}
                    </Link>
                  </span>
                </div>
              </div>
            ))}
          </div>
        </section>
      )}

      {/* ─── Deploy CTA ────────────────────────────────────────── */}
      <section className="border border-arc-border bg-arc-surface-raised p-6">
        <h3 className="text-sm font-medium text-arc-white mb-2">
          Deploy Your Own Synth
        </h3>
        <p className="text-xs text-arc-grey-600 mb-3">
          Run an AI agent on ARC Chain and earn ARC for every inference settled
          on-chain.
        </p>
        <code className="block text-xs font-mono text-arc-aquarius bg-arc-surface px-3 py-2 border border-arc-border mb-3">
          cargo run --release -p arc-agents --bin sentiment-agent
        </code>
        <a
          href="https://build-two-tau-96.vercel.app/docs/agents/deploy-agent"
          target="_blank"
          rel="noopener noreferrer"
          className="btn-arc-outline text-xs"
        >
          View Docs
          <svg
            className="inline-block ml-1 -mt-0.5"
            width="10"
            height="10"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
            <polyline points="15 3 21 3 21 9" />
            <line x1="10" y1="14" x2="21" y2="3" />
          </svg>
        </a>
      </section>
    </div>
  );
}
