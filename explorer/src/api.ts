import type {
  HealthResponse,
  InfoResponse,
  NodeInfoResponse,
  StatsResponse,
  BlocksResponse,
  BlockDetail,
  TxReceipt,
  TxProof,
  FullTransaction,
  ContractInfo,
  ContractCallResult,
  AccountInfo,
  AccountTxsResponse,
  ValidatorsResponse,
  FaucetStatus,
  FaucetClaimResponse,
  AgentsResponse,
  AgentAction,
} from './types';

const BASE_URL = import.meta.env.VITE_API_URL || 'http://localhost:9090';

class ApiError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

async function request<T>(path: string): Promise<T> {
  const url = `${BASE_URL}${path}`;
  const res = await fetch(url);
  if (!res.ok) {
    throw new ApiError(
      `API request failed: ${res.status} ${res.statusText}`,
      res.status
    );
  }
  return res.json() as Promise<T>;
}

// ─── Health & Info ───────────────────────────────────────────────

export function getHealth(): Promise<HealthResponse> {
  return request<HealthResponse>('/health');
}

export function getInfo(): Promise<InfoResponse> {
  return request<InfoResponse>('/info');
}

export function getNodeInfo(): Promise<NodeInfoResponse> {
  return request<NodeInfoResponse>('/node/info');
}

export function getStats(): Promise<StatsResponse> {
  return request<StatsResponse>('/stats');
}

// ─── Blocks ──────────────────────────────────────────────────────

export function getBlocks(
  from: number = 0,
  to: number = 100,
  limit: number = 20
): Promise<BlocksResponse> {
  return request<BlocksResponse>(
    `/blocks?from=${from}&to=${to}&limit=${limit}`
  );
}

export function getBlock(height: number): Promise<BlockDetail> {
  return request<BlockDetail>(`/block/${height}`);
}

// ─── Transactions ────────────────────────────────────────────────

export function getTx(hash: string): Promise<TxReceipt> {
  return request<TxReceipt>(`/tx/${hash}`);
}

export function getTxProof(hash: string): Promise<TxProof> {
  return request<TxProof>(`/tx/${hash}/proof`);
}

// ─── Accounts ────────────────────────────────────────────────────

export function getAccount(address: string): Promise<AccountInfo> {
  return request<AccountInfo>(`/account/${address}`);
}

export function getAccountTxs(address: string): Promise<AccountTxsResponse> {
  return request<AccountTxsResponse>(`/account/${address}/txs`);
}

// ─── Full Transaction ────────────────────────────────────────────

export function getFullTx(hash: string): Promise<FullTransaction> {
  return request<FullTransaction>(`/tx/${hash}/full`);
}

// ─── Contracts ──────────────────────────────────────────────────

export function getContractInfo(address: string): Promise<ContractInfo> {
  return request<ContractInfo>(`/contract/${address}`);
}

export async function callContract(
  address: string,
  functionName: string,
  calldata?: string,
  from?: string,
  gasLimit?: number,
): Promise<ContractCallResult> {
  const url = `${BASE_URL}/contract/${address}/call`;
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      function: functionName,
      calldata,
      from,
      gas_limit: gasLimit,
    }),
  });
  if (!res.ok) {
    throw new ApiError(`Contract call failed: ${res.status}`, res.status);
  }
  return res.json() as Promise<ContractCallResult>;
}

// ─── Validators ─────────────────────────────────────────────────

export function getValidators(): Promise<ValidatorsResponse> {
  return request<ValidatorsResponse>('/validators');
}

// ─── Agents (Synths) ────────────────────────────────────────────

export function fetchAgents(): Promise<AgentsResponse> {
  return request<AgentsResponse>('/agents');
}

export async function fetchAgentActions(): Promise<AgentAction[]> {
  try {
    return await request<AgentAction[]>('/agents/actions');
  } catch {
    // Endpoint may not exist yet — return simulated data for demo
    return [];
  }
}

// ─── Faucet ────────────────────────────────────────────────────

const FAUCET_URL = import.meta.env.VITE_FAUCET_URL || 'http://localhost:3001';

export function getFaucetStatus(): Promise<FaucetStatus> {
  return faucetRequest<FaucetStatus>('/status');
}

export async function claimFaucetTokens(address: string): Promise<FaucetClaimResponse> {
  const url = `${FAUCET_URL}/claim`;
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ address }),
  });
  const data = await res.json();
  if (!res.ok) {
    throw new ApiError(
      data.error || `Claim failed: ${res.status}`,
      res.status
    );
  }
  return data as FaucetClaimResponse;
}

async function faucetRequest<T>(path: string): Promise<T> {
  const url = `${FAUCET_URL}${path}`;
  const res = await fetch(url);
  if (!res.ok) {
    throw new ApiError(
      `Faucet request failed: ${res.status} ${res.statusText}`,
      res.status
    );
  }
  return res.json() as Promise<T>;
}

export { ApiError };
