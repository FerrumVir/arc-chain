// Mock data — replace with real RPC calls when node is running
// Story-driven: every transaction is a named AI agent doing real work on ARC Chain.
import type {
  Block,
  Transaction,
  NetworkStats,
  NodeInfo,
  TxType,
} from "./types";

// ---------------------------------------------------------------------------
// 1. Named Agents — 20 real-feeling identities
// ---------------------------------------------------------------------------

export interface Agent {
  name: string;
  address: string;
  type:
    | "payment-agent"
    | "defi-agent"
    | "oracle"
    | "insurance"
    | "escrow"
    | "governance"
    | "validator"
    | "treasury"
    | "merchant"
    | "liquidity"
    | "compliance"
    | "bridge";
  balance: number;
  is_contract: boolean;
}

const AGENTS: Agent[] = [
  { name: "PaymentBot-7",         address: "0x7a2F8c91D034bE56A1c3d90482eF7319aB08cD4e", type: "payment-agent", balance: 1_240_300,   is_contract: true },
  { name: "TradingEngine-Alpha",  address: "0x3b8C44eF7291Da05B6c82A9f1dE03478FC56a1D2", type: "defi-agent",    balance: 8_750_000,   is_contract: true },
  { name: "DataOracle-West",      address: "0xf1D4a29C0387bE56d1A4c90F82e37194DB05aC8e", type: "oracle",        balance: 520_400,     is_contract: true },
  { name: "InsurancePool-3",      address: "0x9e2A7b14C0583dF69a1B42E07c8D93F51642eA0b", type: "insurance",     balance: 15_200_000,  is_contract: true },
  { name: "EscrowVault-Prime",    address: "0x2cD1F83a7B09e46C5d82A1f03E94b6D78c21aF50", type: "escrow",        balance: 4_380_000,   is_contract: true },
  { name: "GovernanceBot-DAO",    address: "0x5e8B2f41C0Da739A6b1c4E82d0F57193cB04eD28", type: "governance",    balance: 320_000,     is_contract: true },
  { name: "ValidatorNode-East",   address: "0xa4C9d1E07B283fD56c90482eA1F37d08bC5a62F1", type: "validator",     balance: 25_000_000,  is_contract: false },
  { name: "Treasury-MultiSig",    address: "0x1bF0e38D4Ca729A5d16B82c0F47E931D058a4C67", type: "treasury",      balance: 120_000_000, is_contract: true },
  { name: "MerchantGateway-12",   address: "0xd83A4c1F0b27E569D82a4C93f1e0B574Da28cF01", type: "merchant",      balance: 890_200,     is_contract: true },
  { name: "LiquidityBot-9",       address: "0x6fE2b91C4D083a57A1d29e0B4cF8716D3E50a4C8", type: "liquidity",     balance: 6_100_000,   is_contract: true },
  { name: "ComplianceOracle-US",  address: "0x0c4D8e2F1a7B359C6d12A4b08E93f7D14Ca50eB6", type: "compliance",    balance: 180_500,     is_contract: true },
  { name: "BridgeRelay-ETH",      address: "0x8aB1c3D09E4f27A5d680B12e3cF4a918D75b02C4", type: "bridge",        balance: 32_400_000,  is_contract: true },
  { name: "PaymentBot-22",        address: "0xe5D2f0a1B483c769d12E84A0b3cF5D927a16eC08", type: "payment-agent", balance: 2_100_700,   is_contract: true },
  { name: "TradingEngine-Beta",   address: "0x4a9C1dE27B08f356A1c82D04b3eF7918Da50cB61", type: "defi-agent",    balance: 5_430_000,   is_contract: true },
  { name: "DataOracle-East",      address: "0xb7F0e24D1C83a5697d12B40A8eC3f91D054b2cE6", type: "oracle",        balance: 410_200,     is_contract: true },
  { name: "InsurancePool-7",      address: "0x3dA2c1F08E4b7569a1D82C93b0eF47D15Ca60eB8", type: "insurance",     balance: 9_800_000,   is_contract: true },
  { name: "EscrowVault-Gamma",    address: "0xc1E04a2D9B38f756d182C40b3eA7F91D05c84bF2", type: "escrow",        balance: 2_750_000,   is_contract: true },
  { name: "ValidatorNode-West",   address: "0x72B4d1C0E83a9F56A1c2D048b3eF719Da50c82E4", type: "validator",     balance: 18_500_000,  is_contract: false },
  { name: "SettlementEngine-1",   address: "0xf9A1b2C4D083e756d1E284B0c3aF7D19C5e40b68", type: "defi-agent",    balance: 11_200_000,  is_contract: true },
  { name: "BridgeRelay-SOL",      address: "0x2eC1d0A4B783f956a1D42E08b3cF5a91D7e60cB4", type: "bridge",        balance: 27_100_000,  is_contract: true },
];

const ADDRESS_TO_AGENT = new Map<string, Agent>();
for (const a of AGENTS) ADDRESS_TO_AGENT.set(a.address, a);

/**
 * Look up a human-readable agent name from an address.
 * Returns the address truncated if no agent is found.
 */
export function getAgentName(address: string): string {
  const agent = ADDRESS_TO_AGENT.get(address);
  if (agent) return agent.name;
  return `${address.slice(0, 6)}...${address.slice(-4)}`;
}

// ---------------------------------------------------------------------------
// 2. Deterministic helpers
// ---------------------------------------------------------------------------

/** Seeded pseudo-random so mock data is stable across renders. */
function seededRandom(seed: number): () => number {
  let s = Math.abs(seed) || 1;
  return () => {
    s = (s * 16807 + 0) % 2147483647;
    return (s - 1) / 2147483646;
  };
}

function pickAgent(rand: () => number, exclude?: string): Agent {
  let agent: Agent;
  do {
    agent = AGENTS[Math.floor(rand() * AGENTS.length)];
  } while (agent.address === exclude);
  return agent;
}

function deterministicHash(seed: number): string {
  const rand = seededRandom(seed);
  const chars = "0123456789abcdef";
  let h = "0x";
  for (let i = 0; i < 64; i++) h += chars[Math.floor(rand() * 16)];
  return h;
}

// ---------------------------------------------------------------------------
// 3. Transaction generation — story-driven, blockchain-standard (~250 bytes)
// ---------------------------------------------------------------------------

const TX_TYPES: TxType[] = [
  "Transfer",
  "Settle",
  "Swap",
  "Escrow",
  "Stake",
  "WasmCall",
  "MultiSig",
];

/** Gas ranges by tx type — blockchain-standard: 50k-200k */
const GAS_RANGE: Record<TxType, [number, number]> = {
  Transfer: [52_000, 78_000],
  Settle:   [85_000, 140_000],
  Swap:     [95_000, 165_000],
  Escrow:   [110_000, 180_000],
  Stake:    [55_000, 90_000],
  WasmCall: [130_000, 200_000],
  MultiSig: [140_000, 195_000],
};

/** Byte size ranges by tx type — blockchain-standard 180-350 (ETH ~250, SUI ~300) */
const BYTE_RANGE: Record<TxType, [number, number]> = {
  Transfer: [180, 230],
  Settle:   [220, 290],
  Swap:     [210, 270],
  Escrow:   [250, 330],
  Stake:    [180, 220],
  WasmCall: [280, 350],
  MultiSig: [300, 350],
};

/** Amount ranges by tx type — institutional-grade, 100-500k */
const AMOUNT_RANGE: Record<TxType, [number, number]> = {
  Transfer: [100, 25_000],
  Settle:   [5_000, 250_000],
  Swap:     [1_000, 100_000],
  Escrow:   [10_000, 300_000],
  Stake:    [50_000, 500_000],
  WasmCall: [0, 5_000],
  MultiSig: [25_000, 500_000],
};

/** Weighted type selection: more transfers/settles than governance calls */
const TYPE_WEIGHTS: [TxType, number][] = [
  ["Transfer", 30],
  ["Settle", 22],
  ["Swap", 18],
  ["Escrow", 10],
  ["Stake", 8],
  ["WasmCall", 7],
  ["MultiSig", 5],
];

function pickTxType(rand: () => number): TxType {
  const total = TYPE_WEIGHTS.reduce((s, [, w]) => s + w, 0);
  let r = rand() * total;
  for (const [t, w] of TYPE_WEIGHTS) {
    r -= w;
    if (r <= 0) return t;
  }
  return "Transfer";
}

function rangeVal(rand: () => number, range: [number, number]): number {
  return range[0] + rand() * (range[1] - range[0]);
}

/** Pair agents intelligently based on tx type */
function pickAgentPair(
  rand: () => number,
  txType: TxType
): { from: Agent; to: Agent } {
  const typePreferences: Record<TxType, [Agent["type"][], Agent["type"][]]> = {
    Transfer:  [["payment-agent", "defi-agent", "bridge"], ["merchant", "payment-agent", "escrow"]],
    Settle:    [["defi-agent", "payment-agent"], ["oracle", "merchant", "defi-agent"]],
    Swap:      [["liquidity", "defi-agent", "bridge"], ["liquidity", "defi-agent", "bridge"]],
    Escrow:    [["insurance", "escrow", "defi-agent"], ["escrow", "insurance", "merchant"]],
    Stake:     [["validator", "defi-agent"], ["validator"]],
    WasmCall:  [["governance", "compliance", "oracle"], ["governance", "compliance", "oracle"]],
    MultiSig:  [["treasury", "governance"], ["validator", "insurance", "bridge"]],
  };

  const [fromPrefs, toPrefs] = typePreferences[txType];

  const fromCandidates = AGENTS.filter((a) => fromPrefs.includes(a.type));
  const from = fromCandidates.length > 0
    ? fromCandidates[Math.floor(rand() * fromCandidates.length)]
    : pickAgent(rand);

  const toCandidates = AGENTS.filter(
    (a) => toPrefs.includes(a.type) && a.address !== from.address
  );
  const to = toCandidates.length > 0
    ? toCandidates[Math.floor(rand() * toCandidates.length)]
    : pickAgent(rand, from.address);

  return { from, to };
}

function generateTx(blockHeight: number, idx: number, seed: number): Transaction {
  const rand = seededRandom(seed + blockHeight * 1000 + idx);
  const txType = pickTxType(rand);
  const { from, to } = pickAgentPair(rand, txType);

  const amount = Math.round(rangeVal(rand, AMOUNT_RANGE[txType]) * 100) / 100;
  const gas_used = Math.round(rangeVal(rand, GAS_RANGE[txType]));
  const byte_size = Math.round(rangeVal(rand, BYTE_RANGE[txType]));

  // 98.5% success rate — occasional failures for realism
  const success = rand() > 0.015;

  // Spread timestamps 1-4 seconds apart
  const gap = 1000 + Math.floor(rand() * 3000);

  return {
    hash: deterministicHash(seed + blockHeight * 1000 + idx * 7),
    tx_type: txType,
    from: from.address,
    to: to.address,
    amount,
    nonce: Math.floor(rand() * 50_000),
    gas_used,
    byte_size,
    success,
    timestamp: Date.now() - idx * gap,
    block_height: blockHeight,
  };
}

// ---------------------------------------------------------------------------
// 4. agentDescription — human-readable tx narrative
// ---------------------------------------------------------------------------

/**
 * Returns a human-readable string describing what happened in a transaction.
 * e.g. "PaymentBot-7 sent 2,450 ARC to MerchantGateway-12"
 */
export function agentDescription(tx: Transaction): string {
  const fromName = getAgentName(tx.from);
  const toName = getAgentName(tx.to);
  const amt = tx.amount.toLocaleString("en-US", {
    minimumFractionDigits: 0,
    maximumFractionDigits: 2,
  });

  switch (tx.tx_type) {
    case "Transfer":
      return `${fromName} sent ${amt} ARC to ${toName}`;
    case "Settle":
      return `${fromName} settled ${amt} ARC invoice with ${toName}`;
    case "Swap": {
      const usdcAmount = Math.round(tx.amount * 0.82).toLocaleString();
      return `${fromName} swapped ${amt} ARC for ${usdcAmount} USDC via ${toName}`;
    }
    case "Escrow": {
      const days = 3 + (tx.byte_size % 12);
      return `${fromName} created escrow for ${amt} ARC (${days}-day lockup) held by ${toName}`;
    }
    case "Stake":
      return `${fromName} staked ${amt} ARC with ${toName}`;
    case "WasmCall": {
      const proposalNum = 40 + (tx.gas_used % 60);
      return `${fromName} executed vote on Proposal #${proposalNum} via ${toName}`;
    }
    case "MultiSig": {
      const signers = 3 + (tx.byte_size % 3);
      const required = Math.max(2, signers - 1);
      return `${fromName} approved ${amt} ARC distribution (${required}/${signers} signers) to ${toName}`;
    }
    default:
      return `${fromName} transacted ${amt} ARC with ${toName}`;
  }
}

// ---------------------------------------------------------------------------
// 5. Block generation
// ---------------------------------------------------------------------------

const BLOCK_PRODUCERS: string[] = [
  AGENTS[6].address,  // ValidatorNode-East
  AGENTS[17].address, // ValidatorNode-West
];

function generateBlock(height: number, seed: number = 42): Block {
  const rand = seededRandom(seed + height);
  const txCount = 800 + Math.floor(rand() * 4200); // 800-5000 txs per block
  const txs: Transaction[] = [];
  for (let i = 0; i < Math.min(txCount, 25); i++) {
    txs.push(generateTx(height, i, seed));
  }

  const producer = BLOCK_PRODUCERS[Math.floor(rand() * BLOCK_PRODUCERS.length)];

  return {
    height,
    hash: deterministicHash(seed + height * 3),
    parent_hash: deterministicHash(seed + (height - 1) * 3),
    state_root: deterministicHash(seed + height * 5 + 1),
    tx_root: deterministicHash(seed + height * 5 + 2),
    tx_count: txCount,
    timestamp: Date.now() - (148_293 - height) * 400,
    producer,
    transactions: txs,
  };
}

// ---------------------------------------------------------------------------
// 6. Exported API — matches existing signatures
// ---------------------------------------------------------------------------

let cachedBlocks: Block[] | null = null;

export function getBlocks(count: number = 20): Block[] {
  if (cachedBlocks && cachedBlocks.length >= count)
    return cachedBlocks.slice(0, count);
  const blocks: Block[] = [];
  const startHeight = 148_293;
  for (let i = 0; i < count; i++) {
    blocks.push(generateBlock(startHeight - i));
  }
  cachedBlocks = blocks;
  return blocks;
}

export function getBlock(height: number): Block {
  return generateBlock(height);
}

export function getRecentTransactions(count: number = 20): Transaction[] {
  const txs: Transaction[] = [];
  for (let i = 0; i < count; i++) {
    txs.push(generateTx(148_293 - Math.floor(i / 5), i, 42));
  }
  return txs;
}

export function getNetworkStats(): NetworkStats {
  const jitter = Math.floor(Math.random() * 200_000);
  return {
    chain_height: 148_293,
    total_transactions: 847_291_038,
    total_accounts: 10_482,
    tps_current: 4_300_000 + jitter,
    tps_peak: 5_996_417,
    total_staked: 42_500_000,
    node_count: 1,
    uptime_seconds: 518_400,
    visa_comparison: "36x faster than Sui, 66x Solana, 143,000x Ethereum",
    avg_tx_size: 250,
  };
}

export function getNodes(): NodeInfo[] {
  return [
    {
      peer_id: "arc-node-genesis-01",
      version: "0.1.0",
      region: "Local (MacBook M4)",
      stake: 10_000_000,
      status: "active",
      blocks_produced: 148_293,
      uptime_pct: 99.97,
    },
  ];
}

export function getAccount(address: string) {
  const agent = ADDRESS_TO_AGENT.get(address);
  if (agent) {
    const codeHash = agent.is_contract
      ? deterministicHash(agent.address.length * 7)
      : null;
    return {
      address: agent.address,
      balance: agent.balance,
      nonce: Math.floor(agent.balance / 100),
      code_hash: codeHash,
      is_contract: agent.is_contract,
      tx_count: Math.floor(agent.balance / 50),
    };
  }
  // Fallback for unknown addresses
  const rand = seededRandom(parseInt(address.slice(2, 10), 16) || 12345);
  return {
    address,
    balance: Math.floor(rand() * 10_000_000) / 100,
    nonce: Math.floor(rand() * 5000),
    code_hash:
      rand() > 0.7
        ? deterministicHash(parseInt(address.slice(2, 10), 16) || 0)
        : null,
    is_contract: rand() > 0.7,
    tx_count: Math.floor(rand() * 10000),
  };
}
