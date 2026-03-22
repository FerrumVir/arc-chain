import { useRef, useState, useEffect, type ReactNode } from 'react';
import { Link } from 'react-router-dom';

/* ═══════════════════════════════════════════════════════════════════════════
   Scroll-Reveal primitives
   ═══════════════════════════════════════════════════════════════════════════ */

function useReveal() {
  const ref = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true);
          observer.disconnect();
        }
      },
      { threshold: 0.05, rootMargin: '0px 0px -50px 0px' },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return { ref, visible };
}

function Reveal({
  children,
  delay = 0,
}: {
  children: ReactNode;
  delay?: number;
}) {
  const { ref, visible } = useReveal();
  return (
    <div
      ref={ref}
      style={{
        opacity: visible ? 1 : 0,
        transform: visible ? 'translateY(0)' : 'translateY(24px)',
        transition: `opacity 0.7s cubic-bezier(0.16,1,0.3,1) ${delay}s, transform 0.7s cubic-bezier(0.16,1,0.3,1) ${delay}s`,
      }}
    >
      {children}
    </div>
  );
}

/* ═══════════════════════════════════════════════════════════════════════════
   Shared tiny components
   ═══════════════════════════════════════════════════════════════════════════ */

function SectionLabel({ children }: { children: ReactNode }) {
  return (
    <p className="text-xs uppercase tracking-[0.2em] text-arc-aquarius font-mono mb-4">
      {children}
    </p>
  );
}

function SectionTitle({ children }: { children: ReactNode }) {
  return (
    <h2 className="text-3xl sm:text-4xl font-medium tracking-tight text-arc-white mb-3">
      {children}
    </h2>
  );
}

function SectionSubtitle({ children }: { children: ReactNode }) {
  return (
    <p className="text-base sm:text-lg text-arc-grey-600 max-w-2xl">
      {children}
    </p>
  );
}

/* ═══════════════════════════════════════════════════════════════════════════
   Data
   ═══════════════════════════════════════════════════════════════════════════ */

const LAYERS = [
  {
    icon: '\u25C8', // ◈
    name: 'Consensus',
    desc: 'DAG-based block structure with VRF proposer selection. Stake-weighted, deterministic leader rotation.',
  },
  {
    icon: '\u25B6', // ▶
    name: 'Execution',
    desc: 'Block-STM parallel execution with WASM virtual machine. Optimistic concurrency with abort-and-retry.',
  },
  {
    icon: '\u25A6', // ▦
    name: 'State',
    desc: 'Jellyfish Merkle Tree with incremental updates. WAL persistence and sharded cross-shard locking.',
  },
  {
    icon: '\u25CE', // ◎
    name: 'Network',
    desc: 'QUIC transport with FEC erasure coding and PEX peer discovery. Sub-second block propagation.',
  },
  {
    icon: '\u25C7', // ◇
    name: 'Cryptography',
    desc: 'Stwo STARK proofs, GPU Ed25519, BLS threshold signatures, Poseidon hashing. Every layer is proven.',
  },
];

interface TxType {
  name: string;
  gas: string;
  desc: string;
}

interface TxCategory {
  label: string;
  color: string;
  types: TxType[];
}

const TX_CATEGORIES: TxCategory[] = [
  {
    label: 'Core',
    color: 'text-arc-aquarius',
    types: [
      { name: 'Transfer', gas: '21k', desc: 'Send tokens between accounts' },
      { name: 'Settle', gas: '25k', desc: 'Finalize off-chain obligations' },
      { name: 'Swap', gas: '30k', desc: 'Atomic token exchange' },
      { name: 'Escrow', gas: '35k', desc: 'Time-locked conditional release' },
      { name: 'MultiSig', gas: '35k', desc: 'M-of-N authorization' },
    ],
  },
  {
    label: 'Agent Economy',
    color: 'text-arc-info',
    types: [
      { name: 'RegisterAgent', gas: '30k', desc: 'Onboard an AI agent identity' },
      { name: 'WasmCall', gas: '21k', desc: 'Invoke a WASM smart contract' },
      { name: 'DeployContract', gas: '53k', desc: 'Deploy WASM bytecode on-chain' },
    ],
  },
  {
    label: 'Staking',
    color: 'text-arc-success',
    types: [
      { name: 'Stake', gas: '25k', desc: 'Bond or unbond tokens to a validator' },
      { name: 'JoinValidator', gas: '30k', desc: 'Register as a validator with initial stake' },
      { name: 'LeaveValidator', gas: '25k', desc: 'Exit validator set and return stake' },
      { name: 'ClaimRewards', gas: '25k', desc: 'Claim epoch staking rewards' },
      { name: 'UpdateStake', gas: '25k', desc: 'Increase or decrease validator stake' },
    ],
  },
  {
    label: 'Governance',
    color: 'text-arc-warning',
    types: [
      { name: 'Governance', gas: '50k', desc: 'Submit or vote on proposals' },
    ],
  },
  {
    label: 'Bridge',
    color: 'text-arc-purple',
    types: [
      { name: 'BridgeLock', gas: '50k', desc: 'Lock tokens for cross-chain transfer' },
      { name: 'BridgeMint', gas: '50k', desc: 'Mint bridged tokens from proof' },
    ],
  },
  {
    label: 'L1 Scaling',
    color: 'text-arc-blue',
    types: [
      { name: 'BatchSettle', gas: '30k', desc: 'Settle a batch of L2 transactions' },
      { name: 'ChannelOpen', gas: '40k', desc: 'Open a state channel' },
      { name: 'ChannelClose', gas: '35k', desc: 'Cooperatively close a channel' },
      { name: 'ChannelDispute', gas: '50k', desc: 'Submit dispute proof' },
      { name: 'ShardProof', gas: '60k', desc: 'Cross-shard state proof' },
      { name: 'InferenceAttestation', gas: '30k', desc: 'AI inference result attestation with bond' },
      { name: 'InferenceChallenge', gas: '50k', desc: 'Challenge an inference attestation (fraud proof)' },
      { name: 'InferenceRegister', gas: '30k', desc: 'Register as inference provider (declare hardware tier)' },
    ],
  },
];

const CRYPTO_FEATURES = [
  {
    title: 'Stwo STARK Proofs',
    desc: 'Circle STARK over M31 field. 22-constraint AIR. Inner-circuit recursion for composable proof aggregation.',
    stat: '22',
    statLabel: 'AIR constraints',
  },
  {
    title: 'GPU Ed25519',
    desc: 'Metal + WGSL shaders. Branchless Shamir trick with 4-entry LUT. Zero SIMD divergence.',
    stat: '121K',
    statLabel: 'verifications/sec',
  },
  {
    title: 'BLS Threshold',
    desc: 'blst-based threshold signatures. Feldman VSS for verifiable key distribution. N-of-M threshold encryption.',
    stat: 'N-of-M',
    statLabel: 'threshold scheme',
  },
  {
    title: 'VRF Proposer Selection',
    desc: 'Ed25519-based verifiable random function. Weighted by stake. Cryptographically deterministic.',
    stat: 'VRF',
    statLabel: 'leader election',
  },
  {
    title: 'Poseidon Hash',
    desc: 'ZK-friendly algebraic hash. 2-to-1 Merkle hashing. Configurable full and partial rounds.',
    stat: '2:1',
    statLabel: 'Merkle hashing',
  },
  {
    title: 'Reed-Solomon FEC',
    desc: 'XOR erasure coding with 50% redundancy. Single-shred recovery for network resilience.',
    stat: '50%',
    statLabel: 'redundancy',
  },
];

const DEV_TOOLS = [
  {
    title: 'Python SDK',
    install: 'pip install arc-sdk',
    desc: 'ABI encoding, function selectors, keccak256 hashing. Full transaction builder.',
    lang: 'Python',
  },
  {
    title: 'TypeScript SDK',
    install: 'npm install @arc/sdk',
    desc: 'Full ABI encode/decode. Type-safe transaction construction and signing.',
    lang: 'TypeScript',
  },
  {
    title: 'Smart Contracts',
    install: 'Solidity + WASM',
    desc: 'ARC-20, ARC-721, ARC-1155 token standards. Deploy via WasmCall transaction.',
    lang: 'Solidity',
  },
  {
    title: 'Explorer & Faucet',
    install: 'explorer.arc.ai',
    desc: 'Real-time block explorer with testnet faucet. GPU verification status and TPS metrics.',
    lang: 'Web',
  },
];

/* ═══════════════════════════════════════════════════════════════════════════
   Main Page Component
   ═══════════════════════════════════════════════════════════════════════════ */

export default function Blockchain() {
  useEffect(() => {
    document.title = 'ARC Chain — The chain built for AI';
  }, []);

  return (
    <div className="-mx-4 sm:-mx-6 -mt-8">
      {/* ━━━ 1. HERO ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative overflow-hidden">
        {/* Gradient mesh background */}
        <div className="absolute inset-0 overflow-hidden pointer-events-none">
          <div
            className="absolute w-[600px] h-[600px] rounded-full opacity-[0.07]"
            style={{
              background: '#2563EB',
              filter: 'blur(120px)',
              top: '-10%',
              left: '15%',
            }}
          />
          <div
            className="absolute w-[500px] h-[500px] rounded-full opacity-[0.05]"
            style={{
              background: '#1E40AF',
              filter: 'blur(120px)',
              top: '20%',
              right: '10%',
            }}
          />
          <div
            className="absolute w-[400px] h-[400px] rounded-full opacity-[0.04]"
            style={{
              background: '#60A5FA',
              filter: 'blur(120px)',
              bottom: '0%',
              left: '40%',
            }}
          />
        </div>

        {/* Fine grid overlay */}
        <div
          className="absolute inset-0 pointer-events-none opacity-[0.03]"
          style={{
            backgroundImage:
              'linear-gradient(rgba(37,99,235,0.3) 1px, transparent 1px), linear-gradient(90deg, rgba(37,99,235,0.3) 1px, transparent 1px)',
            backgroundSize: '60px 60px',
          }}
        />

        <div className="relative max-w-7xl mx-auto px-4 sm:px-6 pt-20 sm:pt-32 pb-20 sm:pb-28">
          <Reveal>
            <div className="inline-flex items-center gap-2 px-3 py-1.5 border border-arc-border bg-arc-surface-raised/50 backdrop-blur-sm mb-8">
              <span className="w-1.5 h-1.5 rounded-full bg-arc-success animate-pulse" />
              <span className="text-xs text-arc-grey-500 font-mono tracking-wide">
                L1 Blockchain &middot; AI-Native &middot; STARK-Proven
              </span>
            </div>
          </Reveal>

          <Reveal delay={0.08}>
            <h1 className="text-5xl sm:text-6xl md:text-7xl font-medium tracking-tight leading-[1.05] mb-6 max-w-4xl">
              The chain built
              <br />
              for{' '}
              <span className="text-gradient">AI</span>.
            </h1>
          </Reveal>

          <Reveal delay={0.16}>
            <p className="text-lg sm:text-xl text-arc-grey-500 max-w-2xl mb-10 leading-relaxed">
              21 native transaction types. GPU-accelerated verification.
              Zero-knowledge state proofs. All at 27,000+ TPS.
            </p>
          </Reveal>

          <Reveal delay={0.24}>
            <div className="flex flex-wrap gap-3 mb-16">
              <Link to="/" className="btn-arc">
                Explore Chain <span aria-hidden="true">&rarr;</span>
              </Link>
              <a href="https://build-two-tau-96.vercel.app/docs/architecture" target="_blank" rel="noopener noreferrer" className="btn-arc-outline">
                Read Docs <span aria-hidden="true">&rarr;</span>
              </a>
            </div>
          </Reveal>

          <Reveal delay={0.32}>
            <div className="flex flex-wrap gap-3">
              {[
                ['27K+', 'TPS'],
                ['1,028', 'Tests'],
                ['21', 'TX Types'],
                ['72K+', 'LOC'],
              ].map(([value, label]) => (
                <div
                  key={label}
                  className="flex items-center gap-2.5 px-4 py-2 border border-arc-border bg-arc-surface/60 backdrop-blur-sm"
                >
                  <span className="text-sm font-medium text-arc-white font-mono">{value}</span>
                  <span className="text-xs text-arc-grey-700">{label}</span>
                </div>
              ))}
            </div>
          </Reveal>
        </div>

        {/* Bottom fade line */}
        <div className="absolute bottom-0 left-0 right-0 h-px bg-gradient-to-r from-transparent via-arc-border to-transparent" />
      </section>

      {/* ━━━ 2. ARCHITECTURE ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative py-24 sm:py-32">
        <div className="max-w-7xl mx-auto px-4 sm:px-6">
          <Reveal>
            <SectionLabel>Architecture</SectionLabel>
          </Reveal>
          <Reveal delay={0.05}>
            <SectionTitle>Five layers. One chain.</SectionTitle>
          </Reveal>
          <Reveal delay={0.1}>
            <SectionSubtitle>
              A vertically integrated stack from consensus to cryptography,
              each layer purpose-built for AI workloads.
            </SectionSubtitle>
          </Reveal>

          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-5 gap-4 mt-14">
            {LAYERS.map((layer, i) => (
              <Reveal key={layer.name} delay={0.08 * i}>
                <div className="card-glow border border-arc-border bg-arc-surface-raised p-5 h-full flex flex-col group transition-colors duration-200 hover:border-arc-aquarius/30">
                  <div className="text-2xl mb-4 text-arc-aquarius opacity-60 group-hover:opacity-100 transition-opacity">
                    {layer.icon}
                  </div>
                  <h3 className="text-sm font-medium text-arc-white mb-2 tracking-wide">
                    {layer.name}
                  </h3>
                  <p className="text-xs text-arc-grey-700 leading-relaxed flex-1">
                    {layer.desc}
                  </p>
                  <div className="mt-4 pt-3 border-t border-arc-border-subtle">
                    <span className="text-[10px] font-mono text-arc-grey-700 uppercase tracking-widest">
                      Layer {i + 1}
                    </span>
                  </div>
                </div>
              </Reveal>
            ))}
          </div>
        </div>
      </section>

      {/* ━━━ 3. TRANSACTION TYPES ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative py-24 sm:py-32 border-t border-arc-border">
        {/* Subtle bg tint */}
        <div className="absolute inset-0 bg-arc-surface/40 pointer-events-none" />

        <div className="relative max-w-7xl mx-auto px-4 sm:px-6">
          <Reveal>
            <SectionLabel>Transaction Types</SectionLabel>
          </Reveal>
          <Reveal delay={0.05}>
            <SectionTitle>21 native operations. Not smart contracts.</SectionTitle>
          </Reveal>
          <Reveal delay={0.1}>
            <SectionSubtitle>
              Every operation is a first-class citizen of the protocol.
              Deterministic gas, native verification, zero ABI overhead.
            </SectionSubtitle>
          </Reveal>

          <div className="mt-14 space-y-8">
            {TX_CATEGORIES.map((cat, catIdx) => (
              <Reveal key={cat.label} delay={0.04 * catIdx}>
                <div>
                  <h3
                    className={`text-xs font-mono uppercase tracking-[0.15em] mb-3 ${cat.color}`}
                  >
                    {cat.label}
                  </h3>
                  <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5 gap-2">
                    {cat.types.map((tx) => (
                      <div
                        key={tx.name}
                        className="border border-arc-border bg-arc-surface-raised px-4 py-3 group hover:border-arc-aquarius/20 transition-colors duration-150"
                      >
                        <div className="flex items-center justify-between mb-1">
                          <span className="text-sm font-medium text-arc-white font-mono">
                            {tx.name}
                          </span>
                          <span className="text-[10px] text-arc-grey-700 font-mono">
                            {tx.gas}
                          </span>
                        </div>
                        <p className="text-xs text-arc-grey-700 leading-relaxed">
                          {tx.desc}
                        </p>
                      </div>
                    ))}
                  </div>
                </div>
              </Reveal>
            ))}
          </div>

          {/* Total count pill */}
          <Reveal delay={0.4}>
            <div className="mt-10 flex justify-center">
              <div className="inline-flex items-center gap-2 px-4 py-2 border border-arc-border bg-arc-surface-raised">
                <span className="text-sm font-mono text-arc-aquarius font-medium">21</span>
                <span className="text-xs text-arc-grey-600">
                  transaction types &middot; all native &middot; zero ABI
                </span>
              </div>
            </div>
          </Reveal>
        </div>
      </section>

      {/* ━━━ 4. PERFORMANCE ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative py-24 sm:py-32 border-t border-arc-border">
        <div className="max-w-7xl mx-auto px-4 sm:px-6">
          <Reveal>
            <SectionLabel>Performance</SectionLabel>
          </Reveal>
          <Reveal delay={0.05}>
            <SectionTitle>Built for throughput.</SectionTitle>
          </Reveal>

          {/* Hero number */}
          <Reveal delay={0.12}>
            <div className="mt-12 mb-16 text-center">
              <div className="text-7xl sm:text-8xl md:text-9xl font-medium tracking-tight text-gradient leading-none">
                27,000+
              </div>
              <p className="text-base text-arc-grey-600 mt-4">
                sustained TPS on M4 MacBook Pro
              </p>
            </div>
          </Reveal>

          {/* Stats row */}
          <Reveal delay={0.2}>
            <div className="grid grid-cols-1 sm:grid-cols-3 gap-4 mb-16">
              {[
                { value: '350K', label: 'Peak TPS', sub: 'burst throughput' },
                { value: '500K', label: 'Transactions', sub: 'in benchmark run' },
                { value: '88.9s', label: 'GPU Verify', sub: '32 test passes' },
              ].map((s) => (
                <div
                  key={s.label}
                  className="border border-arc-border bg-arc-surface-raised p-6 text-center"
                >
                  <div className="text-3xl font-medium text-arc-white font-mono mb-1">
                    {s.value}
                  </div>
                  <div className="text-sm text-arc-grey-500">{s.label}</div>
                  <div className="text-xs text-arc-grey-700 mt-1">{s.sub}</div>
                </div>
              ))}
            </div>
          </Reveal>

          {/* Features list */}
          <Reveal delay={0.28}>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-x-12 gap-y-5 max-w-3xl">
              {[
                ['Block-STM parallel execution', 'Optimistic concurrency with abort-and-retry on conflicts'],
                ['GPU Ed25519 batch verification', 'Metal + WGSL shaders with branchless Shamir trick'],
                ['Sharded state with cross-shard locking', 'JMT partitions with incremental Merkle root updates'],
                ['Propose-verify pipeline', 'Bifurcated execution decouples block production from validation'],
              ].map(([title, desc]) => (
                <div key={title} className="flex gap-3">
                  <div className="w-1 shrink-0 bg-gradient-to-b from-arc-aquarius to-arc-purple rounded-full mt-1" style={{ height: '32px' }} />
                  <div>
                    <p className="text-sm font-medium text-arc-white mb-0.5">{title}</p>
                    <p className="text-xs text-arc-grey-700 leading-relaxed">{desc}</p>
                  </div>
                </div>
              ))}
            </div>
          </Reveal>
        </div>
      </section>

      {/* ━━━ 5. CRYPTOGRAPHY ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative py-24 sm:py-32 border-t border-arc-border">
        <div className="absolute inset-0 bg-arc-surface/40 pointer-events-none" />

        <div className="relative max-w-7xl mx-auto px-4 sm:px-6">
          <Reveal>
            <SectionLabel>Cryptography</SectionLabel>
          </Reveal>
          <Reveal delay={0.05}>
            <SectionTitle>State-of-the-art. Every layer.</SectionTitle>
          </Reveal>
          <Reveal delay={0.1}>
            <SectionSubtitle>
              Six cryptographic primitives, each chosen for its
              zero-knowledge compatibility and hardware acceleration potential.
            </SectionSubtitle>
          </Reveal>

          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4 mt-14">
            {CRYPTO_FEATURES.map((feat, i) => (
              <Reveal key={feat.title} delay={0.06 * i}>
                <div className="card-glow border border-arc-border bg-arc-surface-raised p-6 h-full flex flex-col group hover:border-arc-aquarius/30 transition-colors duration-200">
                  <div className="flex items-start justify-between mb-4">
                    <h3 className="text-sm font-medium text-arc-white tracking-wide">
                      {feat.title}
                    </h3>
                    <div className="text-right shrink-0 ml-3">
                      <span className="text-lg font-mono font-medium text-arc-aquarius">
                        {feat.stat}
                      </span>
                    </div>
                  </div>
                  <p className="text-xs text-arc-grey-700 leading-relaxed flex-1">
                    {feat.desc}
                  </p>
                  <div className="mt-4 pt-3 border-t border-arc-border-subtle">
                    <span className="text-[10px] font-mono text-arc-grey-700 uppercase tracking-widest">
                      {feat.statLabel}
                    </span>
                  </div>
                </div>
              </Reveal>
            ))}
          </div>
        </div>
      </section>

      {/* ━━━ 6. DEVELOPER TOOLS ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative py-24 sm:py-32 border-t border-arc-border">
        <div className="max-w-7xl mx-auto px-4 sm:px-6">
          <Reveal>
            <SectionLabel>Build on ARC</SectionLabel>
          </Reveal>
          <Reveal delay={0.05}>
            <SectionTitle>Everything you need.</SectionTitle>
          </Reveal>
          <Reveal delay={0.1}>
            <SectionSubtitle>
              SDKs, token standards, and tooling to ship AI-native applications on day one.
            </SectionSubtitle>
          </Reveal>

          <div className="grid grid-cols-1 sm:grid-cols-2 gap-4 mt-14">
            {DEV_TOOLS.map((tool, i) => (
              <Reveal key={tool.title} delay={0.06 * i}>
                <div className="card-glow border border-arc-border bg-arc-surface-raised p-6 h-full flex flex-col group hover:border-arc-aquarius/30 transition-colors duration-200">
                  <div className="flex items-center justify-between mb-3">
                    <h3 className="text-sm font-medium text-arc-white">
                      {tool.title}
                    </h3>
                    <span className="text-[10px] font-mono text-arc-grey-700 uppercase tracking-widest px-2 py-0.5 border border-arc-border">
                      {tool.lang}
                    </span>
                  </div>
                  <div className="font-mono text-xs text-arc-aquarius bg-arc-black/40 px-3 py-2 border border-arc-border-subtle mb-4">
                    {tool.install}
                  </div>
                  <p className="text-xs text-arc-grey-700 leading-relaxed flex-1">
                    {tool.desc}
                  </p>
                </div>
              </Reveal>
            ))}
          </div>

          <Reveal delay={0.3}>
            <div className="mt-10 text-center">
              <a href="https://build-two-tau-96.vercel.app/docs/architecture" target="_blank" rel="noopener noreferrer" className="btn-arc-outline">
                View Documentation <span aria-hidden="true">&rarr;</span>
              </a>
            </div>
          </Reveal>
        </div>
      </section>

      {/* ━━━ 7. FOOTER CTA ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */}
      <section className="relative border-t border-arc-border overflow-hidden">
        {/* Background gradient */}
        <div className="absolute inset-0 pointer-events-none">
          <div
            className="absolute w-[600px] h-[400px] rounded-full opacity-[0.04]"
            style={{
              background: '#2563EB',
              filter: 'blur(120px)',
              top: '10%',
              left: '20%',
            }}
          />
          <div
            className="absolute w-[500px] h-[400px] rounded-full opacity-[0.03]"
            style={{
              background: '#1E40AF',
              filter: 'blur(120px)',
              top: '10%',
              right: '20%',
            }}
          />
        </div>

        <div className="relative max-w-7xl mx-auto px-4 sm:px-6 py-24 sm:py-32 text-center">
          <Reveal>
            <h2 className="text-3xl sm:text-4xl md:text-5xl font-medium tracking-tight text-arc-white mb-4">
              Start building on ARC Chain.
            </h2>
          </Reveal>
          <Reveal delay={0.08}>
            <p className="text-base sm:text-lg text-arc-grey-600 mb-10 max-w-xl mx-auto">
              The first L1 designed for the AI economy.
            </p>
          </Reveal>
          <Reveal delay={0.16}>
            <div className="flex flex-wrap justify-center gap-3">
              <Link to="/" className="btn-arc">
                Open Explorer
              </Link>
              <Link to="/faucet" className="btn-arc-outline">
                Get Test Tokens
              </Link>
            </div>
          </Reveal>
        </div>
      </section>
    </div>
  );
}
