import type {
  HealthResponse,
  InfoResponse,
  NodeInfoResponse,
  StatsResponse,
  BlocksResponse,
  BlockDetail,
  TxReceipt,
  TxProof,
  AccountInfo,
  AccountTxsResponse,
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

export { ApiError };
