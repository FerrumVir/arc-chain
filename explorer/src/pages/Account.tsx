import { useState, useEffect, useMemo } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getAccount, getAccountTxs } from '../api';
import type { AccountInfo, AccountTxsResponse } from '../types';
import { formatHash, formatNumber } from '../utils';
import CopyButton from '../components/CopyButton';
import TxTable from '../components/TxTable';

const TX_PAGE_SIZE = 25;

interface DetailRowProps {
  label: string;
  children: React.ReactNode;
}

function DetailRow({ label, children }: DetailRowProps) {
  return (
    <div className="flex flex-col sm:flex-row sm:items-start border-b border-arc-border-subtle py-3.5 gap-1 sm:gap-0">
      <span className="text-xs uppercase tracking-widest text-arc-grey-600 sm:w-36 shrink-0">
        {label}
      </span>
      <span className="text-sm text-arc-white break-all flex-1">
        {children}
      </span>
    </div>
  );
}

export default function Account() {
  const { address } = useParams<{ address: string }>();
  const [account, setAccount] = useState<AccountInfo | null>(null);
  const [txData, setTxData] = useState<AccountTxsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [txPage, setTxPage] = useState(0);

  useEffect(() => {
    if (!address) return;
    document.title = `Account ${address.slice(0, 12)}... — ARC Explorer`;
    setLoading(true);
    setTxPage(0);

    Promise.all([getAccount(address), getAccountTxs(address)])
      .then(([accountData, txsData]) => {
        setAccount(accountData);
        setTxData(txsData);
        setError('');
      })
      .catch((err) => {
        setError(
          err instanceof Error ? err.message : 'Failed to load account'
        );
      })
      .finally(() => setLoading(false));
  }, [address]);

  const totalTxPages = Math.max(1, Math.ceil((txData?.tx_hashes.length ?? 0) / TX_PAGE_SIZE));
  const paginatedHashes = useMemo(
    () =>
      (txData?.tx_hashes ?? []).slice(
        txPage * TX_PAGE_SIZE,
        (txPage + 1) * TX_PAGE_SIZE
      ),
    [txData, txPage]
  );

  if (loading) {
    return (
      <div className="space-y-6">
        <div className="skeleton h-8 w-48" />
        <div className="border border-arc-border bg-arc-surface-raised p-6 space-y-4">
          {Array.from({ length: 4 }).map((_, i) => (
            <div key={i} className="flex gap-4">
              <div className="skeleton h-4 w-28" />
              <div className="skeleton h-4 w-48" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="space-y-4">
        <h1 className="text-2xl font-medium text-arc-white">
          Account Not Found
        </h1>
        <div className="bg-arc-error/5 border border-arc-error/20 px-4 py-3 text-sm text-arc-error">
          {error}
        </div>
        <Link to="/" className="btn-arc-outline text-xs">
          Back to home
        </Link>
      </div>
    );
  }

  if (!account || !address) return null;

  const txCount = txData?.tx_hashes.length ?? 0;

  return (
    <div className="space-y-8">
      {/* ─── Header ──────────────────────────────────────────── */}
      <div>
        <h1 className="text-2xl font-medium tracking-tight text-arc-white mb-2">
          Account
        </h1>
        <div className="flex items-center gap-2">
          <span className="font-mono text-xs text-arc-grey-500 break-all">
            {formatHash(address)}
          </span>
          <CopyButton text={address} />
        </div>
      </div>

      {/* ─── Account Details ─────────────────────────────────── */}
      <div className="border border-arc-border bg-arc-surface-raised p-6">
        <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
          Overview
        </h2>

        <DetailRow label="Balance">
          <span className="text-lg font-medium text-arc-white">
            {formatNumber(account.balance)}
          </span>
          <span className="text-sm text-arc-grey-600 ml-2">ARC</span>
        </DetailRow>

        <DetailRow label="Nonce">
          {account.nonce}
        </DetailRow>

        <DetailRow label="Transactions">
          {txData ? formatNumber(txData.tx_count) : '—'}
        </DetailRow>
      </div>

      {/* ─── Transaction History ─────────────────────────────── */}
      {txData && txData.tx_hashes.length > 0 && (
        <section>
          <h2 className="text-lg font-medium text-arc-white mb-4">
            Transaction History
            <span className="text-arc-grey-600 text-sm ml-2">
              ({txData.tx_count})
            </span>
          </h2>
          <div className="border border-arc-border bg-arc-surface">
            <TxTable txHashes={paginatedHashes} />
          </div>
          {txCount > TX_PAGE_SIZE && (
            <div className="flex items-center justify-between text-sm mt-4">
              <p className="text-arc-grey-600">
                Showing {txPage * TX_PAGE_SIZE + 1}–
                {Math.min((txPage + 1) * TX_PAGE_SIZE, txCount)} of{' '}
                {txCount.toLocaleString()}
              </p>
              <div className="flex gap-2">
                <button
                  onClick={() => setTxPage((p) => p - 1)}
                  disabled={txPage === 0}
                  className={`btn-arc-outline text-xs px-4 py-2 ${
                    txPage === 0 ? 'opacity-30 cursor-not-allowed' : ''
                  }`}
                >
                  Previous
                </button>
                <button
                  onClick={() => setTxPage((p) => p + 1)}
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
      )}
    </div>
  );
}
