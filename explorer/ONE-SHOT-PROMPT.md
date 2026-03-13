# ARC Explorer — One-Shot Build Prompt

> Feed this entire prompt to Claude Code (or any AI coding agent) to build the complete ARC explorer site from scratch. The site is a combined **blockchain explorer + marketing site** for ARC — an L1 blockchain built for AI. Think **solana.com** meets **Etherscan**, but darker, sharper, and more technical.

---

## MISSION

Build a complete, production-ready React site for **ARC** — an L1 blockchain. The site serves three audiences:
1. **Developers** exploring blocks, transactions, accounts, validators
2. **Community** learning about ARC's technology and getting involved
3. **Institutional visitors** evaluating ARC's technical credibility

The design reference is **solana.com** — bold hero sections with massive typography, gradient mesh backgrounds, scroll-reveal animations, prominent stats counters, card grids, and a dark-first aesthetic. But adapted for ARC's brand: sharper (zero border-radius), more technical (monospace accents), and darker (#03030A base).

---

## WHAT IS ARC (the narrative — use this content throughout the site)

### The Company
**ARC** is a privacy-first AI infrastructure company headquartered in **Zug, Switzerland** (ARC Labs AG), with token operations via ARC Inc. (Samoa). ARC builds infrastructure that lets businesses and individuals run AI **without data exposure** — privately and securely. The tagline is **"ai for Humans First"**.

ARC's website is **arc.ai**. The brand has three core beliefs: privacy as power, security by design, transparency matters.

### The Three Products
ARC has three public-facing products. **ARC Chain (Protocol) is the infrastructure layer that powers all of them.**

1. **Matrix by ARC** — "The Privacy Shield for AI"
   - Data stays **encrypted-in-use** inside verified secure environments — never exposed in plaintext during AI processing
   - **Wallet-enforced access** (no traditional accounts, crypto-native identity)
   - Tamper-proof cryptographic audit trails for compliance (GDPR, HIPAA)
   - Use cases: financial services (fraud detection, KYC/AML, risk modeling), healthcare (diagnostics, clinical trials), enterprise (secure collaboration, compliance reporting)
   - Private chat, secure compute, evaluation environments, integration APIs
   - Live at **matrix.arc.ai**

2. **Reactor by ARC** — Fast, efficient AI assistant
   - Chat, web search, YouTube/X/Reddit/academic search, image generation, code
   - Subscription model, advanced APIs for developers
   - Deep Search feature
   - Live at **reactor.arc.ai**

3. **Protocol by ARC** — The L1 blockchain (THIS IS WHAT WE'RE BUILDING THE EXPLORER FOR)
   - The **encrypted compute backbone** that powers Matrix's privacy guarantees and Reactor's infrastructure
   - Stateless, deterministic backbone with independent control plane
   - This is ARC Chain — the 21-TX-type, GPU-verified, STARK-proven L1

### The $ARC Token
- **Total supply**: 1,030,000,000 $ARC
- **Distribution**: 85% Community / 10% Operations / 5% Team
- **No vesting schedules** — immediate utility
- **Utilities**: Access (unlock ARC products), Operate (network contributors use $ARC for encrypted compute), Govern (community voting)
- **500M tokens** locked in multisig, released via v1→v2 migration portal (1:1 ratio)
- **Contract** (Ethereum): `0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499`
- **Not a security** — utility token for ecosystem participation

### How ARC Chain (Protocol) Connects Everything
ARC Chain is the Protocol layer — the **encrypted compute backbone** that makes Matrix's privacy guarantees provable and Reactor's infrastructure trustless:

| ARC Product | What ARC Chain Provides |
|-------------|------------------------|
| **Matrix** | On-chain attestation that data stayed encrypted-in-use. Wallet-enforced access is verified on-chain. Tamper-proof audit trails are chain-native. $ARC tokens pay for encrypted compute. |
| **Reactor** | Verifiable AI execution. Network contributors run compute and settle via $ARC on-chain. |
| **$ARC Token** | ARC Chain IS the native chain for $ARC. Transfer, Stake, Governance TX types are all built-in. |

### The Architecture (ARC's "three pillars" from arc.ai)
1. **Secure Core Architecture** — Stateless, deterministic backbone. Consistent behavior across environments. (= ARC Chain's consensus + execution layers)
2. **Independent Control Layer** — User-controlled identity, permissions, routing. Wallet-enforced, not admin-configured. (= ARC Chain's RegisterAgent + Governance TX types)
3. **Efficiency Engine** — Resource-light execution without performance loss. Hardware-flexible. (= ARC Chain's Block-STM parallel execution + GPU acceleration)

### Why ARC Built Its Own Chain
No existing blockchain satisfies ARC's requirements:
- **Privacy**: Data must stay encrypted-in-use during all processing — not just at rest or in transit. ARC Chain's native TX types handle encrypted compute attestation.
- **Wallet-native identity**: Users authenticate via wallet, not username/password. ARC Chain's `RegisterAgent` gives every user and AI agent a cryptographic identity.
- **Compliance provability**: Every action must generate tamper-proof cryptographic evidence. ARC Chain writes these to an immutable state tree (Jellyfish Merkle Tree, STARK-proven).
- **Scale**: Matrix processes encrypted AI workloads for enterprise. Reactor serves consumer AI. The chain must handle both — hence 1B+ effective TPS through BatchSettle, State Channels, and Shard Proofs.

### The 1B+ TPS Story
ARC Chain achieves massive throughput through three layers of scaling, all native to L1:

| Layer | Mechanism | Multiplier |
|-------|-----------|------------|
| **Base L1** | Block-STM parallel execution + GPU Ed25519 verification | 27,000+ TPS (proven on M4 MacBook Pro) |
| **Batch Settlement** | `BatchSettle` TX nets 1000:1 — one on-chain TX settles 1,000 off-chain operations | 27M effective TPS |
| **State Channels** | Bilateral off-chain channels (`ChannelOpen/Close/Dispute`) for high-frequency pairs | 100M+ effective TPS |
| **Shard Proofs** | Cross-shard STARK proofs (`ShardProof` TX) verified on-chain — horizontal scaling | 1B+ effective TPS |

This isn't theoretical. The TX types are implemented. The STARK prover works. The GPU verification runs. 1,028 tests pass.

### Key Differentiators (use in hero sections and cards)
1. **Privacy-First L1** — Built from the ground up for encrypted compute attestation. Matrix's encrypted-in-use processing is verified on-chain. No other chain was designed for this.
2. **21 Native TX Types** — Transfer, Settle, Swap, Escrow, Stake, WasmCall, RegisterAgent, BatchSettle, ChannelOpen, ShardProof, and 11 more. Every operation is a protocol primitive, not a smart contract call.
3. **Wallet-Native Identity** — `RegisterAgent` gives users and AI agents cryptographic identity. Wallet-enforced access, not username/password. Matches Matrix's authentication model.
4. **GPU-Accelerated Verification** — Metal + WGSL shaders with branchless Shamir's trick. 121K Ed25519 verifications/second. Every signature is hardware-verified.
5. **STARK-Proven State** — Stwo STARK proofs over M31 field with 22-constraint AIR. Every state transition is cryptographically proven. Zero-knowledge by design.
6. **$ARC Native** — The $ARC utility token (1.03B supply) runs natively on ARC Chain. Transfer, Stake, Governance — all built-in TX types.

### Positioning Statement
> **ARC Protocol: Privacy-first AI infrastructure, on-chain.** The L1 that powers Matrix's encrypted compute, Reactor's AI execution, and the $ARC token economy. 21 native transaction types. GPU-verified. STARK-proven. Architected for 1B+ TPS.

### Tagline
**"ai for Humans First"** — AI that works for people, privately and securely.

### Hero Copy Options (use these or variations)
- Primary: "Privacy-first AI infrastructure, on-chain."
- Alt 1: "The chain built for encrypted AI."
- Alt 2: "1 billion TPS. Zero data exposure."
- Alt 3: "Where privacy meets performance."
- Alt 4: "The Protocol powering Matrix, Reactor, and $ARC."
- Subhead: "21 native transaction types. GPU-accelerated verification. STARK-proven state. Architected for 1B+ TPS. The backbone of ARC's privacy-first AI ecosystem."

### Stats to Feature Prominently
| Stat | Value | Context |
|------|-------|---------|
| Base TPS | 27,000+ | Sustained on M4 MacBook Pro |
| Peak TPS | 350,000 | Burst throughput |
| Effective TPS | 1B+ | With BatchSettle + Channels + Shards |
| TX Types | 21 | All native to protocol |
| Codebase | 72,400 LOC | Rust, from scratch |
| Tests | 1,028 + 232 | Core + STARK prover |
| GPU Verify | 121K/sec | Ed25519 on Metal |
| STARK Constraints | 22 AIR | Circle STARK over M31 |
| $ARC Supply | 1.03B | 85% community, no vesting |

### Technology Stack (for architecture sections)
- **Language**: Rust (100% — no Go, no C++)
- **Consensus**: DAG-based with VRF proposer selection, stake-weighted leader rotation
- **Execution**: Block-STM parallel VM with WASM smart contracts
- **State**: Jellyfish Merkle Tree with incremental updates, WAL persistence, sharded cross-shard locking
- **Network**: QUIC transport with FEC erasure coding, PEX peer discovery
- **Cryptography**: Stwo STARK proofs, GPU Ed25519 (Metal/WGSL), BLS threshold signatures, Poseidon hashing, VRF
- **L1 Scaling**: BatchSettle (1000:1 netting), State Channels (bilateral off-chain), ShardProof (cross-shard STARK verification)

### The 6 Architecture Layers (for the Blockchain page cards)
1. **Consensus** — DAG-based block structure with VRF proposer selection. Stake-weighted, deterministic leader rotation. Sub-second finality.
2. **Execution** — Block-STM parallel execution with WASM virtual machine. Optimistic concurrency with abort-and-retry. 27K+ TPS sustained.
3. **State** — Jellyfish Merkle Tree with incremental updates. WAL persistence and sharded cross-shard locking. Every state root is STARK-proven.
4. **Network** — QUIC transport with FEC erasure coding and PEX peer discovery. Sub-second block propagation across global nodes.
5. **Cryptography** — Stwo STARK proofs, GPU Ed25519, BLS threshold signatures, Poseidon hashing. Every layer is cryptographically verified.
6. **Scaling** — BatchSettle, State Channels, and ShardProof — three native L1 scaling mechanisms that push throughput to 1B+ TPS without L2s.

### TX Type Categories (with ecosystem narrative)
- **Core** (5): Transfer, Settle, Swap, Escrow, MultiSig — the financial primitives powering $ARC token operations
- **Agent Economy** (3): RegisterAgent, WasmCall, DeployContract — wallet-native identity for Matrix users and AI agents
- **Staking** (5): Stake, JoinValidator, LeaveValidator, ClaimRewards, UpdateStake — proof of stake for network contributors
- **Governance** (1): Governance — on-chain proposals and voting with $ARC
- **Bridge** (2): BridgeLock, BridgeMint — cross-chain interoperability (Ethereum ↔ ARC)
- **L1 Scaling** (5): BatchSettle, ChannelOpen, ChannelClose, ChannelDispute, ShardProof — the 1B+ TPS stack

### Cryptography Features (for dedicated section)
1. **Stwo STARK Proofs** — Circle STARK over M31 field. 22-constraint AIR. Inner-circuit recursion for composable proof aggregation. Every state transition is proven.
2. **GPU Ed25519** — Metal + WGSL shaders. Branchless Shamir's trick with 4-entry LUT. Zero SIMD divergence. 121K verifications/sec on Apple Silicon.
3. **BLS Threshold Signatures** — blst-based threshold signatures. Feldman VSS for verifiable key distribution. N-of-M threshold encryption for validator committees.
4. **VRF Proposer Selection** — Ed25519-based verifiable random function. Weighted by stake. Cryptographically deterministic and unpredictable leader election.
5. **Poseidon Hash** — ZK-friendly algebraic hash. 2-to-1 Merkle hashing. Configurable full and partial rounds. Optimized for STARK circuits.
6. **Reed-Solomon FEC** — XOR erasure coding with 50% redundancy. Single-shred recovery for network resilience during block propagation.

---

## THE ARC ECOSYSTEM (what already exists — this is credibility, not a roadmap)

ARC Chain isn't a whitepaper project. It's the Protocol layer of an **existing, live ecosystem** with real products and real users.

### The Live Products

| Product | Status | URL | What It Does |
|---------|--------|-----|-------------|
| **Matrix** | Closed Beta (Q1 2026) | matrix.arc.ai | Privacy Shield for AI — encrypted-in-use processing, wallet-enforced access, compliance attestation |
| **Reactor** | Live (Mk. III) | reactor.arc.ai | Fast AI assistant — chat, deep search, image gen, code, APIs |
| **$ARC Token** | Live | Ethereum + Base | 1.03B supply, 85% community, ecosystem utility token |
| **ARC Chain** | Testnet | This explorer | The Protocol — 21 TX types, GPU-verified, STARK-proven L1 |

### Company Timeline
- **2024**: ARC Labs AG established in Zug, Switzerland. Reactor Mk. I + II launched. B2B partnerships. Migration to arc.ai.
- **Q1 2025**: Reactor Mk. III with Deep Search. Subscription model. Advanced APIs.
- **Q3-Q4 2025**: Matrix Playground beta (closed → open → public).
- **Q1 2026**: Matrix v1 beta. ARC Chain testnet. Explorer launch.

### The Narrative (use throughout the site)
ARC didn't start with a chain. ARC started with a **problem**: AI requires data, but data exposure is unacceptable. So ARC built Matrix — encrypted-in-use AI processing. Then Reactor — a fast, private AI assistant. Then realized: to make privacy guarantees **provable and trustless**, you need a chain purpose-built for encrypted compute attestation. That chain is ARC Protocol.

### Use This In The Site
- **Blockchain page hero**: "The Protocol powering Matrix, Reactor, and $ARC — ARC's privacy-first AI ecosystem."
- **Vision section**: "ARC didn't start with a chain. We started with the products — Matrix for encrypted AI, Reactor for fast AI. Then we built the chain to make it all provable."
- **Ecosystem section**: Show Matrix, Reactor, $ARC as live products with badges. Show ARC Chain as the Protocol layer connecting them.
- **Footer**: Link to arc.ai, matrix.arc.ai, reactor.arc.ai. Social links to ARC's actual channels (X: @ARCreactorAI, Discord, LinkedIn, Telegram).
- **$ARC references**: The chain's native token IS $ARC. 1.03B total supply. Already live on Ethereum, bridging to ARC Chain natively.

### Social Links (use in footer and community sections)
- **Website**: arc.ai
- **X/Twitter**: x.com/ARCreactorAI
- **Discord**: discord.com/invite/arcreactorai
- **LinkedIn**: linkedin.com/company/arcreactorai
- **Telegram**: t.me/JoinTheARC
- **YouTube**: youtube.com/@ARCreactorAI

---

## TECH STACK (exact versions)

```json
{
  "dependencies": {
    "react": "^19.0.0",
    "react-dom": "^19.0.0",
    "react-router-dom": "^7.1.0"
  },
  "devDependencies": {
    "@types/react": "^19.0.0",
    "@types/react-dom": "^19.0.0",
    "@vitejs/plugin-react": "^4.3.0",
    "autoprefixer": "^10.4.20",
    "postcss": "^8.5.0",
    "tailwindcss": "^4.0.0",
    "@tailwindcss/vite": "^4.0.0",
    "typescript": "^5.7.0",
    "vite": "^6.1.0"
  }
}
```

- **Vite 6** with `@tailwindcss/vite` plugin (Tailwind v4 — uses `@theme` syntax, NOT `tailwind.config.ts`)
- **Zero UI libraries** — all components hand-built (no shadcn, no MUI, no Radix)
- **Zero chart libraries** — sparklines are pure SVG
- **TypeScript strict mode**, `jsx: "react-jsx"`, path alias `@/*` → `./src/*`
- Dev server on port **3100**, API on **`VITE_API_URL`** (default `http://localhost:9090`)

---

## BRAND SYSTEM

### Colors
```
Core Black:     #03030A    (background, text on white)
Core White:     #FFFFFF    (text, button bg)
Pacific Purple: #002DDE    (gradient end, deep accent)
Medium Blue:    #3855E9    (secondary blue)
Aquarius:       #6F7CF4    (primary accent, links, gradient start)

Greys:
  700: #777785   (muted text, labels)
  600: #8E8E9D   (secondary text)
  500: #AEAEBC   (body text)
  400: #C7C7D6   (light text)
  300: #E5E5EA   (borders on light)
  200: #F4F4F4   (lightest)

Semantic:
  Success: #51EB8E   (green — confirmations, online)
  Error:   #FF0040   (red — failures, offline)
  Warning: #FF9446   (orange — caution)
  Info:    #00D4FF   (cyan — informational)

Surfaces:
  Surface:         #0A0A14   (card backgrounds)
  Surface Raised:  #12121E   (elevated cards)
  Surface Overlay: #1A1A28   (modals, dropdowns)
  Border:          #1E1E2E   (card borders)
  Border Subtle:   #14141F   (dividers)
```

### Gradient
Primary: `linear-gradient(135deg, #6F7CF4 0%, #002DDE 100%)` — used for text gradients, accent lines, hero backgrounds

### Typography
- **Primary font**: `'Favorit', 'Inter', 'SF Pro Display', -apple-system, sans-serif` — weight 300 (light) default, 500 for medium
- **Mono font**: `'SF Mono', 'Fira Code', 'JetBrains Mono', Consolas, monospace` — for hashes, addresses, code, stats
- **Font file**: `/fonts/Favorit-Regular.otf` loaded via @font-face for weights 300 and 400
- **Anti-aliasing**: `-webkit-font-smoothing: antialiased`

### Hero Typography Scale (solana.com style)
- Hero headlines: `text-5xl sm:text-6xl md:text-7xl` (4rem → 4.5rem) — font-medium, tight tracking
- Section titles: `text-3xl sm:text-4xl` — font-medium
- Section labels: `text-xs uppercase tracking-[0.2em] font-mono text-arc-aquarius`
- Body: `text-base sm:text-lg` — text-arc-grey-500, max-w-2xl
- Stat numbers: `text-7xl sm:text-8xl md:text-9xl` for hero stats

### Buttons (CRITICAL — zero border-radius)
```css
.btn-arc {
  /* White bg, black border, inverts on hover */
  background: #FFFFFF;
  color: #03030A;
  border: 1px solid #03030A;
  border-radius: 0;           /* SHARP CORNERS — never round */
  padding: 0.625rem 1.25rem;
  font-weight: 500;
  font-size: 0.875rem;
  transition: all 150ms;
}
.btn-arc:hover {
  background: #03030A;
  color: #FFFFFF;
  border-color: #FFFFFF;
}

.btn-arc-outline {
  /* Transparent bg, subtle border, aquarius on hover */
  background: transparent;
  color: #FFFFFF;
  border: 1px solid #1E1E2E;
  border-radius: 0;
  /* Same padding/size as btn-arc */
}
.btn-arc-outline:hover {
  border-color: #6F7CF4;
  color: #6F7CF4;
}
```

### Logo
- SVG at `/brand/arc-logo-white.svg` (white wordmark "arc" in lowercase)
- PNG fallback at `/brand/arc-logo-white.png`
- Tagline: **"ai for Humans First"**

---

## DESIGN PATTERNS (solana.com level)

### 1. Gradient Mesh Backgrounds
Hero sections use 2-3 large blurred circles positioned absolutely:
```tsx
<div className="absolute w-[600px] h-[600px] rounded-full opacity-[0.07]"
  style={{ background: '#6F7CF4', filter: 'blur(120px)', top: '-10%', left: '15%' }} />
<div className="absolute w-[500px] h-[500px] rounded-full opacity-[0.05]"
  style={{ background: '#002DDE', filter: 'blur(120px)', top: '20%', right: '10%' }} />
```
Plus a faint grid overlay at `opacity-[0.03]` with 60px spacing.

### 2. Scroll-Reveal Animations
Every section uses IntersectionObserver-based reveal:
```tsx
function useReveal() {
  // threshold: 0.05, rootMargin: '0px 0px -50px 0px'
  // transition: opacity 0.7s cubic-bezier(0.16,1,0.3,1), transform 0.7s same
  // from: opacity:0, translateY(24px) → to: opacity:1, translateY(0)
}
```
Stagger children with increasing `delay` props (0.05s increments).

### 3. Card Design
- Border: `border border-arc-border`
- Background: `bg-arc-surface-raised`
- Padding: `p-5` or `p-6`
- Hover: `hover:border-arc-aquarius/30 transition-colors duration-200`
- Glow effect: pseudo-element with gradient overlay on hover
- **Zero border-radius** on everything

### 4. Section Rhythm
Each major section:
```
py-24 sm:py-32        (generous vertical padding)
border-t border-arc-border  (section divider)
max-w-7xl mx-auto px-4 sm:px-6  (content width)
```
Section header pattern:
```
<SectionLabel>Architecture</SectionLabel>     ← xs, uppercase, mono, aquarius
<SectionTitle>Five layers. One chain.</SectionTitle>  ← 3xl/4xl, white, medium
<SectionSubtitle>Description text...</SectionSubtitle> ← base/lg, grey-600
```

### 5. Stats Counters
Large monospace numbers with small labels beneath:
```tsx
<div className="text-7xl sm:text-8xl md:text-9xl font-medium tracking-tight text-gradient">
  27,000+
</div>
<p className="text-base text-arc-grey-600 mt-4">sustained TPS</p>
```

### 6. Horizontal Stat Pills
```tsx
<div className="flex flex-wrap gap-3">
  {[['27K+', 'TPS'], ['1,028', 'Tests'], ['21', 'TX Types']].map(([v, l]) => (
    <div className="flex items-center gap-2.5 px-4 py-2 border border-arc-border bg-arc-surface/60 backdrop-blur-sm">
      <span className="text-sm font-medium text-arc-white font-mono">{v}</span>
      <span className="text-xs text-arc-grey-700">{l}</span>
    </div>
  ))}
</div>
```

### 7. Network Status Bar
Thin bar below header: green pulsing dot + "Network Active" + block height when connected, red dot + "Disconnected" when not. Polls `/health` every 8 seconds.

### 8. Mobile Hamburger Nav
At `md:` breakpoint: hamburger icon → slide-out panel from right with backdrop blur. Close on outside click, route change, or X button.

---

## ROUTES & PAGES

| Route | Page | Title |
|-------|------|-------|
| `/` | Home | ARC scan — Chain Overview |
| `/blockchain` | Blockchain | ARC — The chain built for AI |
| `/blocks` | Blocks | ARC scan — Blocks |
| `/block/:height` | BlockDetail | ARC scan — Block #N |
| `/tx/:hash` | Transaction | ARC scan — Transaction |
| `/account/:address` | Account | ARC scan — Account |
| `/faucet` | Faucet | ARC scan — Testnet Faucet |
| `/validators` | Validators | ARC scan — Validators |

---

## API LAYER (`src/api.ts`)

Base URL: `import.meta.env.VITE_API_URL || 'http://localhost:9090'`

Custom `ApiError` class with `status` property. Generic `request<T>(path)` function.

### Endpoints:

```typescript
getHealth()         → GET /health         → HealthResponse
getInfo()           → GET /info           → InfoResponse
getNodeInfo()       → GET /node/info      → NodeInfoResponse
getStats()          → GET /stats          → StatsResponse
getBlocks(from, to, limit) → GET /blocks?from=X&to=Y&limit=Z → BlocksResponse
getBlock(height)    → GET /block/{height} → BlockDetail
getTx(hash)         → GET /tx/{hash}      → TxReceipt
getTxProof(hash)    → GET /tx/{hash}/proof → TxProof
getFullTx(hash)     → GET /tx/{hash}/full → FullTransaction
getAccount(address) → GET /account/{address} → AccountInfo
getAccountTxs(addr) → GET /account/{address}/txs → AccountTxsResponse
getContractInfo(addr) → GET /contract/{address} → ContractInfo
callContract(addr, fn, calldata?, from?, gas?) → POST /contract/{address}/call → ContractCallResult
getValidators()     → GET /validators     → ValidatorsResponse
```

### Faucet endpoints (same base URL):
```
POST /faucet/claim   body: { address: string } → { tx_hash, amount, message } | { error }
GET  /faucet/status  → { address, node_url, claims_today, claim_amount, rate_limit_secs }
```

---

## TYPE DEFINITIONS (`src/types.ts`)

```typescript
// Health & Info
interface HealthResponse {
  status: string; version: string; height: number; peers: number; uptime_secs: number;
}
interface InfoResponse {
  chain: string; version: string; block_height: number; account_count: number;
  mempool_size: number; gpu: string | { available: boolean; name: string; backend: string };
}
interface NodeInfoResponse {
  validator: string; stake: number; tier: string; height: number; version: string; mempool_size: number;
}
interface StatsResponse {
  chain: string; version: string; block_height: number; total_accounts: number;
  mempool_size: number; total_transactions: number; indexed_hashes: number; indexed_receipts: number;
}

// Blocks
interface BlockSummary {
  height: number; hash: string; parent_hash: string; tx_root: string;
  tx_count: number; timestamp: number; producer: string;
}
interface BlocksResponse { from: number; to: number; limit: number; count: number; blocks: BlockSummary[]; }
interface BlockHeader {
  height: number; timestamp: number; parent_hash: string; tx_root: string;
  state_root: string; proof_hash: string; tx_count: number; producer: string;
}
interface BlockDetail { header: BlockHeader; tx_hashes: string[]; hash: string; }

// Transactions
interface TxReceipt {
  tx_hash: string; block_height: number; block_hash: string; index: number;
  success: boolean; gas_used: number; value_commitment: string | null;
  inclusion_proof: string | number[] | null;
}
interface TxProof {
  tx_hash: string; block_height: number; merkle_root: string;
  proof_nodes: string[]; index: number; verified: boolean;
}

// Full Transaction (21 TX types)
interface FullTransaction {
  tx_hash: string; tx_type: string; from: string; nonce: number; fee: number;
  gas_limit: number; body: TransactionBody;
  block_height?: number; block_hash?: string; index?: number; success?: boolean; gas_used?: number;
}
type TransactionBody =
  | { type: 'Transfer'; to: string; amount: number; amount_commitment: string | null }
  | { type: 'Settle'; agent_id: string; service_hash: string; amount: number; usage_units: number }
  | { type: 'Swap'; counterparty: string; offer_amount: number; receive_amount: number; offer_asset: string; receive_asset: string }
  | { type: 'Escrow'; beneficiary: string; amount: number; conditions_hash: string; is_create: boolean }
  | { type: 'Stake'; amount: number; is_stake: boolean; validator: string }
  | { type: 'WasmCall'; contract: string; function: string; calldata: string; value: number; gas_limit: number }
  | { type: 'MultiSig'; signers: string[]; threshold: number }
  | { type: 'DeployContract'; bytecode_size: number; constructor_args_size: number; state_rent_deposit: number }
  | { type: 'RegisterAgent'; agent_name: string; endpoint: string; protocol: string; capabilities_size: number }
  | { type: 'JoinValidator'; pubkey: number[]; initial_stake: number }
  | { type: 'LeaveValidator' }
  | { type: 'ClaimRewards' }
  | { type: 'UpdateStake'; new_stake: number }
  | { type: 'Governance'; proposal_id: number; action: string }
  | { type: 'BridgeLock'; destination_chain: number; destination_address: number[]; amount: number }
  | { type: 'BridgeMint'; source_chain: number; source_tx_hash: string; recipient: string; amount: number; merkle_proof: number[] }
  | { type: 'BatchSettle'; entries: Array<{ agent_id: string; service_hash: string; amount: number }> }
  | { type: 'ChannelOpen'; channel_id: string; counterparty: string; deposit: number; timeout_blocks: number }
  | { type: 'ChannelClose'; channel_id: string; opener_balance: number; counterparty_balance: number; counterparty_sig: number[]; state_nonce: number }
  | { type: 'ChannelDispute'; channel_id: string; opener_balance: number; counterparty_balance: number; other_party_sig: number[]; state_nonce: number; challenge_period: number }
  | { type: 'ShardProof'; shard_id: number; block_height: number; block_hash: string; prev_state_root: string; post_state_root: string; tx_count: number; proof_data: number[] };

// Contracts
interface ContractInfo { address: string; bytecode_size: number; code_hash: string; is_wasm: boolean; }
interface ContractCallResult {
  success: boolean; gas_used?: number; return_data?: string;
  logs?: string[]; events?: Array<{ topic: string; data: string }>; error?: string;
}

// Accounts
interface AccountInfo { balance: number; nonce: number; address?: string; [key: string]: unknown; }
interface AccountTxsResponse { address: string; tx_count: number; tx_hashes: string[]; }

// Validators
interface ValidatorInfo { address: string; stake: number; tier: string; }
interface ValidatorsResponse { validators: ValidatorInfo[]; total_stake: number; count: number; }
```

---

## UTILITY FUNCTIONS (`src/utils.ts`)

```typescript
truncateHash(hash, prefixLen=6, suffixLen=4) → "0x1234...abcd"
formatHash(hash) → prepends "0x" if missing
timeAgo(timestamp) → "5s ago", "3m ago", "2h ago", "1d ago", "Genesis"
formatTimestamp(timestamp) → locale string from unix seconds
formatNumber(n) → "1.2B", "1.5M", or "1,234"
detectSearchType(input) → "block" | "tx" | "account" | "unknown"
  - digits only → block
  - 64 hex chars → tx
  - 8+ hex chars → account
copyToClipboard(text) → Promise<boolean>
```

---

## COMPONENTS

### Layout.tsx
- **Network Status Bar**: Thin bar at very top. Green pulsing dot + "Network Active · Block #N" when connected, red dot + "Disconnected" when not. Polls `getHealth()` every 8s.
- **Header**: Sticky. Left: ARC logo (SVG) + "scan" text. Center: SearchBar component. Right: Desktop nav links (`Home | Blockchain | Blocks | Validators | Faucet`), hidden on mobile. Hamburger button on mobile.
- **Mobile Nav**: Slide-out panel from right with backdrop. Links + close button. Auto-closes on route change.
- **Content**: `<Outlet />` wrapped in `max-w-5xl mx-auto px-4 sm:px-6 py-8` with `animate-fade-in` class.
- **Footer**: 4-column grid with link lists. External links get arrow icon. Bottom row: ARC logo + tagline + copyright. Separated from content by `border-t border-arc-border`.
  - **Explorer**: Home, Blocks, Validators, Faucet (internal links)
  - **Developers**: Documentation ↗, GitHub ↗, API Reference ↗, Smart Contracts
  - **ARC Ecosystem**: Matrix (matrix.arc.ai) ↗, Reactor (reactor.arc.ai) ↗, $ARC Token (arc.ai) ↗, Protocol
  - **Community**: X/Twitter (x.com/ARCreactorAI) ↗, Discord ↗, Telegram (t.me/JoinTheARC) ↗, LinkedIn ↗

### SearchBar.tsx
- Input with magnifying glass icon and "/" keyboard shortcut badge
- Type detection hint: shows "Block height" / "Transaction hash" / "Account address" as user types
- Recent searches in localStorage (`arc-recent-searches`, max 5), shown in dropdown when focused with empty input
- Navigate to `/block/:height`, `/tx/:hash`, or `/account/:address` on submit
- Close dropdown on Escape, outside click

### StatsGrid.tsx
- 2x2 grid of stat cards (`grid-cols-2 md:grid-cols-4`)
- Each card: label (xs, uppercase, tracking-widest, grey), value (2xl, mono, white), optional suffix
- Optional sparkline in top-right corner of card (MiniChart component)
- Skeleton state when loading

### MiniChart.tsx
- Pure SVG sparkline (48x24 default, customizable)
- Polyline path with configurable stroke color
- Gradient fill beneath the line (color → transparent)
- Auto-scales to data range with 10% padding

### BlocksTable.tsx
- Columns: HEIGHT, HASH, TXNS, TIME, PRODUCER (producer hidden on mobile)
- Hash and producer are clickable Links (to block detail / account)
- TimeAgo component for timestamps
- Skeleton rows when loading
- "No blocks found" empty state

### TxTable.tsx
- Columns: TX HASH, optional BLOCK, optional INDEX
- Hash links to `/tx/:hash`
- Compact mode for homepage (fewer columns)

### Badge.tsx
- Variants: success, error, warning, info, neutral
- Two sizes: sm (text-[10px] px-2 py-0.5) and md (text-xs px-2.5 py-1)
- Colored background at low opacity + colored text

### TimeAgo.tsx
- Shows relative time, updates every 10 seconds
- Full timestamp on hover (title attribute)

### CopyButton.tsx
- Small icon button (w-7 h-7) with clipboard SVG icon
- On copy: switches to checkmark, shows "Copied" mini-toast above, scales up slightly
- Reverts after 2 seconds

### ContractInteraction.tsx
- Read-only WASM contract call form
- Fields: function name, calldata (hex), from address, gas limit
- Submit calls `callContract()` API
- Shows return data, gas used, logs, events, or error

---

## PAGE SPECIFICATIONS

### Home.tsx (`/`)
**Purpose**: Live dashboard + onboarding

**Data**: Polls `getStats()`, `getHealth()`, `getInfo()`, `getBlocks()` every 5 seconds. Calculates live TPS from recent block timestamps. Tracks TPS and block time history arrays for sparklines.

**Layout**:
1. **Hero**: "ARC scan" title with gradient text + subtitle showing chain version and total transactions
2. **Error banner** if node disconnected (red border, low opacity bg)
3. **Stats grid**: Live TPS (with sparkline), Total Transactions, Network Nodes, Block Height (with sparkline)
4. **Latest Blocks** section: heading + "View all blocks" outline button → BlocksTable (10 rows, compact)
5. **Latest Transactions** section: TxTable from latest block's tx_hashes
6. **Get Started CTA**: 3 cards in a row — "Run a Node" (aquarius), "Build on ARC" (blue), "Join Community" (green). Each has SVG icon, title, description, hover border accent.
7. **Node Status**: If health/info available, show status, version, GPU, uptime, peers in a grid card.

### Blockchain.tsx (`/blockchain`)
**Purpose**: This is the **solana.com equivalent** — the marketing/technology showcase page. This is where VCs, developers, and community members learn what ARC is and why it matters. It must tell the full story: vision → architecture → performance → cryptography → developer tools → call to action.

**Design**: Full-bleed sections (negative margins to break out of Layout container). Each section has scroll-reveal animations. Generous whitespace (py-24 sm:py-32 per section). Gradient mesh backgrounds on hero and footer CTA. Faint grid overlays.

**Sections** (10 total):

1. **Hero**: The opening statement. Gradient mesh background with faint grid overlay.
   - Status pill: green dot + "Privacy-First L1 · STARK-Proven · GPU-Verified"
   - Headline: **"Privacy-first AI infrastructure, on-chain."** (text-5xl → text-7xl, font-medium, tight tracking). "on-chain" in gradient text.
   - Subhead: "The Protocol powering Matrix, Reactor, and $ARC. 21 native transaction types. GPU-accelerated verification. STARK-proven state. Architected for 1B+ TPS."
   - Two CTAs: "Explore Chain →" (btn-arc) + "Read Docs →" (btn-arc-outline)
   - Stat pills row: `1B+` Effective TPS / `27K+` Base TPS / `21` TX Types / `1.03B` $ARC Supply / `72K+` LOC Rust

2. **The Vision** (the "why"): Section label "WHY ARC PROTOCOL". Title: "AI needs a chain built for privacy."
   - 2-column layout: left is narrative text, right is a visual showing the scaling layers
   - Left text: "ARC started with the products — Matrix for encrypted AI, Reactor for fast AI assistance. To make privacy guarantees provable and trustless, we built the Protocol from scratch. No fork. No EVM clone. A completely new L1 where encrypted compute attestation, wallet-native identity, and compliance provability are all built in."
   - Right: 3 stacked scaling layer cards showing Base L1 (27K) → BatchSettle (27M) → Channels + Shards (1B+) with a visual multiplier effect

3. **Architecture**: Section label "ARCHITECTURE". Title: "Six layers. One chain."
   - 6 cards in a row (now including Scaling as 6th layer): Consensus, Execution, State, Network, Cryptography, Scaling
   - Each card: unicode icon, layer name, description, "Layer N" footer
   - The Scaling card should be visually highlighted (aquarius border glow) as it's the differentiator

4. **Privacy & Identity** (the differentiator): Section label "PRIVACY INFRASTRUCTURE". Title: "Encrypted compute, verified on-chain."
   - Show how Matrix's privacy guarantees map to ARC Chain TX types:
     - **RegisterAgent** → Wallet-native identity for Matrix users and AI agents (no username/password)
     - **Settle** → Encrypted compute attestation (proof that data stayed encrypted-in-use)
     - **BatchSettle** → 1000:1 netting for high-volume encrypted AI workloads
   - Visual flow: User sends encrypted query → Matrix processes in secure environment → ARC Chain records attestation → Tamper-proof audit trail
   - Key message: "Every privacy guarantee is cryptographically verified on-chain. Not just encrypted — proven."

5. **Transaction Types**: Section label "PROTOCOL". Title: "21 native operations. Not smart contracts."
   - Grouped by category (Core, Agent Economy, Staking, Governance, Bridge, L1 Scaling) with color-coded labels
   - Each TX card: name (mono font), gas cost, one-line description
   - Bottom pill: "21 transaction types · all native · zero ABI"

6. **Performance**: Section label "PERFORMANCE". Title: "Architected for 1 billion+ TPS."
   - Giant gradient number: **"1,000,000,000+"** (this is the hero number, not 27K)
   - Subtitle: "effective transactions per second through native L1 scaling"
   - 4 stat cards in a row:
     - 27K+ (Base L1 TPS — Block-STM parallel execution)
     - 27M (With BatchSettle — 1000:1 netting)
     - 100M+ (With State Channels — bilateral off-chain)
     - 1B+ (With Shard Proofs — horizontal STARK scaling)
   - Feature list: Block-STM, GPU Ed25519, Sharded state, Propose-verify pipeline

7. **Cryptography**: Section label "CRYPTOGRAPHY". Title: "State-of-the-art. Every layer."
   - 6 cards (Stwo STARK, GPU Ed25519, BLS Threshold, VRF, Poseidon Hash, Reed-Solomon FEC)
   - Each with a prominent stat number in aquarius and description

8. **Developer Tools**: Section label "BUILD ON ARC". Title: "Everything you need."
   - 4 cards: Python SDK (`pip install arc-sdk`), TypeScript SDK (`npm install @arc-chain/sdk`), Smart Contracts (Solidity + WASM), Explorer & Faucet (scan.arc.ai)
   - Each with install command in mono, language badge, description

9. **Ecosystem** (the credibility section): Section label "ECOSYSTEM". Title: "The Protocol powering ARC."
   - Subtitle: "ARC Chain is the infrastructure layer connecting Matrix, Reactor, and the $ARC token economy."
   - 3 large product cards (full width, stacked or 3-col):
     - **Matrix by ARC**: "The Privacy Shield for AI — encrypted-in-use processing, wallet-enforced access, compliance attestation for enterprise." Link: matrix.arc.ai. Badge: "Beta"
     - **Reactor by ARC**: "Fast, efficient AI assistant — chat, deep search, image gen, code, APIs. Mk. III with subscription model." Link: reactor.arc.ai. Badge: "Live"
     - **$ARC Token**: "1.03B supply · 85% community · No vesting · Access, Operate, Govern across the ecosystem." Badge: "Live on Ethereum"
   - Bottom: ARC Labs AG · Zug, Switzerland · Established 2024
   - Key message: "ARC didn't start with a chain. We started with the products, then built the Protocol to make privacy provable."

10. **Footer CTA**: Gradient mesh background. Centered text.
   - Title: "The first L1 for the agent economy."
   - Subtitle: "Start building on ARC today."
   - Two buttons: "Open Explorer" + "Get Test Tokens"

### Blocks.tsx (`/blocks`)
Paginated block list. Fetches `getBlocks()` with from/to params. Shows BlocksTable with full columns. Page N of M navigation with Previous/Next buttons.

### BlockDetail.tsx (`/block/:height`)
Single block detail. Shows: block height, hash, parent hash, state root, tx root, proof hash, producer, timestamp, tx count. Below: paginated list of transaction hashes as links.

### Transaction.tsx (`/tx/:hash`)
Fetches receipt, full TX, and proof in parallel. Shows:
- **Overview**: tx hash (with copy), status badge (success/fail), block height link, index, gas used, fee
- **Transaction Body**: Renders fields based on `body.type` (21 variants). Each TX type has custom field display.
- **Merkle Proof**: If proof exists, shows root, nodes list, verification status badge
- **Raw Data**: Collapsible JSON view of full transaction
- **Contract Interaction**: If TX is WasmCall, show ContractInteraction component

### Account.tsx (`/account/:address`)
Shows address (with copy), balance, nonce. Below: list of transaction hashes with TxTable.

### Validators.tsx (`/validators`)
Shows validator table: rank, address (linked), stake, tier (badge). Above table: stake distribution horizontal bar chart — stacked segments colored by tier, with legend showing name/stake/percentage. Back-to-home link on error.

### Faucet.tsx (`/faucet`)
- Title: "Testnet Faucet" with gradient "Testnet"
- Form: wallet address input (64 hex chars), "Request 10,000 ARC" submit button
- Result: success (green) or error (red) with tx hash link
- "How to Get Tokens" 3-step guide: Install CLI → Generate wallet → Paste address
- Faucet Status card: claim amount, claims today, rate limit, faucet address
- Recent Claims table: time, address, tx hash (client-side only, current session)

---

## ANIMATIONS & MICRO-INTERACTIONS

```css
/* Page fade-in */
@keyframes fade-in {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: translateY(0); }
}

/* Network status dot pulse */
@keyframes pulse-dot {
  0%, 100% { opacity: 1; transform: scale(1); }
  50% { opacity: 0.6; transform: scale(1.3); }
}

/* Mobile nav slide-in */
@keyframes slide-in {
  from { transform: translateX(100%); }
  to { transform: translateX(0); }
}

/* Copy toast */
@keyframes toast-in {
  from { opacity: 0; transform: translateY(4px); }
  to { opacity: 1; transform: translateY(0); }
}

/* Skeleton loading */
@keyframes skeleton-pulse {
  0%, 100% { opacity: 0.04; }
  50% { opacity: 0.08; }
}

/* Scroll reveal (JS-driven) */
transition: opacity 0.7s cubic-bezier(0.16,1,0.3,1), transform 0.7s same;
from: opacity:0 translateY(24px) → to: opacity:1 translateY(0)
```

### Additional CSS:
- Custom scrollbar: 6px, transparent track, arc-border thumb
- Selection color: `rgba(56, 85, 233, 0.3)`
- Focus-visible: 2px solid arc-aquarius outline
- Table row hover: `rgba(111, 124, 244, 0.04)` background
- Card glow: pseudo-element with gradient overlay on hover
- Sparkline paths: `fill: none; stroke-width: 1.5; stroke-linecap: round;`
- Stat values: `transition: all 300ms ease-out` for smooth number changes

---

## FILE STRUCTURE

```
explorer/
├── index.html
├── package.json
├── tsconfig.json
├── vite.config.ts
├── public/
│   ├── brand/
│   │   ├── arc-logo-white.svg
│   │   ├── arc-logo-white.png
│   │   ├── arc-wordmark.png
│   │   └── arc-brand-tokens.json
│   └── fonts/
│       └── Favorit-Regular.otf
└── src/
    ├── main.tsx              (BrowserRouter wrapper)
    ├── App.tsx               (Routes)
    ├── api.ts                (API client)
    ├── types.ts              (TypeScript interfaces)
    ├── utils.ts              (Utility functions)
    ├── index.css             (Global styles + Tailwind @theme)
    ├── vite-env.d.ts
    ├── components/
    │   ├── Layout.tsx
    │   ├── SearchBar.tsx
    │   ├── StatsGrid.tsx
    │   ├── MiniChart.tsx
    │   ├── BlocksTable.tsx
    │   ├── TxTable.tsx
    │   ├── Badge.tsx
    │   ├── TimeAgo.tsx
    │   ├── CopyButton.tsx
    │   └── ContractInteraction.tsx
    └── pages/
        ├── Home.tsx
        ├── Blockchain.tsx
        ├── Blocks.tsx
        ├── BlockDetail.tsx
        ├── Transaction.tsx
        ├── Account.tsx
        ├── Faucet.tsx
        └── Validators.tsx
```

---

## CRITICAL QUALITY BAR

1. **Zero TypeScript errors** — `npx tsc --noEmit` must pass clean
2. **Production build passes** — `npm run build` with zero warnings
3. **Mobile responsive** — Every page works at 375px width
4. **Accessible** — Focus rings, aria labels, keyboard navigation
5. **Performance** — No unnecessary re-renders, memo where needed
6. **Empty/Error states** — Every data section handles loading, error, and empty gracefully
7. **Consistent design** — Every element follows the brand system exactly. Zero border-radius on ALL interactive elements.
8. **solana.com energy** — Bold typography, generous whitespace, gradient meshes, scroll animations, massive stat numbers

---

## VITE CONFIG

```typescript
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import path from 'path';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  server: { port: 3100, host: '127.0.0.1' },
});
```

---

## WHAT "SOLANA.COM LEVEL" MEANS

This is NOT a basic data explorer. It's a **brand experience** that happens to have explorer functionality. It needs to make someone go "holy shit" when they visit. Specifically:

1. **The `/blockchain` page IS the homepage** — it's the solana.com equivalent. A visitor's first impression of ARC Protocol. It must convey: "this is the privacy-first L1 powering Matrix, Reactor, and the $ARC token, architected for 1B+ TPS." The page tells a story through 10 scroll-reveal sections.

2. **The narrative is the product** — ARC's story is: We built Matrix (encrypted AI) and Reactor (fast AI) → We needed privacy guarantees to be provable and trustless → No chain was built for encrypted compute attestation → So we built ARC Protocol from scratch → 21 native TX types, GPU-verified, STARK-proven, 1B+ TPS → The chain completes the ecosystem. The Blockchain page walks through this narrative visually.

3. **The "1B+ TPS" is the hook** — This is the number that makes people stop scrolling. Not 27K (that's base). The effective throughput with BatchSettle (1000:1 netting) + State Channels + Shard Proofs pushes to 1B+. Show the scaling layers visually: Base → Batch → Channels → Shards, with multipliers.

4. **"Privacy Infrastructure" is the differentiator** — ARC is not another general-purpose L1. It's purpose-built for encrypted compute attestation. Matrix's encrypted-in-use processing, wallet-enforced access, and compliance audit trails are all verified on ARC Chain. No other chain was designed for this.

5. **The `/` page is the live dashboard** — real-time stats, sparklines, latest blocks/txs. This is the Etherscan equivalent for developers who are already building.

6. **Every page feels premium** — consistent spacing (py-24 sections), generous padding, subtle animations, monospace accents for technical credibility. Zero border-radius everywhere.

7. **The brand is enforced everywhere** — #03030A background, zero border-radius, Favorit font, aquarius/purple gradient accents, "ai for Humans First" tagline.

8. **Technical credibility through specificity** — Don't say "fast". Say "27,000+ TPS sustained on M4 MacBook Pro". Don't say "secure". Say "Stwo STARK proofs with 22-constraint AIR over M31 field". Don't say "scalable". Say "BatchSettle nets 1,000 agent transactions into 1 on-chain TX". Specific numbers and mechanism names build trust.

Build every file. Make it incredible. Make someone want to invest in this.
