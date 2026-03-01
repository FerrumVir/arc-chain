import { useState, useEffect } from 'react';
import { callContract, getContractInfo } from '../api';
import type { ContractInfo, ContractCallResult } from '../types';
import Badge from './Badge';

interface Props {
  contractAddress: string;
}

export default function ContractInteraction({ contractAddress }: Props) {
  const [functionName, setFunctionName] = useState('');
  const [calldata, setCalldata] = useState('');
  const [callerAddress, setCallerAddress] = useState('');
  const [gasLimit, setGasLimit] = useState('1000000');
  const [result, setResult] = useState<ContractCallResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [contractInfo, setContractInfo] = useState<ContractInfo | null>(null);

  useEffect(() => {
    getContractInfo(contractAddress)
      .then(setContractInfo)
      .catch(() => {});
  }, [contractAddress]);

  const handleCall = async () => {
    if (!functionName) return;
    setLoading(true);
    setResult(null);
    try {
      const res = await callContract(
        contractAddress,
        functionName,
        calldata || undefined,
        callerAddress || undefined,
        parseInt(gasLimit) || undefined,
      );
      setResult(res);
    } catch (err) {
      setResult({
        success: false,
        error: err instanceof Error ? err.message : 'Call failed',
      });
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="border border-arc-border bg-arc-surface-raised p-6 space-y-4">
      <h2 className="text-xs uppercase tracking-widest text-arc-grey-600 mb-2">
        Contract Interaction (Read Only)
      </h2>

      {contractInfo && (
        <div className="text-xs text-arc-grey-500 space-y-1 border-b border-arc-border-subtle pb-3">
          <p>
            Bytecode: {contractInfo.bytecode_size.toLocaleString()} bytes
            {contractInfo.is_wasm && (
              <Badge variant="info" size="sm">WASM</Badge>
            )}
          </p>
          <p className="font-mono">
            Code hash: {contractInfo.code_hash.slice(0, 24)}...
          </p>
        </div>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
        <input
          value={functionName}
          onChange={(e) => setFunctionName(e.target.value)}
          placeholder="Function name (e.g., get_balance)"
          className="bg-arc-surface border border-arc-border px-3 py-2 text-sm text-arc-white placeholder:text-arc-grey-700"
        />
        <input
          value={calldata}
          onChange={(e) => setCalldata(e.target.value)}
          placeholder="Calldata (hex, optional)"
          className="bg-arc-surface border border-arc-border px-3 py-2 text-sm text-arc-white placeholder:text-arc-grey-700 font-mono"
        />
        <input
          value={callerAddress}
          onChange={(e) => setCallerAddress(e.target.value)}
          placeholder="Caller address (optional)"
          className="bg-arc-surface border border-arc-border px-3 py-2 text-sm text-arc-white placeholder:text-arc-grey-700 font-mono"
        />
        <input
          value={gasLimit}
          onChange={(e) => setGasLimit(e.target.value)}
          placeholder="Gas limit"
          className="bg-arc-surface border border-arc-border px-3 py-2 text-sm text-arc-white placeholder:text-arc-grey-700"
        />
      </div>

      <button
        onClick={handleCall}
        disabled={!functionName || loading}
        className="btn-arc text-xs disabled:opacity-40 disabled:cursor-not-allowed"
      >
        {loading ? 'Calling...' : 'Call (Read Only)'}
      </button>

      {result && (
        <div
          className={`p-4 border text-sm font-mono ${
            result.success
              ? 'border-arc-success/20 bg-arc-success/5 text-arc-success'
              : 'border-arc-error/20 bg-arc-error/5 text-arc-error'
          }`}
        >
          <pre className="whitespace-pre-wrap break-all text-xs">
            {JSON.stringify(result, null, 2)}
          </pre>
        </div>
      )}
    </div>
  );
}
