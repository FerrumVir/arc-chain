import { useState, useEffect } from 'react';
import { useParams, Link } from 'react-router-dom';
import { getTx, getTxProof, getFullTx } from '../api';
import type { TxReceipt, TxProof, FullTransaction, TransactionBody } from '../types';
import { formatHash } from '../utils';
import CopyButton from '../components/CopyButton';
import Badge from '../components/Badge';
import ContractInteraction from '../components/ContractInteraction';

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

function bytesToHex(bytes: number[]): string {
  return bytes.map((b) => b.toString(16).padStart(2, '0')).join('');
}

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

// ─── Type-specific body fields ─────────────────────────────────

function TransactionBodyDetails({ body }: { body: TransactionBody }) {
  switch (body.type) {
    case 'Transfer':
      return (
        <>
          <DetailRow label="To">
            <Link to={`/account/${body.to}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.to)}
            </Link>
            <CopyButton text={body.to} />
          </DetailRow>
          <DetailRow label="Amount">{body.amount.toLocaleString()} ARC</DetailRow>
          {body.amount_commitment && (
            <DetailRow label="Commitment">
              <span className="font-mono text-xs">{formatHash(body.amount_commitment)}</span>
            </DetailRow>
          )}
        </>
      );
    case 'Settle':
      return (
        <>
          <DetailRow label="Agent">
            <Link to={`/account/${body.agent_id}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.agent_id)}
            </Link>
          </DetailRow>
          <DetailRow label="Service Hash"><span className="font-mono text-xs">{formatHash(body.service_hash)}</span></DetailRow>
          <DetailRow label="Amount">{body.amount.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Usage Units">{body.usage_units.toLocaleString()}</DetailRow>
        </>
      );
    case 'Swap':
      return (
        <>
          <DetailRow label="Counterparty">
            <Link to={`/account/${body.counterparty}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.counterparty)}
            </Link>
          </DetailRow>
          <DetailRow label="Offer">{body.offer_amount.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Receive">{body.receive_amount.toLocaleString()} ARC</DetailRow>
        </>
      );
    case 'Escrow':
      return (
        <>
          <DetailRow label="Beneficiary">
            <Link to={`/account/${body.beneficiary}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.beneficiary)}
            </Link>
          </DetailRow>
          <DetailRow label="Amount">{body.amount.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Action">
            <Badge variant={body.is_create ? 'info' : 'success'}>{body.is_create ? 'Create' : 'Release'}</Badge>
          </DetailRow>
        </>
      );
    case 'Stake':
      return (
        <>
          <DetailRow label="Amount">{body.amount.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Action">
            <Badge variant={body.is_stake ? 'info' : 'warning'}>{body.is_stake ? 'Stake' : 'Unstake'}</Badge>
          </DetailRow>
          <DetailRow label="Validator">
            <Link to={`/account/${body.validator}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.validator)}
            </Link>
          </DetailRow>
        </>
      );
    case 'WasmCall':
      return (
        <>
          <DetailRow label="Contract">
            <Link to={`/account/${body.contract}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.contract)}
            </Link>
            <CopyButton text={body.contract} />
          </DetailRow>
          <DetailRow label="Function"><span className="font-mono text-sm">{body.function}</span></DetailRow>
          <DetailRow label="Calldata">
            <span className="font-mono text-xs">{body.calldata || '(empty)'}</span>
          </DetailRow>
          <DetailRow label="Value">{body.value.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Gas Limit">{body.gas_limit.toLocaleString()}</DetailRow>
        </>
      );
    case 'MultiSig':
      return (
        <>
          <DetailRow label="Threshold">{body.threshold}</DetailRow>
          <DetailRow label="Signers">{body.signers.length} addresses</DetailRow>
        </>
      );
    case 'DeployContract':
      return (
        <>
          <DetailRow label="Bytecode Size">{body.bytecode_size.toLocaleString()} bytes</DetailRow>
          <DetailRow label="Constructor Args">{body.constructor_args_size.toLocaleString()} bytes</DetailRow>
          <DetailRow label="State Rent">{body.state_rent_deposit.toLocaleString()} ARC</DetailRow>
        </>
      );
    case 'RegisterAgent':
      return (
        <>
          <DetailRow label="Agent Name">{body.agent_name}</DetailRow>
          <DetailRow label="Endpoint"><span className="font-mono text-xs">{body.endpoint}</span></DetailRow>
          <DetailRow label="Protocol"><span className="font-mono text-xs">{formatHash(body.protocol)}</span></DetailRow>
        </>
      );
    case 'JoinValidator':
      return (
        <>
          <DetailRow label="Public Key">
            <span className="font-mono text-xs">
              {Array.isArray(body.pubkey)
                ? '0x' + body.pubkey.map((b: number) => b.toString(16).padStart(2, '0')).join('')
                : String(body.pubkey)}
            </span>
          </DetailRow>
          <DetailRow label="Initial Stake">{body.initial_stake.toLocaleString()} ARC</DetailRow>
        </>
      );
    case 'LeaveValidator':
      return (
        <DetailRow label="Action">
          <Badge variant="warning">Leave Validator Set</Badge>
        </DetailRow>
      );
    case 'ClaimRewards':
      return (
        <DetailRow label="Action">
          <Badge variant="success">Claim Staking Rewards</Badge>
        </DetailRow>
      );
    case 'UpdateStake':
      return (
        <DetailRow label="New Stake">{body.new_stake.toLocaleString()} ARC</DetailRow>
      );
    case 'Governance':
      return (
        <>
          <DetailRow label="Proposal ID">{body.proposal_id}</DetailRow>
          <DetailRow label="Action">
            <Badge variant="info">{body.action}</Badge>
          </DetailRow>
        </>
      );
    case 'BridgeLock':
      return (
        <>
          <DetailRow label="Destination Chain">Chain #{body.destination_chain}</DetailRow>
          <DetailRow label="Destination Address">
            <span className="font-mono text-xs">
              {Array.isArray(body.destination_address)
                ? '0x' + body.destination_address.map((b: number) => b.toString(16).padStart(2, '0')).join('')
                : String(body.destination_address)}
            </span>
          </DetailRow>
          <DetailRow label="Amount">{body.amount.toLocaleString()} ARC</DetailRow>
        </>
      );
    case 'BridgeMint':
      return (
        <>
          <DetailRow label="Source Chain">Chain #{body.source_chain}</DetailRow>
          <DetailRow label="Source TX">
            <span className="font-mono text-xs">{formatHash(body.source_tx_hash)}</span>
          </DetailRow>
          <DetailRow label="Recipient">
            <Link to={`/account/${body.recipient}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.recipient)}
            </Link>
          </DetailRow>
          <DetailRow label="Amount">{body.amount.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Proof Size">{body.merkle_proof.length} bytes</DetailRow>
        </>
      );
    case 'BatchSettle':
      return (
        <>
          <DetailRow label="Entries">{body.entries.length} settlements</DetailRow>
          {body.entries.slice(0, 10).map((entry: { agent_id: string; service_hash: string; amount: number }, i: number) => (
            <DetailRow key={i} label={`Entry ${i + 1}`}>
              <span className="flex flex-col gap-1">
                <Link to={`/account/${entry.agent_id}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
                  {formatHash(entry.agent_id)}
                </Link>
                <span className="text-xs text-arc-grey-500">{entry.amount.toLocaleString()} ARC</span>
              </span>
            </DetailRow>
          ))}
          {body.entries.length > 10 && (
            <DetailRow label="">
              <span className="text-xs text-arc-grey-600">...and {body.entries.length - 10} more</span>
            </DetailRow>
          )}
        </>
      );
    case 'ChannelOpen':
      return (
        <>
          <DetailRow label="Channel ID">
            <span className="font-mono text-xs">{formatHash(body.channel_id)}</span>
          </DetailRow>
          <DetailRow label="Counterparty">
            <Link to={`/account/${body.counterparty}`} className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors">
              {formatHash(body.counterparty)}
            </Link>
          </DetailRow>
          <DetailRow label="Deposit">{body.deposit.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Timeout">{body.timeout_blocks.toLocaleString()} blocks</DetailRow>
        </>
      );
    case 'ChannelClose':
      return (
        <>
          <DetailRow label="Channel ID">
            <span className="font-mono text-xs">{formatHash(body.channel_id)}</span>
          </DetailRow>
          <DetailRow label="Opener Balance">{body.opener_balance.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Counterparty Balance">{body.counterparty_balance.toLocaleString()} ARC</DetailRow>
          <DetailRow label="State Nonce">{body.state_nonce}</DetailRow>
        </>
      );
    case 'ChannelDispute':
      return (
        <>
          <DetailRow label="Channel ID">
            <span className="font-mono text-xs">{formatHash(body.channel_id)}</span>
          </DetailRow>
          <DetailRow label="Opener Balance">{body.opener_balance.toLocaleString()} ARC</DetailRow>
          <DetailRow label="Counterparty Balance">{body.counterparty_balance.toLocaleString()} ARC</DetailRow>
          <DetailRow label="State Nonce">{body.state_nonce}</DetailRow>
          <DetailRow label="Challenge Period">{body.challenge_period.toLocaleString()} blocks</DetailRow>
        </>
      );
    case 'ShardProof':
      return (
        <>
          <DetailRow label="Shard ID">{body.shard_id}</DetailRow>
          <DetailRow label="Block Height">{body.block_height.toLocaleString()}</DetailRow>
          <DetailRow label="Block Hash">
            <span className="font-mono text-xs">{formatHash(body.block_hash)}</span>
          </DetailRow>
          <DetailRow label="Prev State Root">
            <span className="font-mono text-xs">{formatHash(body.prev_state_root)}</span>
          </DetailRow>
          <DetailRow label="Post State Root">
            <span className="font-mono text-xs">{formatHash(body.post_state_root)}</span>
          </DetailRow>
          <DetailRow label="TX Count">{body.tx_count.toLocaleString()}</DetailRow>
          <DetailRow label="Proof Size">{body.proof_data.length.toLocaleString()} bytes</DetailRow>
        </>
      );
    case 'InferenceAttestation':
      return (
        <>
          <DetailRow label="Type">
            <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-purple-100 text-purple-800">
              AI Inference Attestation
            </span>
          </DetailRow>
          <DetailRow label="Model">
            <span className="font-mono text-xs break-all">{body.model_name || body.model_id}</span>
          </DetailRow>
          {body.input_text && (
            <DetailRow label="Query">
              <div className="bg-arc-surface p-3 rounded border border-arc-border-subtle">
                <span className="text-sm text-arc-white whitespace-pre-wrap">{body.input_text}</span>
              </div>
            </DetailRow>
          )}
          {body.output_text && (
            <DetailRow label="Response">
              <div className="bg-arc-surface p-3 rounded border border-arc-border-subtle">
                <span className="text-sm text-arc-white whitespace-pre-wrap">{body.output_text}</span>
              </div>
            </DetailRow>
          )}
          <DetailRow label="Input Hash">
            <span className="font-mono text-xs break-all">{body.input_hash}</span>
          </DetailRow>
          <DetailRow label="Output Hash">
            <span className="font-mono text-xs break-all">{body.output_hash}</span>
          </DetailRow>
          {body.tokens_generated > 0 && (
            <DetailRow label="Performance">
              <span className="text-sm">
                {body.tokens_generated} tokens in {body.inference_ms}ms
                ({body.ms_per_token}ms/token)
              </span>
            </DetailRow>
          )}
          <DetailRow label="Verification">
            <span className={`inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium ${
              body.verified ? 'bg-green-100 text-green-800' : 'bg-yellow-100 text-yellow-800'
            }`}>
              {body.verified ? 'Verified by validators' : 'Pending verification'}
            </span>
          </DetailRow>
          <DetailRow label="Engine">
            <span className="text-xs text-arc-grey-400">
              {body.engine || 'INT8 integer (cross-platform deterministic)'}
            </span>
          </DetailRow>
          <DetailRow label="Challenge Period">{body.challenge_period} blocks</DetailRow>
          <DetailRow label="Bond">{body.bond.toLocaleString()} ARC</DetailRow>
        </>
      );
    case 'InferenceChallenge':
      return (
        <>
          <DetailRow label="Type">
            <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-red-100 text-red-800">
              AI Inference Challenge (Fraud Proof)
            </span>
          </DetailRow>
          <DetailRow label="Attestation Hash">
            <span className="font-mono text-xs break-all">{body.attestation_hash}</span>
          </DetailRow>
          <DetailRow label="Challenger Output Hash">
            <span className="font-mono text-xs break-all">{body.challenger_output_hash}</span>
          </DetailRow>
          <DetailRow label="Challenger Bond">{body.challenger_bond.toLocaleString()} ARC</DetailRow>
        </>
      );
    case 'InferenceRegister':
      return (
        <>
          <DetailRow label="Type">
            <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800">
              Inference Provider Registration
            </span>
          </DetailRow>
          <DetailRow label="Hardware Tier">Tier {body.tier}</DetailRow>
          <DetailRow label="Stake Bond">{body.stake_bond.toLocaleString()} ARC</DetailRow>
        </>
      );
    default:
      return null;
  }
}

// ─── Main component ────────────────────────────────────────────

type Tab = 'receipt' | 'raw' | 'contract';

export default function Transaction() {
  const { hash } = useParams<{ hash: string }>();
  const [receipt, setReceipt] = useState<TxReceipt | null>(null);
  const [proof, setProof] = useState<TxProof | null>(null);
  const [fullTx, setFullTx] = useState<FullTransaction | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [showProof, setShowProof] = useState(false);
  const [activeTab, setActiveTab] = useState<Tab>('receipt');

  useEffect(() => {
    if (!hash) return;
    document.title = `Tx ${hash.slice(0, 12)}... — ARC scan`;
    setLoading(true);
    setActiveTab('receipt');

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

    // Also fetch full transaction (may 404 for old blocks)
    getFullTx(hash)
      .then(setFullTx)
      .catch(() => {});
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
  const isWasmCall = fullTx?.body?.type === 'WasmCall';

  const tabs: { key: Tab; label: string }[] = [
    { key: 'receipt', label: 'Receipt' },
    { key: 'raw', label: 'Raw Data' },
    ...(isWasmCall ? [{ key: 'contract' as Tab, label: 'Contract' }] : []),
  ];

  return (
    <div className="space-y-6">
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

      {/* ─── Tabs ────────────────────────────────────────────── */}
      <div className="flex border-b border-arc-border">
        {tabs.map((tab) => (
          <button
            key={tab.key}
            onClick={() => setActiveTab(tab.key)}
            className={`px-4 py-2.5 text-sm font-medium transition-colors border-b-2 -mb-px ${
              activeTab === tab.key
                ? 'text-arc-aquarius border-arc-aquarius'
                : 'text-arc-grey-600 border-transparent hover:text-arc-white'
            }`}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* ─── Receipt Tab ─────────────────────────────────────── */}
      {activeTab === 'receipt' && (
        <>
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

            {receipt.value_commitment ? (
              <DetailRow label="Value Commitment">
                <span className="flex items-center gap-1 font-mono text-xs">
                  {formatHash(receipt.value_commitment)}
                  <CopyButton text={receipt.value_commitment} />
                </span>
              </DetailRow>
            ) : (
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

          {/* ─── Merkle Proof Viewer ─────────────────────────── */}
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
        </>
      )}

      {/* ─── Raw Data Tab ────────────────────────────────────── */}
      {activeTab === 'raw' && (
        <div className="border border-arc-border bg-arc-surface-raised p-6">
          {fullTx ? (
            <>
              <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-4">
                Transaction Data
              </h2>
              <DetailRow label="Type">
                <Badge variant="info">{fullTx.body.type}</Badge>
              </DetailRow>
              <DetailRow label="From">
                <span className="flex items-center gap-1">
                  <Link
                    to={`/account/${fullTx.from}`}
                    className="font-mono text-xs text-arc-aquarius hover:text-arc-blue transition-colors"
                  >
                    {formatHash(fullTx.from)}
                  </Link>
                  <CopyButton text={fullTx.from} />
                </span>
              </DetailRow>
              <DetailRow label="Nonce">{fullTx.nonce}</DetailRow>
              <DetailRow label="Fee">{fullTx.fee.toLocaleString()} ARC</DetailRow>
              <DetailRow label="Gas Limit">{fullTx.gas_limit.toLocaleString()}</DetailRow>

              <div className="mt-4 pt-2 border-t border-arc-border-subtle">
                <p className="text-xs uppercase tracking-widest text-arc-grey-600 mb-3">
                  {fullTx.body.type} Body
                </p>
                <TransactionBodyDetails body={fullTx.body} />
              </div>
            </>
          ) : (
            <div className="text-sm text-arc-grey-600">
              Raw transaction data not available for this transaction.
              <br />
              <span className="text-xs text-arc-grey-700 mt-1 block">
                Only transactions processed after the latest node update include full body data.
              </span>
            </div>
          )}
        </div>
      )}

      {/* ─── Contract Tab ────────────────────────────────────── */}
      {activeTab === 'contract' && isWasmCall && fullTx?.body.type === 'WasmCall' && (
        <ContractInteraction contractAddress={fullTx.body.contract} />
      )}
    </div>
  );
}
