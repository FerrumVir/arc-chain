"use client";

import { useState, useCallback } from "react";
import {
  Shield,
  ShieldCheck,
  Lock,
  Layers,
  Hash,
  GitBranch,
  Copy,
  CheckCircle,
  XCircle,
} from "lucide-react";
import { truncateHash } from "@/lib/format";
import {
  verifyBlake3Hash,
  verifyMerkleProof,
  type VerificationResult,
  type MerkleVerificationResult,
} from "@/lib/verify";
import VerifyButton from "@/components/VerifyButton";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface VerificationPanelProps {
  txHash: string;
  blake3: {
    preHashHex: string;
    hash: string;
    domain: string;
  };
  merkle: {
    leafHash: string;
    index: number;
    siblings: { hash: string; isLeft: boolean }[];
    root: string;
  };
  pedersen?: {
    commitment: string;
    description: string;
  };
  zkProof?: {
    proofId: string;
    txCount: number;
  };
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

/** Section header with number badge and icon */
function SectionHeader({
  step,
  icon: Icon,
  title,
  subtitle,
}: {
  step: number;
  icon: React.ElementType;
  title: string;
  subtitle: string;
}) {
  return (
    <div className="flex items-center gap-3">
      <div className="w-7 h-7 rounded-lg bg-[var(--accent-light)] flex items-center justify-center shrink-0">
        <Icon className="w-3.5 h-3.5 text-[var(--accent)]" strokeWidth={2.5} />
      </div>
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-bold uppercase tracking-widest text-[var(--accent)] bg-[var(--accent-light)] px-1.5 py-0.5 rounded">
            {step}
          </span>
          <span className="text-[13px] font-semibold">{title}</span>
        </div>
        <p className="text-[11px] text-[var(--text-tertiary)] mt-0.5">
          {subtitle}
        </p>
      </div>
    </div>
  );
}

/** Copyable hash row */
function HashRow({
  label,
  hash,
  accent = false,
}: {
  label: string;
  hash: string;
  accent?: boolean;
}) {
  return (
    <div>
      <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-0.5">
        {label}
      </span>
      <div className="flex items-center gap-1.5">
        <span
          className={`text-[11px] font-hash break-all ${
            accent
              ? "text-[var(--accent)]"
              : "text-[var(--text-secondary)]"
          }`}
        >
          {hash}
        </span>
        <button
          className="shrink-0 p-0.5 rounded hover:bg-[var(--border-light)] transition-colors"
          onClick={() => navigator.clipboard?.writeText(hash)}
          aria-label={`Copy ${label}`}
        >
          <Copy className="w-2.5 h-2.5 text-[var(--text-tertiary)]" />
        </button>
      </div>
    </div>
  );
}

/** Match indicator: green check or red X with computed vs claimed */
function MatchIndicator({
  computedHash,
  claimedHash,
  valid,
}: {
  computedHash: string;
  claimedHash: string;
  valid: boolean;
}) {
  // Normalize for display: add 0x prefix if missing
  const display = (h: string) =>
    h.startsWith("0x") ? h : `0x${h}`;

  return (
    <div className="mt-3 p-3 rounded-lg border bg-[var(--bg)] border-[var(--border-light)] animate-slide-up">
      <div className="flex items-center gap-2 mb-2">
        {valid ? (
          <>
            <CheckCircle className="w-3.5 h-3.5 text-[var(--shield-green)] animate-proof-check" />
            <span className="text-[11px] font-semibold text-[var(--shield-green)]">
              Hash Match
            </span>
          </>
        ) : (
          <>
            <XCircle className="w-3.5 h-3.5 text-[var(--shield-red)]" />
            <span className="text-[11px] font-semibold text-[var(--shield-red)]">
              Hash Mismatch
            </span>
          </>
        )}
      </div>
      <div className="flex flex-col gap-1.5">
        <div>
          <span className="text-[9px] text-[var(--text-tertiary)] uppercase tracking-label">
            Computed
          </span>
          <div className="text-[10px] font-hash text-[var(--shield-green)] break-all">
            {display(computedHash)}
          </div>
        </div>
        <div>
          <span className="text-[9px] text-[var(--text-tertiary)] uppercase tracking-label">
            Claimed
          </span>
          <div className="text-[10px] font-hash text-[var(--text-secondary)] break-all">
            {display(claimedHash)}
          </div>
        </div>
      </div>
    </div>
  );
}

/** Mini Merkle tree path visualization */
function MerklePathViz({
  result,
  siblings,
  leafHash,
  root,
}: {
  result: MerkleVerificationResult;
  siblings: { hash: string; isLeft: boolean }[];
  leafHash: string;
  root: string;
}) {
  return (
    <div className="mt-3 p-3 rounded-lg border bg-[var(--bg)] border-[var(--border-light)] animate-slide-up">
      <div className="flex items-center gap-2 mb-3">
        {result.valid ? (
          <CheckCircle className="w-3.5 h-3.5 text-[var(--shield-green)] animate-proof-check" />
        ) : (
          <XCircle className="w-3.5 h-3.5 text-[var(--shield-red)]" />
        )}
        <span
          className={`text-[11px] font-semibold ${
            result.valid
              ? "text-[var(--shield-green)]"
              : "text-[var(--shield-red)]"
          }`}
        >
          {result.valid ? "Root Verified" : "Root Mismatch"}
        </span>
      </div>

      {/* Visual path: leaf -> siblings -> root */}
      <div className="flex flex-col gap-0">
        {/* Leaf */}
        <div className="flex items-center gap-2">
          <div className="w-5 flex justify-center">
            <div className="w-2 h-2 rounded-full bg-[var(--accent)]" />
          </div>
          <span className="text-[9px] text-[var(--text-tertiary)] uppercase tracking-label w-10 shrink-0">
            Leaf
          </span>
          <span className="text-[10px] font-hash text-[var(--accent)] truncate">
            {truncateHash(leafHash, 10)}
          </span>
        </div>

        {/* Sibling levels */}
        {siblings.map((sib, i) => (
          <div key={i} className="flex items-center gap-2">
            {/* Connector line */}
            <div className="w-5 flex justify-center">
              <div className="w-px h-4 bg-[var(--border)]" />
            </div>
            <span className="text-[9px] text-[var(--text-tertiary)] uppercase tracking-label w-10 shrink-0">
              L{i + 1} {sib.isLeft ? "\u2190" : "\u2192"}
            </span>
            <span className="text-[10px] font-hash text-[var(--text-secondary)] truncate">
              {truncateHash(sib.hash, 10)}
            </span>
          </div>
        ))}

        {/* Root connector */}
        <div className="flex items-center gap-2">
          <div className="w-5 flex justify-center">
            <div className="w-px h-4 bg-[var(--border)]" />
          </div>
          <span className="text-[9px] text-[var(--text-tertiary)] w-10 shrink-0" />
          <span className="text-[10px] font-hash text-[var(--text-secondary)] truncate" />
        </div>

        {/* Root */}
        <div className="flex items-center gap-2">
          <div className="w-5 flex justify-center">
            <div
              className={`w-2.5 h-2.5 rounded-full ${
                result.valid
                  ? "bg-[var(--shield-green)]"
                  : "bg-[var(--shield-red)]"
              }`}
            />
          </div>
          <span className="text-[9px] text-[var(--text-tertiary)] uppercase tracking-label w-10 shrink-0">
            Root
          </span>
          <span
            className={`text-[10px] font-hash truncate ${
              result.valid
                ? "text-[var(--shield-green)]"
                : "text-[var(--shield-red)]"
            }`}
          >
            {truncateHash(root, 10)}
          </span>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main Component
// ---------------------------------------------------------------------------

export default function VerificationPanel({
  txHash,
  blake3,
  merkle,
  pedersen,
  zkProof,
}: VerificationPanelProps) {
  const [blake3Result, setBlake3Result] = useState<VerificationResult | null>(
    null
  );
  const [merkleResult, setMerkleResult] =
    useState<MerkleVerificationResult | null>(null);

  // --- BLAKE3 verification handler ---
  const handleBlake3Verify = useCallback(async () => {
    const result = await verifyBlake3Hash({
      preHashHex: blake3.preHashHex,
      claimedHash: blake3.hash,
      domain: blake3.domain,
    });
    setBlake3Result(result);
    return { valid: result.valid, timeMs: result.timeMs };
  }, [blake3]);

  // --- Merkle verification handler ---
  const handleMerkleVerify = useCallback(async () => {
    const result = await verifyMerkleProof({
      leafHash: merkle.leafHash,
      index: merkle.index,
      siblings: merkle.siblings,
      expectedRoot: merkle.root,
    });
    setMerkleResult(result);
    return { valid: result.valid, timeMs: result.timeMs };
  }, [merkle]);

  return (
    <div className="card-flat overflow-hidden">
      {/* ===== Panel Header ===== */}
      <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border)]">
        <div className="flex items-center gap-2.5">
          <div className="w-7 h-7 rounded-lg bg-[var(--accent)] flex items-center justify-center">
            <Shield
              className="w-3.5 h-3.5 text-white"
              strokeWidth={2.5}
            />
          </div>
          <div>
            <h2 className="text-[14px] font-semibold tracking-tight">
              ARC Proof Engine
            </h2>
            <p className="text-[10px] text-[var(--text-tertiary)]">
              4-layer cryptographic verification
            </p>
          </div>
        </div>
        <div className="text-[10px] font-hash text-[var(--text-tertiary)]">
          {truncateHash(txHash, 8)}
        </div>
      </div>

      {/* ===== Proof Cards ===== */}
      <div className="flex flex-col gap-0 divide-y divide-[var(--border-light)]">
        {/* ---- 1. BLAKE3 Commitment ---- */}
        <div className="p-5 animate-slide-up proof-delay-1">
          <SectionHeader
            step={1}
            icon={Hash}
            title="BLAKE3 Commitment"
            subtitle="Domain-separated cryptographic hash of transaction payload"
          />

          <div className="mt-4 ml-10 flex flex-col gap-3">
            <HashRow label="Transaction Hash" hash={blake3.hash} accent />
            <HashRow label="Pre-Image (hex)" hash={blake3.preHashHex} />

            <div className="flex items-center gap-3">
              <div>
                <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-0.5">
                  Domain
                </span>
                <span className="text-[11px] font-hash text-[var(--accent)] bg-[var(--accent-light)] px-2 py-0.5 rounded">
                  {blake3.domain}
                </span>
              </div>
            </div>

            <div className="pt-1">
              <VerifyButton
                onVerify={handleBlake3Verify}
                size="sm"
                label="Verify Hash"
              />
            </div>

            {blake3Result && (
              <MatchIndicator
                computedHash={blake3Result.computedHash}
                claimedHash={blake3Result.claimedHash}
                valid={blake3Result.valid}
              />
            )}
          </div>
        </div>

        {/* ---- 2. Merkle Inclusion ---- */}
        <div className="p-5 animate-slide-up proof-delay-2">
          <SectionHeader
            step={2}
            icon={GitBranch}
            title="Merkle Inclusion"
            subtitle="Proof that this transaction is included in the block tree"
          />

          <div className="mt-4 ml-10 flex flex-col gap-3">
            <HashRow label="Merkle Root" hash={merkle.root} accent />
            <div className="flex items-center gap-6">
              <div>
                <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-0.5">
                  Leaf Index
                </span>
                <span className="text-[12px] font-semibold font-hash">
                  {merkle.index}
                </span>
              </div>
              <div>
                <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-0.5">
                  Path Depth
                </span>
                <span className="text-[12px] font-semibold font-hash">
                  {merkle.siblings.length} levels
                </span>
              </div>
            </div>

            <div className="pt-1">
              <VerifyButton
                onVerify={handleMerkleVerify}
                size="sm"
                label="Verify Proof"
              />
            </div>

            {merkleResult && (
              <MerklePathViz
                result={merkleResult}
                siblings={merkle.siblings}
                leafHash={merkle.leafHash}
                root={merkle.root}
              />
            )}
          </div>
        </div>

        {/* ---- 3. Pedersen Privacy ---- */}
        {pedersen && (
          <div className="p-5 animate-slide-up proof-delay-3">
            <SectionHeader
              step={3}
              icon={Lock}
              title="Pedersen Privacy"
              subtitle="Amount hidden via Pedersen commitment on Ristretto curve"
            />

            <div className="mt-4 ml-10 flex flex-col gap-3">
              <HashRow
                label="Commitment Point"
                hash={pedersen.commitment}
              />
              <p className="text-[11px] text-[var(--text-secondary)] leading-relaxed">
                {pedersen.description}
              </p>
              <div>
                <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full text-[10px] font-semibold uppercase tracking-label bg-[var(--accent-light)] border border-[var(--accent)]/20 text-[var(--accent)]">
                  <Lock className="w-3 h-3" strokeWidth={2.5} />
                  Privacy Shielded
                </span>
              </div>
            </div>
          </div>
        )}

        {/* ---- 4. ZK Compression ---- */}
        {zkProof && (
          <div className="p-5 animate-slide-up proof-delay-4">
            <SectionHeader
              step={pedersen ? 4 : 3}
              icon={Layers}
              title="ZK Compression"
              subtitle="Batch validity proof aggregating multiple transactions"
            />

            <div className="mt-4 ml-10 flex flex-col gap-3">
              <HashRow label="Proof ID" hash={zkProof.proofId} />
              <div>
                <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-0.5">
                  Batch Size
                </span>
                <span className="text-[12px] font-semibold">
                  {zkProof.txCount.toLocaleString()} transactions
                </span>
              </div>
              <div>
                <span className="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full text-[10px] font-semibold uppercase tracking-label bg-[var(--shield-green-bg)] border border-[var(--shield-green)]/20 text-[var(--shield-green)]">
                  <ShieldCheck className="w-3 h-3" strokeWidth={2.5} />
                  Batch Compressed
                </span>
              </div>
            </div>
          </div>
        )}
      </div>

      {/* ===== Footer ===== */}
      <div className="px-5 py-3 border-t border-[var(--border)] bg-[var(--bg)]">
        <div className="flex items-center justify-between">
          <span className="text-[10px] text-[var(--text-tertiary)]">
            All proofs verified client-side using BLAKE3 WASM
          </span>
          <div className="flex items-center gap-1.5">
            <ShieldCheck
              className="w-3 h-3 text-[var(--shield-green)]"
              strokeWidth={2.5}
            />
            <span className="text-[10px] font-medium text-[var(--shield-green)]">
              {[blake3Result?.valid, merkleResult?.valid].filter(Boolean).length}
              /
              {2 + (pedersen ? 1 : 0) + (zkProof ? 1 : 0)} layers verified
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}
