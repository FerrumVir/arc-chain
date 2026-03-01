import { useState, useEffect, useMemo } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getBlock } from '../api';
import type { BlockDetail as BlockDetailType } from '../types';
import { formatHash, formatTimestamp } from '../utils';
import CopyButton from '../components/CopyButton';
import TimeAgo from '../components/TimeAgo';
import TxTable from '../components/TxTable';

const TX_PAGE_SIZE = 25;

interface DetailRowProps {
  label: string;
  children: React.ReactNode;
}

function DetailRow({ label, children }: DetailRowProps) {
  return (
    <div className="flex flex-col sm:flex-row sm:items-start border-b border-arc-border-subtle py-3.5 gap-1 sm:gap-0">
      <span className="text-xs uppercase tracking-widest text-arc-grey-600 sm:w-40 shrink-0">
        {label}
      </span>
      <span className="text-sm text-arc-white break-all flex-1">
        {children}
      </span>
    </div>
  );
}

function TransactionSection({
  txHashes,
  blockHeight,
  txPage,
  setTxPage,
}: {
  txHashes: string[];
  blockHeight: number;
  txPage: number;
  setTxPage: (p: number | ((p: number) => number)) => void;
}) {
  const totalTxPages = Math.max(1, Math.ceil(txHashes.length / TX_PAGE_SIZE));
  const paginatedHashes = useMemo(
    () =>
      txHashes.slice(
        txPage * TX_PAGE_SIZE,
        (txPage + 1) * TX_PAGE_SIZE
      ),
    [txHashes, txPage]
  );

  return (
    <section>
      <h2 className="text-lg font-medium text-arc-white mb-4">
        Transactions
        <span className="text-arc-grey-600 text-sm ml-2">
          ({txHashes.length.toLocaleString()})
        </span>
      </h2>
      <div className="border border-arc-border bg-arc-surface">
        <TxTable
          txHashes={paginatedHashes}
          showBlockHeight
          blockHeight={blockHeight}
        />
      </div>
      {txHashes.length > TX_PAGE_SIZE && (
        <div className="flex items-center justify-between text-sm mt-4">
          <p className="text-arc-grey-600">
            Showing {txPage * TX_PAGE_SIZE + 1}–
            {Math.min((txPage + 1) * TX_PAGE_SIZE, txHashes.length)} of{' '}
            {txHashes.length.toLocaleString()}
          </p>
          <div className="flex gap-2">
            <button
              onClick={() => setTxPage((p: number) => p - 1)}
              disabled={txPage === 0}
              className={`btn-arc-outline text-xs px-4 py-2 ${
                txPage === 0 ? 'opacity-30 cursor-not-allowed' : ''
              }`}
            >
              Previous
            </button>
            <button
              onClick={() => setTxPage((p: number) => p + 1)}
              disabled={txPage >= totalTxPages - 1}
              className={`btn-arc-outline text-xs px-4 py-2 ${
                txPage >= totalTxPages - 1
                  ? 'opacity-30 cursor-not-allowed'
                  : ''
              }`}
            >
              Next
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

export default function BlockDetail() {
  const { height } = useParams<{ height: string }>();
  const [block, setBlock] = useState<BlockDetailType | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [txPage, setTxPage] = useState(0);

  useEffect(() => {
    if (!height) return;
    document.title = `Block #${height} — ARC Explorer`;
    setLoading(true);

    getBlock(Number(height))
      .then((data) => {
        setBlock(data);
        setError('');
      })
      .catch((err) => {
        setError(err instanceof Error ? err.message : 'Failed to load block');
      })
      .finally(() => setLoading(false));
  }, [height]);

  if (loading) {
    return (
      <div className="space-y-6">
        <div className="skeleton h-8 w-48" />
        <div className="border border-arc-border bg-arc-surface-raised p-6 space-y-4">
          {Array.from({ length: 8 }).map((_, i) => (
            <div key={i} className="flex gap-4">
              <div className="skeleton h-4 w-32" />
              <div className="skeleton h-4 w-64" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="space-y-4">
        <h1 className="text-2xl font-medium text-arc-white">Block Not Found</h1>
        <div className="bg-arc-error/5 border border-arc-error/20 px-4 py-3 text-sm text-arc-error">
          {error}
        </div>
        <Link to="/blocks" className="btn-arc-outline text-xs">
          Back to blocks
        </Link>
      </div>
    );
  }

  if (!block) return null;

  const blockHeight = block.header.height;

  return (
    <div className="space-y-8">
      {/* ─── Header ──────────────────────────────────────────── */}
      <div className="flex items-center gap-4">
        <h1 className="text-2xl font-medium tracking-tight text-arc-white">
          Block{' '}
          <span className="text-gradient">#{blockHeight}</span>
        </h1>

        {/* Nav arrows */}
        <div className="flex items-center gap-1 ml-auto">
          {blockHeight > 0 && (
            <Link
              to={`/block/${blockHeight - 1}`}
              className="btn-arc-outline text-xs px-3 py-1.5"
            >
              &larr; Prev
            </Link>
          )}
          <Link
            to={`/block/${blockHeight + 1}`}
            className="btn-arc-outline text-xs px-3 py-1.5"
          >
            Next &rarr;
          </Link>
        </div>
      </div>

      {/* ─── Block Details ───────────────────────────────────── */}
      <div className="border border-arc-border bg-arc-surface-raised p-6">
        <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
          Block Header
        </h2>

        <DetailRow label="Height">{blockHeight}</DetailRow>

        <DetailRow label="Timestamp">
          <span className="flex items-center gap-2">
            {formatTimestamp(block.header.timestamp)}
            <TimeAgo timestamp={block.header.timestamp} className="text-xs" />
          </span>
        </DetailRow>

        <DetailRow label="Hash">
          <span className="flex items-center gap-1 font-mono text-xs">
            {formatHash(block.hash)}
            <CopyButton text={block.hash} />
          </span>
        </DetailRow>

        <DetailRow label="Parent Hash">
          {blockHeight > 0 ? (
            <span className="flex items-center gap-1">
              <Link
                to={`/block/${blockHeight - 1}`}
                className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors"
              >
                {formatHash(block.header.parent_hash)}
              </Link>
              <CopyButton text={block.header.parent_hash} />
            </span>
          ) : (
            <span className="font-mono text-xs text-arc-grey-600">Genesis</span>
          )}
        </DetailRow>

        <DetailRow label="Tx Root">
          <span className="flex items-center gap-1 font-mono text-xs">
            {formatHash(block.header.tx_root)}
            <CopyButton text={block.header.tx_root} />
          </span>
        </DetailRow>

        <DetailRow label="State Root">
          <span className="flex items-center gap-1 font-mono text-xs">
            {formatHash(block.header.state_root)}
            <CopyButton text={block.header.state_root} />
          </span>
        </DetailRow>

        <DetailRow label="Proof Hash">
          <span className="flex items-center gap-1 font-mono text-xs">
            {formatHash(block.header.proof_hash)}
            <CopyButton text={block.header.proof_hash} />
          </span>
        </DetailRow>

        <DetailRow label="Tx Count">
          <span className="font-medium">{block.header.tx_count}</span>
        </DetailRow>

        <DetailRow label="Producer">
          <span className="flex items-center gap-1">
            <Link
              to={`/account/${block.header.producer}`}
              className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors"
            >
              {formatHash(block.header.producer)}
            </Link>
            <CopyButton text={block.header.producer} />
          </span>
        </DetailRow>
      </div>

      {/* ─── Transactions ────────────────────────────────────── */}
      <TransactionSection
        txHashes={block.tx_hashes}
        blockHeight={blockHeight}
        txPage={txPage}
        setTxPage={setTxPage}
      />
    </div>
  );
}
