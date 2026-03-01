import { useState, useEffect, useCallback } from 'react';
import { getBlocks, getStats } from '../api';
import type { BlockSummary } from '../types';
import BlocksTable from '../components/BlocksTable';

const PAGE_SIZE = 20;

export default function Blocks() {
  const [blocks, setBlocks] = useState<BlockSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [currentPage, setCurrentPage] = useState(0);
  const [totalBlocks, setTotalBlocks] = useState(0);

  const fetchBlocks = useCallback(async (page: number) => {
    setLoading(true);
    try {
      const stats = await getStats();
      setTotalBlocks(stats.block_height);

      const from = page * PAGE_SIZE;
      const to = from + PAGE_SIZE;
      const data = await getBlocks(from, to, PAGE_SIZE);
      setBlocks(data.blocks);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load blocks');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    document.title = 'Blocks — ARC Explorer';
    fetchBlocks(currentPage);
  }, [currentPage, fetchBlocks]);

  const totalPages = Math.max(1, Math.ceil(totalBlocks / PAGE_SIZE));
  const canPrev = currentPage > 0;
  const canNext = currentPage < totalPages - 1;

  return (
    <div className="space-y-6">
      {/* ─── Header ──────────────────────────────────────────── */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-medium tracking-tight text-arc-white">
            Blocks
          </h1>
          <p className="text-sm text-arc-grey-600 mt-1">
            {totalBlocks > 0 ? `${totalBlocks} total blocks` : 'Loading...'}
          </p>
        </div>
      </div>

      {/* ─── Error ───────────────────────────────────────────── */}
      {error && (
        <div className="bg-arc-error/5 border border-arc-error/20 px-4 py-3 text-sm text-arc-error">
          {error}
        </div>
      )}

      {/* ─── Table ───────────────────────────────────────────── */}
      <div className="border border-arc-border bg-arc-surface">
        <BlocksTable blocks={blocks} loading={loading} />
      </div>

      {/* ─── Pagination ──────────────────────────────────────── */}
      <div className="flex items-center justify-between text-sm">
        <p className="text-arc-grey-600">
          Page {currentPage + 1} of {totalPages}
        </p>
        <div className="flex gap-2">
          <button
            onClick={() => setCurrentPage((p) => p - 1)}
            disabled={!canPrev}
            className={`
              btn-arc-outline text-xs px-4 py-2
              ${!canPrev ? 'opacity-30 cursor-not-allowed' : ''}
            `}
          >
            Previous
          </button>
          <button
            onClick={() => setCurrentPage((p) => p + 1)}
            disabled={!canNext}
            className={`
              btn-arc-outline text-xs px-4 py-2
              ${!canNext ? 'opacity-30 cursor-not-allowed' : ''}
            `}
          >
            Next
          </button>
        </div>
      </div>
    </div>
  );
}
