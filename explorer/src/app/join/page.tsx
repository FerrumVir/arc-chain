"use client";

import { useState } from "react";
import {
  Globe,
  Terminal,
  Shield,
  Zap,
  Cpu,
  HardDrive,
  Wifi,
  CheckCircle,
  Copy,
  ChevronDown,
  ChevronUp,
  ArrowRight,
  Download,
  Monitor,
  Apple,
} from "lucide-react";

const REQUIREMENTS = [
  { icon: Cpu, label: "CPU", value: "4+ cores (ARM or x86)", rec: "8+ cores recommended" },
  { icon: HardDrive, label: "RAM", value: "8 GB minimum", rec: "16 GB+ for validators" },
  { icon: HardDrive, label: "Disk", value: "50 GB SSD", rec: "NVMe preferred" },
  { icon: Wifi, label: "Network", value: "100 Mbps+", rec: "Low latency preferred" },
];

const STEPS = [
  {
    title: "Install ARC Chain",
    description: "Download the pre-built binary or build from source with Rust",
    commands: [
      { label: "macOS (Apple Silicon)", cmd: "curl -fsSL https://arc.chain/install.sh | sh" },
      { label: "Linux (x86_64)", cmd: "curl -fsSL https://arc.chain/install.sh | sh -s -- --arch x86_64" },
      { label: "Build from source", cmd: "git clone https://github.com/ARC-Chain/arc-chain.git && cd arc-chain && cargo build --release" },
    ],
  },
  {
    title: "Generate Node Identity",
    description: "Create a cryptographic keypair for your node",
    commands: [
      { label: "Generate keys", cmd: "arc-node keygen --output ~/.arc/node-key.json" },
    ],
  },
  {
    title: "Start Your Node",
    description: "Join the testnet and start syncing blocks",
    commands: [
      { label: "Start node", cmd: "arc-node start --testnet --rpc-port 8545 --p2p-port 30303" },
      { label: "Start as validator", cmd: "arc-node start --testnet --validator --stake 10000 --rpc-port 8545" },
    ],
  },
  {
    title: "Verify Connection",
    description: "Check that your node is syncing with the network",
    commands: [
      { label: "Check status", cmd: "arc-node status" },
      { label: "Check peers", cmd: "arc-node peers" },
      { label: "Check sync", cmd: "curl -s localhost:8545/info | jq ." },
    ],
  },
];

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);

  function handleCopy() {
    navigator.clipboard?.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }

  return (
    <button
      onClick={handleCopy}
      className="shrink-0 p-1.5 rounded-md hover:bg-[var(--border)] transition-colors"
    >
      {copied ? (
        <CheckCircle className="w-3.5 h-3.5 text-[var(--shield-green)]" />
      ) : (
        <Copy className="w-3.5 h-3.5 text-[var(--text-tertiary)]" />
      )}
    </button>
  );
}

function StepCard({
  step,
  index,
}: {
  step: (typeof STEPS)[0];
  index: number;
}) {
  const [expanded, setExpanded] = useState(index === 0);

  return (
    <div
      className="animate-slide-up card-flat overflow-hidden"
      style={{ animationDelay: `${index * 80}ms` }}
    >
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center gap-4 p-4 text-left hover:bg-[var(--bg)] transition-colors"
      >
        <div className="w-8 h-8 rounded-lg bg-[var(--accent)] flex items-center justify-center shrink-0">
          <span className="text-[13px] font-semibold text-white">
            {index + 1}
          </span>
        </div>
        <div className="flex-1 min-w-0">
          <div className="text-[14px] font-medium">{step.title}</div>
          <div className="text-[12px] text-[var(--text-secondary)]">
            {step.description}
          </div>
        </div>
        {expanded ? (
          <ChevronUp className="w-4 h-4 text-[var(--text-tertiary)] shrink-0" />
        ) : (
          <ChevronDown className="w-4 h-4 text-[var(--text-tertiary)] shrink-0" />
        )}
      </button>

      {expanded && (
        <div className="px-4 pb-4 flex flex-col gap-2">
          {step.commands.map(({ label, cmd }) => (
            <div key={cmd}>
              <span className="text-[11px] text-[var(--text-tertiary)] mb-1 block">
                {label}
              </span>
              <div className="flex items-center gap-2 bg-[var(--bg)] border border-[var(--border-light)] rounded-lg px-3 py-2.5">
                <Terminal className="w-3.5 h-3.5 text-[var(--accent)] shrink-0" />
                <code className="flex-1 text-[12px] font-hash text-[var(--text)] break-all select-all">
                  {cmd}
                </code>
                <CopyButton text={cmd} />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export default function JoinPage() {
  return (
    <div className="flex flex-col gap-6 max-w-[900px] mx-auto">
      {/* Hero */}
      <div className="text-center py-8">
        <div className="inline-flex items-center gap-2 px-3 py-1.5 rounded-full bg-[var(--accent-light)] border border-[var(--accent)]/20 mb-4">
          <Globe className="w-3.5 h-3.5 text-[var(--accent)]" />
          <span className="text-[12px] font-medium text-[var(--accent)]">
            Join the Network
          </span>
        </div>
        <h1 className="text-[28px] md:text-[36px] font-bold tracking-tight">
          Run an ARC Chain Node
        </h1>
        <p className="text-[14px] text-[var(--text-secondary)] mt-2 max-w-lg mx-auto">
          Help power the fastest L1 blockchain. Run a full node or become a
          validator — takes under 5 minutes on any machine.
        </p>
      </div>

      {/* Why run a node */}
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
        <div className="animate-slide-up card-flat p-4 text-center">
          <div className="w-10 h-10 rounded-lg bg-[var(--shield-green-bg)] flex items-center justify-center mx-auto mb-3">
            <Shield className="w-5 h-5 text-[var(--shield-green)]" />
          </div>
          <div className="text-[13px] font-medium">Secure the Network</div>
          <div className="text-[11px] text-[var(--text-secondary)] mt-1">
            Validate transactions with cryptographic proofs
          </div>
        </div>
        <div
          className="animate-slide-up card-flat p-4 text-center"
          style={{ animationDelay: "50ms" }}
        >
          <div className="w-10 h-10 rounded-lg bg-[var(--accent-light)] flex items-center justify-center mx-auto mb-3">
            <Zap className="w-5 h-5 text-[var(--accent)]" />
          </div>
          <div className="text-[13px] font-medium">Earn Rewards</div>
          <div className="text-[11px] text-[var(--text-secondary)] mt-1">
            Stake ARC tokens and earn validator rewards
          </div>
        </div>
        <div
          className="animate-slide-up card-flat p-4 text-center"
          style={{ animationDelay: "100ms" }}
        >
          <div className="w-10 h-10 rounded-lg bg-[var(--shield-green-bg)] flex items-center justify-center mx-auto mb-3">
            <Globe className="w-5 h-5 text-[var(--shield-green)]" />
          </div>
          <div className="text-[13px] font-medium">Decentralize</div>
          <div className="text-[11px] text-[var(--text-secondary)] mt-1">
            More nodes = more throughput with DAG consensus
          </div>
        </div>
      </div>

      {/* System requirements */}
      <div className="card-flat p-5">
        <h2 className="text-[15px] font-semibold mb-4">System Requirements</h2>
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          {REQUIREMENTS.map(({ icon: Icon, label, value, rec }) => (
            <div
              key={label}
              className="flex items-start gap-3 p-3 rounded-lg bg-[var(--bg)] border border-[var(--border-light)]"
            >
              <Icon className="w-4 h-4 text-[var(--accent)] mt-0.5 shrink-0" />
              <div>
                <div className="text-[13px] font-medium">{label}</div>
                <div className="text-[12px] text-[var(--text-secondary)]">
                  {value}
                </div>
                <div className="text-[11px] text-[var(--text-tertiary)]">
                  {rec}
                </div>
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* Quick install buttons */}
      <div className="card-flat p-5">
        <h2 className="text-[15px] font-semibold mb-4">Quick Install</h2>
        <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
          <a
            href="#"
            className="flex items-center gap-3 p-3 rounded-lg border border-[var(--border)] hover:border-[var(--accent)] hover:bg-[var(--accent-light)] transition-colors group"
          >
            <Apple className="w-5 h-5 text-[var(--text-secondary)] group-hover:text-[var(--accent)]" />
            <div>
              <div className="text-[13px] font-medium group-hover:text-[var(--accent)]">
                macOS
              </div>
              <div className="text-[11px] text-[var(--text-tertiary)]">
                Apple Silicon / Intel
              </div>
            </div>
            <Download className="w-3.5 h-3.5 text-[var(--text-tertiary)] ml-auto" />
          </a>
          <a
            href="#"
            className="flex items-center gap-3 p-3 rounded-lg border border-[var(--border)] hover:border-[var(--accent)] hover:bg-[var(--accent-light)] transition-colors group"
          >
            <Monitor className="w-5 h-5 text-[var(--text-secondary)] group-hover:text-[var(--accent)]" />
            <div>
              <div className="text-[13px] font-medium group-hover:text-[var(--accent)]">
                Linux
              </div>
              <div className="text-[11px] text-[var(--text-tertiary)]">
                x86_64 / ARM64
              </div>
            </div>
            <Download className="w-3.5 h-3.5 text-[var(--text-tertiary)] ml-auto" />
          </a>
          <a
            href="#"
            className="flex items-center gap-3 p-3 rounded-lg border border-[var(--border)] hover:border-[var(--accent)] hover:bg-[var(--accent-light)] transition-colors group"
          >
            <Monitor className="w-5 h-5 text-[var(--text-secondary)] group-hover:text-[var(--accent)]" />
            <div>
              <div className="text-[13px] font-medium group-hover:text-[var(--accent)]">
                Windows
              </div>
              <div className="text-[11px] text-[var(--text-tertiary)]">
                x86_64 (WSL2)
              </div>
            </div>
            <Download className="w-3.5 h-3.5 text-[var(--text-tertiary)] ml-auto" />
          </a>
        </div>
      </div>

      {/* Step-by-step setup */}
      <div>
        <h2 className="text-[15px] font-semibold mb-4">
          Setup Guide — 4 Steps
        </h2>
        <div className="flex flex-col gap-3">
          {STEPS.map((step, i) => (
            <StepCard key={step.title} step={step} index={i} />
          ))}
        </div>
      </div>

      {/* Active nodes */}
      <div className="card-flat p-5">
        <div className="flex items-center justify-between mb-4">
          <h2 className="text-[15px] font-semibold">Active Nodes</h2>
          <div className="flex items-center gap-1.5">
            <div className="w-2 h-2 rounded-full bg-[var(--shield-green)] animate-pulse" />
            <span className="text-[12px] text-[var(--shield-green)] font-medium">
              1 node online
            </span>
          </div>
        </div>

        <div className="border border-[var(--border-light)] rounded-lg overflow-hidden">
          <div className="flex items-center gap-4 px-4 py-3 bg-[var(--bg)]">
            <div className="w-8 h-8 rounded-lg bg-[var(--shield-green-bg)] flex items-center justify-center">
              <Globe className="w-4 h-4 text-[var(--shield-green)]" />
            </div>
            <div className="flex-1 min-w-0">
              <div className="text-[13px] font-medium">
                arc-node-genesis-01
              </div>
              <div className="text-[11px] text-[var(--text-secondary)]">
                Local (MacBook M4) &middot; v0.1.0 &middot; 148,293 blocks
                produced
              </div>
            </div>
            <div className="flex items-center gap-1.5 px-2 py-1 rounded-full bg-[var(--shield-green-bg)]">
              <div className="w-1.5 h-1.5 rounded-full bg-[var(--shield-green)]" />
              <span className="text-[11px] font-medium text-[var(--shield-green)]">
                Active
              </span>
            </div>
          </div>
        </div>

        <div className="mt-4 p-4 rounded-lg border border-dashed border-[var(--border)] text-center">
          <p className="text-[13px] text-[var(--text-secondary)]">
            Your node will appear here after joining the testnet
          </p>
          <a
            href="#"
            className="inline-flex items-center gap-1.5 mt-2 text-[12px] text-[var(--accent)] font-medium hover:underline"
          >
            Get started above
            <ArrowRight className="w-3 h-3" />
          </a>
        </div>
      </div>
    </div>
  );
}
