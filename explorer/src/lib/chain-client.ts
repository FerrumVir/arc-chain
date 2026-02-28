// ---------------------------------------------------------------------------
// ARC Chain Client — live RPC with demo fallback
// ---------------------------------------------------------------------------

export type DataSource = "live" | "demo";

export interface ChainResponse<T> {
  data: T;
  source: DataSource;
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

export interface Block {
  height: number;
  hash: string;
  parentHash: string;
  timestamp: number;
  txCount: number;
  merkleRoot: string;
  size: number;
}

export interface TxDetail {
  hash: string;
  blockHeight: number;
  from: string;
  to: string;
  amount: bigint | string;
  txType: string;
  nonce: number;
  timestamp: number;
  status: "confirmed" | "pending";
  preHashHex?: string;
}

export interface TxProof {
  txHash: string;
  preHashHex: string;
  blake3Domain: string;
  merkleProof: {
    leaf: string;
    index: number;
    siblings: { hash: string; isLeft: boolean }[];
    root: string;
  };
  blockHeight: number;
  pedersenCommitment: string;
}

export interface Stats {
  tps: number;
  blockHeight: number;
  totalTxs: number;
  avgBlockTimeMs: number;
  nodeCount: number;
}

// ---------------------------------------------------------------------------
// Mock-data helpers (self-contained, no external imports)
// ---------------------------------------------------------------------------

function randomHex(bytes: number): string {
  const chars = "0123456789abcdef";
  let out = "0x";
  for (let i = 0; i < bytes * 2; i++) {
    out += chars[Math.floor(Math.random() * 16)];
  }
  return out;
}

function randomAddress(): string {
  return randomHex(20);
}

function mockBlock(height: number): Block {
  return {
    height,
    hash: randomHex(32),
    parentHash: randomHex(32),
    timestamp: Date.now() - (148_293 - height) * 400,
    txCount: 800 + Math.floor(Math.random() * 4200),
    merkleRoot: randomHex(32),
    size: 180_000 + Math.floor(Math.random() * 320_000),
  };
}

function mockTxDetail(hash: string): TxDetail {
  const types = [
    "Transfer",
    "Settle",
    "Swap",
    "Escrow",
    "Stake",
    "WasmCall",
    "MultiSig",
  ];
  return {
    hash,
    blockHeight: 148_000 + Math.floor(Math.random() * 293),
    from: randomAddress(),
    to: randomAddress(),
    amount: String(Math.floor(Math.random() * 500_000)),
    txType: types[Math.floor(Math.random() * types.length)],
    nonce: Math.floor(Math.random() * 50_000),
    timestamp: Date.now() - Math.floor(Math.random() * 300_000),
    status: "confirmed",
    preHashHex: randomHex(32),
  };
}

function mockTxProof(hash: string): TxProof {
  const siblingCount = 8 + Math.floor(Math.random() * 8);
  const siblings: { hash: string; isLeft: boolean }[] = [];
  for (let i = 0; i < siblingCount; i++) {
    siblings.push({ hash: randomHex(32), isLeft: Math.random() > 0.5 });
  }
  return {
    txHash: hash,
    preHashHex: randomHex(32),
    blake3Domain: "arc-chain-v1",
    merkleProof: {
      leaf: randomHex(32),
      index: Math.floor(Math.random() * 4096),
      siblings,
      root: randomHex(32),
    },
    blockHeight: 148_000 + Math.floor(Math.random() * 293),
    pedersenCommitment: randomHex(32),
  };
}

function mockStats(): Stats {
  return {
    tps: 0,
    blockHeight: 148_293,
    totalTxs: 742_156,
    avgBlockTimeMs: 400,
    nodeCount: 0,
  };
}

function mockRecentBlocks(limit: number): Block[] {
  const blocks: Block[] = [];
  const tip = 148_293;
  for (let i = 0; i < limit; i++) {
    blocks.push(mockBlock(tip - i));
  }
  return blocks;
}

// ---------------------------------------------------------------------------
// Fetch helper — tries /api/chain/* proxy, returns null on failure
// ---------------------------------------------------------------------------

async function fetchRpc<T>(endpoint: string): Promise<{
  data: T;
  live: boolean;
} | null> {
  try {
    const res = await fetch(`/api/chain/${endpoint}`);
    const json = await res.json();

    if (json._source === "unavailable" || !res.ok) {
      return null;
    }

    // Strip the _source meta key before returning data
    // If proxy wrapped an array response, unwrap it
    const { _source, data: wrappedData, ...rest } = json;
    void _source;
    const result = wrappedData !== undefined ? wrappedData : rest;
    return { data: result as T, live: true };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

export async function getBlock(
  height: number
): Promise<ChainResponse<Block>> {
  const rpc = await fetchRpc<Block>(`block/${height}`);
  if (rpc) return { data: rpc.data, source: "live" };
  return { data: mockBlock(height), source: "demo" };
}

export async function getTx(hash: string): Promise<ChainResponse<TxDetail>> {
  const rpc = await fetchRpc<TxDetail>(`tx/${hash}`);
  if (rpc) return { data: rpc.data, source: "live" };
  return { data: mockTxDetail(hash), source: "demo" };
}

export async function getTxProof(
  hash: string
): Promise<ChainResponse<TxProof>> {
  const rpc = await fetchRpc<TxProof>(`tx/${hash}/proof`);
  if (rpc) return { data: rpc.data, source: "live" };
  return { data: mockTxProof(hash), source: "demo" };
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function mapLiveStats(raw: any): Stats {
  // Rust node returns snake_case fields; map to our camelCase Stats
  return {
    tps: raw.tps ?? 0,
    blockHeight: raw.block_height ?? raw.blockHeight ?? 0,
    totalTxs: raw.total_receipts ?? raw.totalTxs ?? raw.total_transactions ?? 0,
    avgBlockTimeMs: raw.avg_block_time_ms ?? raw.avgBlockTimeMs ?? 400,
    nodeCount: raw.node_count ?? raw.nodeCount ?? 1,
  };
}

export async function getStats(): Promise<ChainResponse<Stats>> {
  const rpc = await fetchRpc<Record<string, unknown>>("stats");
  if (rpc) return { data: mapLiveStats(rpc.data), source: "live" };
  return { data: mockStats(), source: "demo" };
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function mapLiveBlock(raw: any): Block {
  return {
    height: raw.height ?? 0,
    hash: raw.hash?.startsWith("0x") ? raw.hash : `0x${raw.hash ?? ""}`,
    parentHash: raw.parent_hash?.startsWith("0x") ? raw.parent_hash : `0x${raw.parent_hash ?? ""}`,
    timestamp: raw.timestamp ?? 0,
    txCount: raw.tx_count ?? 0,
    merkleRoot: raw.tx_root?.startsWith("0x") ? raw.tx_root : `0x${raw.tx_root ?? ""}`,
    size: raw.size ?? 0,
  };
}

export async function getRecentBlocks(
  limit: number = 20
): Promise<ChainResponse<Block[]>> {
  const rpc = await fetchRpc<Record<string, unknown>>(`blocks?limit=${limit}`);
  if (rpc) {
    // Rust node wraps blocks in a `blocks` field
    const rawBlocks = (rpc.data as Record<string, unknown>).blocks ?? rpc.data;
    const blocks = Array.isArray(rawBlocks) ? rawBlocks.map(mapLiveBlock) : [];
    return { data: blocks, source: "live" };
  }
  return { data: mockRecentBlocks(limit), source: "demo" };
}

export async function checkDataSource(): Promise<DataSource> {
  const rpc = await fetchRpc<unknown>("health");
  return rpc ? "live" : "demo";
}
