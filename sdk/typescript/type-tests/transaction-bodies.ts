import type { TransactionBody, TransactionType } from "../src/index.js";

type BodyFor<Type extends TransactionType> = Extract<
  TransactionBody,
  { type: Type }
>;

type BodyFixtures = {
  [Type in TransactionType]: BodyFor<Type>;
};

const hex = "00";
const prefixedHex = "0x00" as const;

/**
 * Compile-only fixtures for the JSON projections returned by GET /tx/{hash}/full.
 * The mapped type makes additions to TransactionBody fail typecheck until this
 * contract is updated with the exact RPC shape.
 */
export const rpcTransactionBodyFixtures = {
  Transfer: {
    type: "Transfer",
    to: hex,
    amount: 1,
    amount_commitment: null,
  },
  Settle: {
    type: "Settle",
    agent_id: hex,
    service_hash: hex,
    amount: 1,
    usage_units: 1,
  },
  Swap: {
    type: "Swap",
    counterparty: hex,
    offer_amount: 1,
    receive_amount: 1,
    offer_asset: hex,
    receive_asset: hex,
  },
  Escrow: {
    type: "Escrow",
    beneficiary: hex,
    amount: 1,
    conditions_hash: hex,
    is_create: true,
  },
  Stake: {
    type: "Stake",
    amount: 1,
    is_stake: true,
    validator: hex,
  },
  WasmCall: {
    type: "WasmCall",
    contract: hex,
    function: "run",
    calldata: hex,
    value: 0,
    gas_limit: 1,
  },
  MultiSig: {
    type: "MultiSig",
    signers: [hex],
    threshold: 1,
  },
  DeployContract: {
    type: "DeployContract",
    bytecode_size: 1,
    constructor_args_size: 0,
    state_rent_deposit: 1,
  },
  RegisterAgent: {
    type: "RegisterAgent",
    agent_name: "worker",
    endpoint: "https://worker.invalid",
    protocol: hex,
    capabilities_size: 1,
  },
  JoinValidator: {
    type: "JoinValidator",
    pubkey: hex,
    initial_stake: 1,
  },
  LeaveValidator: { type: "LeaveValidator" },
  ClaimRewards: { type: "ClaimRewards" },
  UpdateStake: { type: "UpdateStake", new_stake: 1 },
  Governance: {
    type: "Governance",
    proposal_id: 1,
    action: "Execute",
  },
  BridgeLock: {
    type: "BridgeLock",
    destination_chain: 1,
    destination_address: hex,
    amount: 1,
  },
  BridgeMint: {
    type: "BridgeMint",
    source_chain: 1,
    source_tx_hash: hex,
    recipient: hex,
    amount: 1,
    merkle_proof_size: 1,
  },
  BatchSettle: {
    type: "BatchSettle",
    entries: 1,
    total_amount: 1,
  },
  ChannelOpen: {
    type: "ChannelOpen",
    channel_id: prefixedHex,
    counterparty: prefixedHex,
    deposit: 1,
    timeout_blocks: 1,
  },
  ChannelClose: {
    type: "ChannelClose",
    channel_id: prefixedHex,
    opener_balance: 1,
    counterparty_balance: 1,
    state_nonce: 1,
  },
  ChannelDispute: {
    type: "ChannelDispute",
    channel_id: prefixedHex,
    opener_balance: 1,
    counterparty_balance: 1,
    state_nonce: 1,
    challenge_period: 1,
  },
  ShardProof: {
    type: "ShardProof",
    shard_id: 1,
    block_height: 1,
    tx_count: 1,
    proof_size: 1,
    prev_state_root: prefixedHex,
    post_state_root: prefixedHex,
  },
  InferenceAttestation: {
    type: "InferenceAttestation",
    model_id: prefixedHex,
    input_hash: prefixedHex,
    output_hash: prefixedHex,
    challenge_period: 1,
    bond: 1,
  },
  InferenceChallenge: {
    type: "InferenceChallenge",
    attestation_hash: prefixedHex,
    challenger_output_hash: prefixedHex,
    challenger_bond: 1,
  },
  InferenceRegister: {
    type: "InferenceRegister",
    tier: 1,
    stake_bond: 1,
  },
  InferenceEscrowOpen: {
    type: "InferenceEscrowOpen",
    request_id: prefixedHex,
    model_id: prefixedHex,
    max_fee: 1,
    max_tokens: 1,
    timeout_blocks: 1,
  },
  InferenceEscrowRelease: {
    type: "InferenceEscrowRelease",
    request_id: prefixedHex,
    payer: prefixedHex,
    model_id: prefixedHex,
    max_tokens: 1,
    timeout_blocks: 1,
    output_hash: prefixedHex,
    proposer: prefixedHex,
    replicas: [prefixedHex],
    observer_pool: prefixedHex,
    treasury: prefixedHex,
  },
  InferenceEscrowRefund: {
    type: "InferenceEscrowRefund",
    request_id: prefixedHex,
    model_id: prefixedHex,
    max_tokens: 1,
    timeout_blocks: 1,
  },
  ModelRegistration: {
    type: "ModelRegistration",
    model_id: prefixedHex,
    metadata_hash: prefixedHex,
    chunk_tree_root: prefixedHex,
    n_layers: 1,
    d_model: 1,
    quantization: "int8",
    registration_fee: 1,
    royalty_recipient: prefixedHex,
  },
  ModelRequest: {
    type: "ModelRequest",
    request_id: prefixedHex,
    model_id: prefixedHex,
    target_k_replication: 1,
    bond_per_layer_epoch: 1,
    max_wait_secs: 1,
  },
  ShardCoverageClaim: {
    type: "ShardCoverageClaim",
    model_id: prefixedHex,
    node_pubkey: prefixedHex,
    ranges: [[0, 1]],
    bond: 1,
    epoch_blocks: 1,
  },
  CapacityAdvertisement: {
    type: "CapacityAdvertisement",
    node_pubkey: prefixedHex,
    ram_bytes: 1,
    vram_bytes: 1,
    bandwidth_mbps: 1,
    uptime_hint_mins: 1,
    stake: 1,
    region: "US",
  },
  ShardAssignmentProposal: {
    type: "ShardAssignmentProposal",
    epoch_blocks: 1,
    input_snapshot_hash: prefixedHex,
    assignments: [
      {
        node_pubkey: prefixedHex,
        model_id: prefixedHex,
        ranges: [[0, 1]],
      },
    ],
  },
  FaucetClaim: {
    type: "FaucetClaim",
    recipient: hex,
    amount: 1,
  },
  InferenceRequest: {
    type: "InferenceRequest",
    request_id: prefixedHex,
    model_id: hex,
    input_hash: hex,
    max_tokens: 1,
    tier: 1,
    max_reward: 1,
    deadline_blocks: 1,
    committee_size: 1,
  },
  InferenceVote: {
    type: "InferenceVote",
    request_id: prefixedHex,
    output_hash: hex,
    output_blob_attached: false,
  },
  InferenceFinalize: {
    type: "InferenceFinalize",
    request_id: prefixedHex,
  },
  CommunityInferenceReward: {
    type: "CommunityInferenceReward",
    chain_domain: hex,
    job_id: hex,
    worker: hex,
    model_id: hex,
    input_hash: hex,
    output_hash: hex,
    max_tokens: 1,
    expires_at_height: 1,
    worker_attestation_hash: hex,
  },
} satisfies BodyFixtures;

/** TxType wire discriminants; this record also makes 0x01..0x25 coverage explicit. */
export const transactionTypeCodes = {
  Transfer: 0x01,
  Settle: 0x02,
  Swap: 0x03,
  Escrow: 0x04,
  Stake: 0x05,
  WasmCall: 0x06,
  MultiSig: 0x07,
  DeployContract: 0x08,
  RegisterAgent: 0x09,
  JoinValidator: 0x0a,
  LeaveValidator: 0x0b,
  ClaimRewards: 0x0c,
  UpdateStake: 0x0d,
  Governance: 0x0e,
  BridgeLock: 0x0f,
  BridgeMint: 0x10,
  BatchSettle: 0x11,
  ChannelOpen: 0x12,
  ChannelClose: 0x13,
  ChannelDispute: 0x14,
  ShardProof: 0x15,
  InferenceAttestation: 0x16,
  InferenceChallenge: 0x17,
  InferenceRegister: 0x18,
  InferenceEscrowOpen: 0x19,
  InferenceEscrowRelease: 0x1a,
  InferenceEscrowRefund: 0x1b,
  ModelRegistration: 0x1c,
  ModelRequest: 0x1d,
  ShardCoverageClaim: 0x1e,
  CapacityAdvertisement: 0x1f,
  ShardAssignmentProposal: 0x20,
  FaucetClaim: 0x21,
  InferenceRequest: 0x22,
  InferenceVote: 0x23,
  InferenceFinalize: 0x24,
  CommunityInferenceReward: 0x25,
} as const satisfies Record<TransactionType, number>;

const finalWireDiscriminant: 0x25 =
  transactionTypeCodes.CommunityInferenceReward;
void finalWireDiscriminant;
