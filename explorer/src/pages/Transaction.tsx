import { useState, useEffect } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getTx, getTxProof } from '../api';
import type { TxReceipt, TxProof } from '../types';
import { formatHash } from '../utils';
import CopyButton from '../components/CopyButton';
import Badge from '../components/Badge';

interface DetailRowProps {
  label: string;
  children: React.ReactNode;
}

function DetailRow({ label, children }: DetailRowProps) {
  return (
    <div className="flex flex-col sm:flex-row sm:items-start border-b border-arc-border-subtle py-3.5 gap-1 sm:gap-0">
      <span className="text-xs uppercase tracking-widest text-arc-grey-600 sm:w-44 shrink-0">
        {label}
      </span>
      <span className="text-sm text-arc-white break-all flex-1">
        {children}
      </span>
    </div>
  );
}

/**
 * Convert a byte array to a hex string
 */
function bytesToHex(bytes: number[]): string {
  return bytes.map((b) => b.toString(16).padStart(2, '0')).join('');
}

/**
 * Format the inclusion proof for display.
 * Could be a hex string, byte array, or null.
 */
function formatProofValue(
  proof: string | number[] | null
): { display: string; raw: string } | null {
  if (proof === null || proof === undefined) return null;
  if (typeof proof === 'string') {
    return { display: formatHash(proof), raw: proof };
  }
  if (Array.isArray(proof)) {
    const hex = bytesToHex(proof);
    return { display: `${hex.slice(0, 16)}...${hex.slice(-16)}`, raw: hex };
  }
  return null;
}

export default function Transaction() {
  const { hash } = useParams<{ hash: string }>();
  const [receipt, setReceipt] = useState<TxReceipt | null>(null);
  const [proof, setProof] = useState<TxProof | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [showProof, setShowProof] = useState(false);

  useEffect(() => {
    if (!hash) return;
    document.title = `Tx ${hash.slice(0, 12)}... — ARC Explorer`;
    setLoading(true);

    getTx(hash)
      .then((data) => {
        setReceipt(data);
        setError('');
      })
      .catch((err) => {
        setError(
          err instanceof Error ? err.message : 'Failed to load transaction'
        );
      })
      .finally(() => setLoading(false));
  }, [hash]);

  const loadProof = async () => {
    if (!hash || proof) {
      setShowProof(!showProof);
      return;
    }
    try {
      const data = await getTxProof(hash);
      setProof(data);
      setShowProof(true);
    } catch {
      // Proof may not be available for all transactions
      setShowProof(false);
    }
  };

  if (loading) {
    return (
      <div className="space-y-6">
        <div className="skeleton h-8 w-64" />
        <div className="border border-arc-border bg-arc-surface-raised p-6 space-y-4">
          {Array.from({ length: 6 }).map((_, i) => (
            <div key={i} className="flex gap-4">
              <div className="skeleton h-4 w-36" />
              <div className="skeleton h-4 w-80" />
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
          Transaction Not Found
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

  if (!receipt) return null;

  const proofData = formatProofValue(receipt.inclusion_proof);

  return (
    <div className="space-y-8">
      {/* ─── Header ──────────────────────────────────────────── */}
      <div>
        <h1 className="text-2xl font-medium tracking-tight text-arc-white mb-2">
          Transaction
        </h1>
        <div className="flex items-center gap-2">
          <span className="font-mono text-xs text-arc-grey-500 break-all">
            {formatHash(receipt.tx_hash)}
          </span>
          <CopyButton text={receipt.tx_hash} />
        </div>
      </div>

      {/* ─── Receipt Details ─────────────────────────────────── */}
      <div className="border border-arc-border bg-arc-surface-raised p-6">
        <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
          Receipt
        </h2>

        <DetailRow label="Status">
          {receipt.success ? (
            <Badge variant="success">Success</Badge>
          ) : (
            <Badge variant="error">Failed</Badge>
          )}
        </DetailRow>

        <DetailRow label="Block">
          <Link
            to={`/block/${receipt.block_height}`}
            className="text-arc-aquarius hover:text-arc-blue transition-colors font-medium"
          >
            #{receipt.block_height}
          </Link>
        </DetailRow>

        <DetailRow label="Block Hash">
          <span className="flex items-center gap-1">
            <Link
              to={`/block/${receipt.block_height}`}
              className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors"
            >
              {formatHash(receipt.block_hash)}
            </Link>
            <CopyButton text={receipt.block_hash} />
          </span>
        </DetailRow>

        <DetailRow label="Index">
          {receipt.index}
        </DetailRow>

        <DetailRow label="Gas Used">
          <span className="font-mono">{receipt.gas_used.toLocaleString()}</span>
        </DetailRow>

        {receipt.value_commitment && (
          <DetailRow label="Value Commitment">
            <span className="flex items-center gap-1 font-mono text-xs">
              {formatHash(receipt.value_commitment)}
              <CopyButton text={receipt.value_commitment} />
            </span>
          </DetailRow>
        )}

        {!receipt.value_commitment && (
          <DetailRow label="Value Commitment">
            <span className="text-arc-grey-700">None (confidential)</span>
          </DetailRow>
        )}

        {proofData && (
          <DetailRow label="Inclusion Proof">
            <span className="flex items-center gap-1 font-mono text-xs">
              <span className="text-arc-grey-400">
                {proofData.display}
              </span>
              <CopyButton text={proofData.raw} />
            </span>
            <span className="text-xs text-arc-grey-700 mt-1 block">
              {Array.isArray(receipt.inclusion_proof)
                ? `${receipt.inclusion_proof.length} bytes`
                : ''}
            </span>
          </DetailRow>
        )}
      </div>

      {/* ─── Merkle Proof Viewer ─────────────────────────────── */}
      <section>
        <button
          onClick={loadProof}
          className="btn-arc-outline text-xs mb-4"
        >
          {showProof ? 'Hide' : 'Show'} Merkle Proof
        </button>

        {showProof && proof && (
          <div className="border border-arc-border bg-arc-surface-raised p-6 space-y-4">
            <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-2">
              Merkle Proof
            </h2>

            <DetailRow label="Merkle Root">
              <span className="font-mono text-xs">
                {formatHash(proof.merkle_root)}
              </span>
            </DetailRow>

            <DetailRow label="Index">
              {proof.index}
            </DetailRow>

            <DetailRow label="Verified">
              {proof.verified ? (
                <Badge variant="success" size="md">Verified</Badge>
              ) : (
                <Badge variant="error" size="md">Unverified</Badge>
              )}
            </DetailRow>

            {proof.proof_nodes && proof.proof_nodes.length > 0 && (
              <div className="mt-4">
                <p className="text-xs uppercase tracking-widest text-arc-grey-600 mb-3">
                  Proof Nodes ({proof.proof_nodes.length})
                </p>
                <div className="space-y-1">
                  {proof.proof_nodes.map((node, i) => (
                    <div
                      key={i}
                      className="flex items-center gap-3 py-2 border-b border-arc-border-subtle"
                    >
                      <span className="text-xs text-arc-grey-700 w-8 text-right shrink-0">
                        {i}
                      </span>
                      <span className="font-mono text-xs text-arc-grey-400 break-all">
                        {formatHash(node)}
                      </span>
                      <CopyButton text={node} />
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        )}
      </section>
    </div>
  );
}
