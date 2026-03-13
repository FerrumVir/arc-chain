import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import { formatHash } from '../utils';
import { getFaucetStatus, claimFaucetTokens, ApiError } from '../api';
import type { FaucetStatus } from '../types';

interface ClaimResult {
  tx_hash?: string;
  amount?: number;
  message?: string;
  error?: string;
}

export default function Faucet() {
  const [address, setAddress] = useState('');
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<ClaimResult | null>(null);
  const [status, setStatus] = useState<FaucetStatus | null>(null);
  const [recentClaims, setRecentClaims] = useState<
    Array<{ address: string; tx_hash: string; time: string }>
  >([]);

  useEffect(() => {
    document.title = 'ARC scan — Testnet Faucet';
    fetchStatus();
  }, []);

  const fetchStatus = async () => {
    try {
      const data = await getFaucetStatus();
      setStatus(data);
    } catch {
      // Faucet may not be running
    }
  };

  const handleClaim = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      const trimmed = address.trim().toLowerCase().replace(/^0x/, '');

      // Client-side validation
      if (trimmed.length !== 64 || !/^[0-9a-f]{64}$/.test(trimmed)) {
        setResult({ error: 'Invalid address. Must be 64 hex characters (without 0x prefix).' });
        return;
      }

      setLoading(true);
      setResult(null);

      try {
        const data = await claimFaucetTokens(trimmed);
        setResult({ tx_hash: data.tx_hash, amount: data.amount, message: data.message });

        setRecentClaims((prev) => [
          {
            address: trimmed,
            tx_hash: data.tx_hash,
            time: new Date().toLocaleTimeString(),
          },
          ...prev.slice(0, 9),
        ]);
        fetchStatus();
      } catch (err) {
        const msg = err instanceof ApiError ? err.message : 'Failed to connect to faucet. Is it running?';
        setResult({ error: msg });
      } finally {
        setLoading(false);
      }
    },
    [address]
  );

  return (
    <div className="space-y-8">
      {/* Header */}
      <div className="space-y-2">
        <h1 className="text-3xl font-medium tracking-tight">
          <span className="text-gradient">Testnet</span>{' '}
          <span className="text-arc-white">Faucet</span>
        </h1>
        <p className="text-sm text-arc-grey-600">
          Request test ARC tokens for development. Rate limited to 1 claim per
          address per hour.
        </p>
      </div>

      {/* Claim Form */}
      <div className="border border-arc-border bg-arc-surface p-6">
        <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
          Request Tokens
        </h2>
        <form onSubmit={handleClaim} className="space-y-4">
          <div>
            <label
              htmlFor="address"
              className="block text-xs text-arc-grey-600 mb-1.5"
            >
              Wallet Address
            </label>
            <input
              id="address"
              type="text"
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="64-character hex address (e.g. a1b2c3d4...)"
              maxLength={66}
              className="w-full px-3 py-2.5 bg-arc-black border border-arc-border text-arc-white
                         font-mono text-sm placeholder-arc-grey-700 outline-none
                         focus:border-arc-aquarius transition-colors duration-150"
              required
            />
            <p className="text-[10px] text-arc-grey-700 mt-1">
              Enter your 64-character hex address. You can find this with <code className="text-arc-grey-500">arc-cli wallet address</code>
            </p>
          </div>
          <button
            type="submit"
            disabled={loading}
            className="w-full px-4 py-2.5 bg-arc-white text-arc-black border border-arc-black
                       text-sm font-medium cursor-pointer transition-all duration-150
                       hover:bg-arc-black hover:text-arc-white hover:border-arc-white
                       disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {loading ? 'Sending...' : `Request ${status?.claim_amount?.toLocaleString() ?? '10,000'} ARC`}
          </button>
        </form>

        {/* Result */}
        {result && (
          <div
            className={`mt-4 p-4 border text-sm animate-fade-in ${
              result.error
                ? 'border-arc-error/30 bg-arc-error/5'
                : 'border-arc-success/30 bg-arc-success/5'
            }`}
          >
            {result.error ? (
              <p className="text-arc-error">{result.error}</p>
            ) : (
              <div className="space-y-2">
                <p className="text-arc-success">{result.message}</p>
                {result.tx_hash && (
                  <div>
                    <p className="text-xs text-arc-grey-600 mb-0.5">
                      Transaction Hash
                    </p>
                    <Link
                      to={`/tx/${result.tx_hash}`}
                      className="font-mono text-xs text-arc-aquarius hover:text-arc-blue
                                 transition-colors break-all"
                    >
                      {result.tx_hash}
                    </Link>
                  </div>
                )}
              </div>
            )}
          </div>
        )}
      </div>

      {/* How to use */}
      <div className="border border-arc-border bg-arc-surface-raised p-6">
        <h3 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
          How to Get Tokens
        </h3>
        <div className="space-y-4 text-sm text-arc-grey-500">
          <div className="flex gap-3">
            <span className="w-6 h-6 flex items-center justify-center bg-arc-aquarius/10 text-arc-aquarius text-xs font-medium shrink-0">1</span>
            <div>
              <p className="text-arc-white mb-1">Install the CLI</p>
              <code className="text-xs text-arc-grey-400 bg-arc-surface px-2 py-1 block">
                cargo install --path crates/arc-cli
              </code>
            </div>
          </div>
          <div className="flex gap-3">
            <span className="w-6 h-6 flex items-center justify-center bg-arc-aquarius/10 text-arc-aquarius text-xs font-medium shrink-0">2</span>
            <div>
              <p className="text-arc-white mb-1">Generate a wallet</p>
              <code className="text-xs text-arc-grey-400 bg-arc-surface px-2 py-1 block">
                arc wallet new
              </code>
            </div>
          </div>
          <div className="flex gap-3">
            <span className="w-6 h-6 flex items-center justify-center bg-arc-aquarius/10 text-arc-aquarius text-xs font-medium shrink-0">3</span>
            <div>
              <p className="text-arc-white mb-1">Paste your address above and claim tokens</p>
              <p className="text-xs text-arc-grey-600">Tokens will arrive in the next block (~2s)</p>
            </div>
          </div>
        </div>
      </div>

      {/* Faucet Info */}
      {status && (
        <div className="border border-arc-border bg-arc-surface-raised p-5">
          <h3 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-3">
            Faucet Status
          </h3>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-sm">
            <div>
              <p className="text-arc-grey-600 text-xs mb-1">Claim Amount</p>
              <p className="text-arc-white font-medium">
                {status.claim_amount.toLocaleString()} ARC
              </p>
            </div>
            <div>
              <p className="text-arc-grey-600 text-xs mb-1">Claims Today</p>
              <p className="text-arc-white font-medium">{status.claims_today}</p>
            </div>
            <div>
              <p className="text-arc-grey-600 text-xs mb-1">Rate Limit</p>
              <p className="text-arc-white font-medium">1 per hour</p>
            </div>
            <div>
              <p className="text-arc-grey-600 text-xs mb-1">Faucet Address</p>
              <p className="text-arc-white font-mono text-xs">
                {formatHash(status.address)}
              </p>
            </div>
          </div>
        </div>
      )}

      {/* Recent Claims */}
      {recentClaims.length > 0 && (
        <section>
          <h2 className="text-lg font-medium text-arc-white mb-4">
            Recent Claims
          </h2>
          <div className="border border-arc-border bg-arc-surface">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-arc-border">
                  <th className="px-4 py-3 text-left text-xs font-medium text-arc-grey-600 uppercase tracking-wider">
                    Time
                  </th>
                  <th className="px-4 py-3 text-left text-xs font-medium text-arc-grey-600 uppercase tracking-wider">
                    Address
                  </th>
                  <th className="px-4 py-3 text-left text-xs font-medium text-arc-grey-600 uppercase tracking-wider">
                    Tx Hash
                  </th>
                </tr>
              </thead>
              <tbody>
                {recentClaims.map((claim, i) => (
                  <tr
                    key={i}
                    className="border-b border-arc-border-subtle last:border-b-0 table-row-hover"
                  >
                    <td className="px-4 py-3 text-arc-grey-500 whitespace-nowrap">
                      {claim.time}
                    </td>
                    <td className="px-4 py-3 font-mono text-xs text-arc-white">
                      <Link
                        to={`/account/${claim.address}`}
                        className="hover:text-arc-aquarius transition-colors"
                      >
                        {formatHash(claim.address)}
                      </Link>
                    </td>
                    <td className="px-4 py-3 font-mono text-xs">
                      <Link
                        to={`/tx/${claim.tx_hash}`}
                        className="text-arc-aquarius hover:text-arc-blue transition-colors"
                      >
                        {formatHash(claim.tx_hash)}
                      </Link>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      )}
    </div>
  );
}
