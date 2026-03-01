import { Link } from 'react-router-dom';
import { truncateHash } from '../utils';

interface TxTableProps {
  txHashes: string[];
  loading?: boolean;
  compact?: boolean;
  showBlockHeight?: boolean;
  blockHeight?: number;
}

function SkeletonRows({ count = 5 }: { count?: number }) {
  return (
    <>
      {Array.from({ length: count }).map((_, i) => (
        <tr key={i} className="border-b border-arc-border-subtle">
          <td className="px-4 py-3.5"><div className="skeleton h-4 w-40" /></td>
          <td className="px-4 py-3.5"><div className="skeleton h-4 w-16" /></td>
        </tr>
      ))}
    </>
  );
}

export default function TxTable({
  txHashes,
  loading = false,
  compact = false,
  showBlockHeight = false,
  blockHeight,
}: TxTableProps) {
  const displayHashes = compact ? txHashes.slice(0, 10) : txHashes;

  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-arc-border text-left">
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
              Transaction Hash
            </th>
            {showBlockHeight && (
              <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
                Block
              </th>
            )}
            <th className="px-4 py-3 text-xs uppercase tracking-widest text-arc-grey-600 font-medium">
              Index
            </th>
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <SkeletonRows count={compact ? 10 : 20} />
          ) : displayHashes.length === 0 ? (
            <tr>
              <td
                colSpan={showBlockHeight ? 3 : 2}
                className="px-4 py-12 text-center text-arc-grey-600"
              >
                No transactions found
              </td>
            </tr>
          ) : (
            displayHashes.map((hash, index) => (
              <tr
                key={hash}
                className="border-b border-arc-border-subtle table-row-hover"
              >
                <td className="px-4 py-3.5 font-mono">
                  <Link
                    to={`/tx/${hash}`}
                    className="text-arc-aquarius hover:text-arc-blue transition-colors"
                  >
                    {truncateHash(hash, 10, 8)}
                  </Link>
                </td>
                {showBlockHeight && (
                  <td className="px-4 py-3.5">
                    <Link
                      to={`/block/${blockHeight}`}
                      className="text-arc-aquarius hover:text-arc-blue transition-colors font-medium"
                    >
                      {blockHeight}
                    </Link>
                  </td>
                )}
                <td className="px-4 py-3.5 text-arc-grey-500">
                  {index}
                </td>
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
