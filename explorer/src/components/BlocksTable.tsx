import { Link } from 'react-router-dom';
import type { BlockSummary } from '../types';
import { truncateHash, formatNumber } from '../utils';
import TimeAgo from './TimeAgo';

interface BlocksTableProps {
  blocks: BlockSummary[];
  loading?: boolean;
  compact?: boolean;
}

function SkeletonRows({ count = 5 }: { count?: number }) {
  return (
    <>
      {Array.from({ length: count }).map((_, i) => (
        <tr key={i} className="border-b border-arc-border-subtle">
          <td className="px-4 py-3.5"><div className="skeleton h-4 w-12" /></td>
          <td className="px-4 py-3.5"><div className="skeleton h-4 w-32" /></td>
          <td className="px-4 py-3.5"><div className="skeleton h-4 w-8" /></td>
          <td className="px-4 py-3.5"><div className="skeleton h-4 w-20" /></td>
          <td className="px-4 py-3.5 hidden md:table-cell"><div className="skeleton h-4 w-24" /></td>
        </tr>
      ))}
    </>
  );
}

export default function BlocksTable({
  blocks,
  loading = false,
  compact = false,
}: BlocksTableProps) {
  const displayBlocks = compact ? blocks.slice(0, 10) : blocks;

  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-arc-border text-left">
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
              Height
            </th>
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
              Hash
            </th>
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
              Txns
            </th>
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
              Time
            </th>
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium hidden md:table-cell">
              Producer
            </th>
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <SkeletonRows count={compact ? 10 : 20} />
          ) : displayBlocks.length === 0 ? (
            <tr>
              <td
                colSpan={5}
                className="px-4 py-12 text-center text-arc-grey-600"
              >
                No blocks found
              </td>
            </tr>
          ) : (
            displayBlocks.map((block) => (
              <tr
                key={block.height}
                className="border-b border-arc-border-subtle table-row-hover"
              >
                <td className="px-4 py-3.5">
                  <Link
                    to={`/block/${block.height}`}
                    className="text-arc-aquarius hover:text-arc-blue transition-colors font-medium"
                  >
                    {block.height}
                  </Link>
                </td>
                <td className="px-4 py-3.5 font-mono text-arc-grey-500">
                  <Link
                    to={`/block/${block.height}`}
                    className="hover:text-arc-white transition-colors"
                  >
                    {truncateHash(block.hash)}
                  </Link>
                </td>
                <td className="px-4 py-3.5 text-arc-grey-400">
                  {formatNumber(block.tx_count)}
                </td>
                <td className="px-4 py-3.5">
                  <TimeAgo timestamp={block.timestamp} />
                </td>
                <td className="px-4 py-3.5 font-mono text-arc-grey-600 hidden md:table-cell">
                  {truncateHash(block.producer, 4, 4)}
                </td>
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
