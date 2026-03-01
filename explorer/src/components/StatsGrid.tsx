import { formatNumber } from '../utils';

interface StatCard {
  label: string;
  value: string | number;
  suffix?: string;
  loading?: boolean;
}

interface StatsGridProps {
  stats: StatCard[];
}

function SkeletonCard() {
  return (
    <div className="bg-arc-surface-raised border border-arc-border p-5">
      <div className="skeleton h-3 w-24 mb-3" />
      <div className="skeleton h-7 w-32" />
    </div>
  );
}

export default function StatsGrid({ stats }: StatsGridProps) {
  return (
    <div className="grid grid-cols-2 lg:grid-cols-4 gap-px bg-arc-border">
      {stats.map((stat, i) =>
        stat.loading ? (
          <SkeletonCard key={i} />
        ) : (
          <div
            key={i}
            className="bg-arc-surface-raised p-5 card-glow"
          >
            <p className="text-xs uppercase tracking-widest text-arc-grey-600 mb-2">
              {stat.label}
            </p>
            <p className="text-2xl font-medium tracking-tight text-arc-white">
              {typeof stat.value === 'number'
                ? formatNumber(stat.value)
                : stat.value}
              {stat.suffix && (
                <span className="text-sm text-arc-grey-600 ml-1.5">
                  {stat.suffix}
                </span>
              )}
            </p>
          </div>
        )
      )}
    </div>
  );
}
