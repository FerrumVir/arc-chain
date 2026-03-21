import { useState, useEffect, useRef } from 'react';
import { Link } from 'react-router-dom';
import { getValidators } from '../api';
import type { ValidatorsResponse } from '../types';
import { formatHash, formatNumber } from '../utils';
import CopyButton from '../components/CopyButton';

/* ─── Tier Badge ────────────────────────────────────────────────── */

function TierBadge({ tier }: { tier: string }) {
  const colors: Record<string, string> = {
    Core: 'text-arc-aquarius',
    Arc: 'text-arc-blue',
    Spark: 'text-arc-success',
  };
  return (
    <span className={`text-sm font-medium ${colors[tier] ?? 'text-arc-grey-500'}`}>
      {tier}
    </span>
  );
}

/* ─── Stake Distribution Bar ────────────────────────────────────── */

function StakeDistribution({
  validators,
  totalStake,
}: {
  validators: { address: string; stake: number; tier: string }[];
  totalStake: number;
}) {
  if (!validators.length || totalStake === 0) return null;

  const tierColors: Record<string, string> = {
    Core: '#2563EB',
    Arc: '#1E40AF',
    Spark: '#60A5FA',
  };

  // Group by tier for legend
  const tierTotals: Record<string, number> = {};
  for (const v of validators) {
    tierTotals[v.tier] = (tierTotals[v.tier] || 0) + v.stake;
  }

  return (
    <div className="border border-arc-border bg-arc-surface-raised p-5">
      <h3 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
        Stake Distribution
      </h3>

      {/* Stacked bar */}
      <div className="flex h-6 overflow-hidden bg-arc-surface mb-4" style={{ gap: '1px' }}>
        {validators.map((v) => {
          const pct = (v.stake / totalStake) * 100;
          if (pct < 0.5) return null;
          return (
            <div
              key={v.address}
              className="h-full transition-all duration-300 hover:opacity-80"
              style={{
                width: `${pct}%`,
                backgroundColor: tierColors[v.tier] || '#777785',
                minWidth: '2px',
              }}
              title={`${formatHash(v.address)}: ${formatNumber(v.stake)} ARC (${pct.toFixed(1)}%)`}
            />
          );
        })}
      </div>

      {/* Legend */}
      <div className="flex flex-wrap gap-4 text-xs">
        {Object.entries(tierTotals).map(([tier, total]) => (
          <div key={tier} className="flex items-center gap-1.5">
            <span
              className="inline-block w-3 h-3"
              style={{ backgroundColor: tierColors[tier] || '#777785' }}
            />
            <span className="text-arc-grey-500">
              {tier}: {formatNumber(total)} ARC ({((total / totalStake) * 100).toFixed(1)}%)
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

/* ─── Main Component ────────────────────────────────────────────── */

export default function Validators() {
  const [data, setData] = useState<ValidatorsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    document.title = 'Validators — ARC scan';

    const fetchData = async () => {
      try {
        const res = await getValidators();
        setData(res);
        setError('');
      } catch (err) {
        setError(
          err instanceof Error ? err.message : 'Failed to load validators'
        );
      } finally {
        setLoading(false);
      }
    };

    fetchData();
    intervalRef.current = setInterval(fetchData, 10000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, []);

  if (loading) {
    return (
      <div className="space-y-6">
        <div className="skeleton h-8 w-48" />
        <div className="border border-arc-border bg-arc-surface-raised p-6 space-y-4">
          {Array.from({ length: 6 }).map((_, i) => (
            <div key={i} className="flex gap-4">
              <div className="skeleton h-4 w-8" />
              <div className="skeleton h-4 w-64" />
              <div className="skeleton h-4 w-32" />
              <div className="skeleton h-4 w-20" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="space-y-4">
        <h1 className="text-2xl font-medium text-arc-white">Validators</h1>
        <div className="bg-arc-error/5 border border-arc-error/20 px-4 py-3 text-sm text-arc-error">
          {error}
        </div>
        <Link to="/" className="btn-arc-outline text-xs">
          Back to home
        </Link>
      </div>
    );
  }

  return (
    <div className="space-y-8">
      {/* ─── Header ──────────────────────────────────────────── */}
      <div className="space-y-2">
        <h1 className="text-2xl font-medium tracking-tight text-arc-white">
          Validators
        </h1>
        <p className="text-sm text-arc-grey-600">
          {data
            ? `${data.count} active validator${data.count !== 1 ? 's' : ''} — ${formatNumber(data.total_stake)} ARC total stake`
            : 'Loading...'}
        </p>
      </div>

      {/* ─── Stats ───────────────────────────────────────────── */}
      {data && (
        <div className="grid grid-cols-2 md:grid-cols-3 gap-4">
          <div className="border border-arc-border bg-arc-surface-raised p-4">
            <p className="text-xs uppercase tracking-widest text-arc-grey-600 mb-1">
              Active Validators
            </p>
            <p className="text-2xl font-medium text-arc-white">{data.count}</p>
          </div>
          <div className="border border-arc-border bg-arc-surface-raised p-4">
            <p className="text-xs uppercase tracking-widest text-arc-grey-600 mb-1">
              Total Stake
            </p>
            <p className="text-2xl font-medium text-arc-white">
              {formatNumber(data.total_stake)}
              <span className="text-sm text-arc-grey-600 ml-1">ARC</span>
            </p>
          </div>
          <div className="border border-arc-border bg-arc-surface-raised p-4">
            <p className="text-xs uppercase tracking-widest text-arc-grey-600 mb-1">
              Avg Stake
            </p>
            <p className="text-2xl font-medium text-arc-white">
              {data.count > 0
                ? formatNumber(Math.floor(data.total_stake / data.count))
                : '0'}
              <span className="text-sm text-arc-grey-600 ml-1">ARC</span>
            </p>
          </div>
        </div>
      )}

      {/* ─── Stake Distribution Chart ────────────────────────── */}
      {data && (
        <StakeDistribution
          validators={data.validators}
          totalStake={data.total_stake}
        />
      )}

      {/* ─── Validators Table ────────────────────────────────── */}
      {data && (
        <div className="border border-arc-border bg-arc-surface overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-arc-border text-left">
                <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium w-16">
                  Rank
                </th>
                <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
                  Address
                </th>
                <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium text-right">
                  Stake
                </th>
                <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium text-right">
                  Share
                </th>
                <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
                  Tier
                </th>
              </tr>
            </thead>
            <tbody>
              {data.validators.length === 0 ? (
                <tr>
                  <td
                    colSpan={5}
                    className="px-4 py-12 text-center text-arc-grey-600"
                  >
                    No validators found
                  </td>
                </tr>
              ) : (
                data.validators.map((v, i) => (
                  <tr
                    key={v.address}
                    className="border-b border-arc-border-subtle table-row-hover"
                  >
                    <td className="px-4 py-3.5 text-arc-grey-500 font-medium">
                      {i + 1}
                    </td>
                    <td className="px-4 py-3.5 font-mono">
                      <span className="flex items-center gap-1">
                        <Link
                          to={`/account/${v.address}`}
                          className="text-arc-aquarius hover:text-arc-blue transition-colors text-xs"
                        >
                          {formatHash(v.address)}
                        </Link>
                        <CopyButton text={v.address} />
                      </span>
                    </td>
                    <td className="px-4 py-3.5 text-right text-arc-white">
                      {formatNumber(v.stake)}
                      <span className="text-arc-grey-600 ml-1">ARC</span>
                    </td>
                    <td className="px-4 py-3.5 text-right text-arc-grey-400">
                      {data.total_stake > 0
                        ? ((v.stake / data.total_stake) * 100).toFixed(1)
                        : '0'}
                      %
                    </td>
                    <td className="px-4 py-3.5">
                      <TierBadge tier={v.tier} />
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
