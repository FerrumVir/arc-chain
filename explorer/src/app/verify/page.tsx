"use client";

import { useState, useCallback } from "react";
import {
  Shield,
  ShieldCheck,
  Hash,
  GitBranch,
  FileCode,
  Copy,
  CheckCircle,
  XCircle,
  ChevronRight,
  Clock,
} from "lucide-react";
import {
  computeBlake3Hash,
  verifyBlake3Hash,
  verifyMerkleProof,
  hexToBytes,
  bytesToHex,
  concatBytes,
} from "@/lib/verify";
import { blake3 } from "@noble/hashes/blake3.js";
import VerifyButton from "@/components/VerifyButton";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type Tab = "transaction" | "merkle" | "raw";

interface TxVerifyResult {
  valid: boolean;
  computedHash: string;
  claimedHash: string;
  timeMs: number;
  preHashByteLen: number;
}

interface MerkleVerifyResult {
  valid: boolean;
  computedRoot: string;
  expectedRoot: string;
  pathLength: number;
  timeMs: number;
}

interface RawHashResult {
  hash: string;
  timeMs: number;
  inputByteLen: number;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
      className="shrink-0 p-1 rounded-md hover:bg-[var(--border-light)] transition-colors"
      aria-label="Copy to clipboard"
    >
      {copied ? (
        <CheckCircle className="w-3 h-3 text-[var(--shield-green)]" />
      ) : (
        <Copy className="w-3 h-3 text-[var(--text-tertiary)]" />
      )}
    </button>
  );
}

function formatTime(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)}\u00B5s`;
  return `${ms.toFixed(2)}ms`;
}

const TABS: { key: Tab; label: string; icon: React.ElementType }[] = [
  { key: "transaction", label: "Verify Transaction", icon: Hash },
  { key: "merkle", label: "Verify Merkle Proof", icon: GitBranch },
  { key: "raw", label: "Raw BLAKE3 Hash", icon: FileCode },
];

const DOMAINS = [
  { value: "ARC-chain-tx-v1", label: "ARC-chain-tx-v1 (Transaction)" },
  { value: "ARC-chain-block-v1", label: "ARC-chain-block-v1 (Block)" },
  { value: "__custom__", label: "Custom domain..." },
];

// ---------------------------------------------------------------------------
// Tab 1: Verify Transaction
// ---------------------------------------------------------------------------

function VerifyTransactionTab() {
  const [claimedHash, setClaimedHash] = useState("");
  const [preHashHex, setPreHashHex] = useState("");
  const [domainSelect, setDomainSelect] = useState("ARC-chain-tx-v1");
  const [customDomain, setCustomDomain] = useState("");
  const [result, setResult] = useState<TxVerifyResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  const domain =
    domainSelect === "__custom__" ? customDomain : domainSelect;

  const handleVerify = useCallback(async () => {
    setError(null);
    try {
      const preHashBytes = hexToBytes(preHashHex);
      const r = verifyBlake3Hash({
        preHashHex,
        claimedHash,
        domain,
      });
      const txResult: TxVerifyResult = {
        valid: r.valid,
        computedHash: r.computedHash,
        claimedHash: r.claimedHash,
        timeMs: r.timeMs,
        preHashByteLen: preHashBytes.length,
      };
      setResult(txResult);
      return { valid: r.valid, timeMs: r.timeMs };
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : "Unknown error";
      setError(msg);
      setResult(null);
      throw e;
    }
  }, [preHashHex, claimedHash, domain]);

  return (
    <div className="flex flex-col gap-5">
      {/* Input Card */}
      <div className="card-flat p-5">
        <h3 className="text-[14px] font-semibold mb-4 flex items-center gap-2">
          <Hash className="w-4 h-4 text-[var(--accent)]" />
          Input Data
        </h3>

        <div className="flex flex-col gap-4">
          {/* Transaction Hash */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Transaction Hash (claimed)
            </label>
            <input
              type="text"
              value={claimedHash}
              onChange={(e) => setClaimedHash(e.target.value)}
              placeholder="e.g. 0x7a3b1c..."
              className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
            />
          </div>

          {/* Pre-Hash Hex */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Pre-Hash Hex (raw bytes that produce the hash)
            </label>
            <input
              type="text"
              value={preHashHex}
              onChange={(e) => setPreHashHex(e.target.value)}
              placeholder="e.g. 01a3f2b5..."
              className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
            />
          </div>

          {/* Domain */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Domain Separation Context
            </label>
            <select
              value={domainSelect}
              onChange={(e) => setDomainSelect(e.target.value)}
              className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] focus:outline-none focus:border-[var(--accent)] transition-colors appearance-none cursor-pointer"
            >
              {DOMAINS.map((d) => (
                <option key={d.value} value={d.value}>
                  {d.label}
                </option>
              ))}
            </select>
          </div>

          {/* Custom domain input */}
          {domainSelect === "__custom__" && (
            <div>
              <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
                Custom Domain String
              </label>
              <input
                type="text"
                value={customDomain}
                onChange={(e) => setCustomDomain(e.target.value)}
                placeholder="e.g. my-custom-domain-v1"
                className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
              />
            </div>
          )}

          {/* Verify button */}
          <div className="pt-1">
            <VerifyButton
              onVerify={handleVerify}
              label="ARC Verify"
            />
          </div>
        </div>
      </div>

      {/* Error */}
      {error && (
        <div className="bg-[var(--shield-red-bg)] border border-[var(--shield-red)]/30 rounded-xl p-4 animate-slide-up">
          <div className="flex items-center gap-2 mb-1">
            <XCircle className="w-4 h-4 text-[var(--shield-red)]" />
            <span className="text-[13px] font-semibold text-[var(--shield-red)]">
              Verification Error
            </span>
          </div>
          <p className="text-[12px] text-[var(--shield-red)] font-hash ml-6">
            {error}
          </p>
        </div>
      )}

      {/* Result Card */}
      {result && (
        <div className="card-flat overflow-hidden animate-slide-up">
          {/* Result header */}
          <div
            className={`px-5 py-4 border-b flex items-center gap-3 ${
              result.valid
                ? "border-[var(--shield-green)]/20 bg-[var(--shield-green-bg)]"
                : "border-[var(--shield-red)]/20 bg-[var(--shield-red-bg)]"
            }`}
          >
            {result.valid ? (
              <ShieldCheck className="w-5 h-5 text-[var(--shield-green)] animate-proof-check" />
            ) : (
              <XCircle className="w-5 h-5 text-[var(--shield-red)]" />
            )}
            <div>
              <div
                className={`text-[14px] font-semibold ${
                  result.valid
                    ? "text-[var(--shield-green)]"
                    : "text-[var(--shield-red)]"
                }`}
              >
                {result.valid
                  ? "\u2713 Independently Verified"
                  : "\u2717 Hash Mismatch"}
              </div>
              <div className="text-[11px] text-[var(--text-tertiary)] flex items-center gap-1.5 mt-0.5">
                <Clock className="w-3 h-3" />
                Verified in {formatTime(result.timeMs)}
              </div>
            </div>
          </div>

          {/* Hash comparison */}
          <div className="p-5 flex flex-col gap-4">
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1">
                Computed Hash
              </span>
              <div className="flex items-center gap-2">
                <span className="text-[11px] font-hash text-[var(--accent)] break-all">
                  {result.computedHash}
                </span>
                <CopyButton text={result.computedHash} />
              </div>
            </div>

            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1">
                Claimed Hash
              </span>
              <div className="flex items-center gap-2">
                <span className="text-[11px] font-hash text-[var(--text-secondary)] break-all">
                  {result.claimedHash}
                </span>
                <CopyButton text={result.claimedHash} />
              </div>
            </div>

            {/* Step-by-step computation */}
            <div className="mt-2 border-t border-[var(--border-light)] pt-4">
              <span className="text-[11px] font-semibold text-[var(--text-secondary)] uppercase tracking-label block mb-3">
                Computation Steps
              </span>
              <div className="flex flex-col gap-2.5">
                <div className="flex items-start gap-3">
                  <div className="w-5 h-5 rounded-full bg-[var(--accent-light)] flex items-center justify-center shrink-0 mt-0.5">
                    <span className="text-[10px] font-bold text-[var(--accent)]">
                      1
                    </span>
                  </div>
                  <div className="text-[12px] text-[var(--text-secondary)]">
                    Decode hex{" "}
                    <ChevronRight className="w-3 h-3 inline text-[var(--text-tertiary)]" />{" "}
                    <span className="font-hash text-[var(--accent)]">
                      {result.preHashByteLen} bytes
                    </span>
                  </div>
                </div>
                <div className="flex items-start gap-3">
                  <div className="w-5 h-5 rounded-full bg-[var(--accent-light)] flex items-center justify-center shrink-0 mt-0.5">
                    <span className="text-[10px] font-bold text-[var(--accent)]">
                      2
                    </span>
                  </div>
                  <div className="text-[12px] text-[var(--text-secondary)]">
                    BLAKE3{" "}
                    <span className="font-hash text-[11px] text-[var(--accent)] bg-[var(--accent-light)] px-1.5 py-0.5 rounded">
                      derive_key(&quot;{domain}&quot;, data)
                    </span>
                  </div>
                </div>
                <div className="flex items-start gap-3">
                  <div className="w-5 h-5 rounded-full bg-[var(--accent-light)] flex items-center justify-center shrink-0 mt-0.5">
                    <span className="text-[10px] font-bold text-[var(--accent)]">
                      3
                    </span>
                  </div>
                  <div className="text-[12px] text-[var(--text-secondary)]">
                    Compare output{" "}
                    <ChevronRight className="w-3 h-3 inline text-[var(--text-tertiary)]" />{" "}
                    <span
                      className={`font-semibold ${
                        result.valid
                          ? "text-[var(--shield-green)]"
                          : "text-[var(--shield-red)]"
                      }`}
                    >
                      {result.valid ? "Match" : "Mismatch"}
                    </span>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Tab 2: Verify Merkle Proof
// ---------------------------------------------------------------------------

function VerifyMerkleTab() {
  const [leafHash, setLeafHash] = useState("");
  const [leafIndex, setLeafIndex] = useState("0");
  const [siblingsText, setSiblingsText] = useState("");
  const [expectedRoot, setExpectedRoot] = useState("");
  const [result, setResult] = useState<MerkleVerifyResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  /** Parse sibling text lines like "L:abc123..." or "R:def456..." */
  function parseSiblings(
    text: string
  ): { hash: string; isLeft: boolean }[] {
    return text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0)
      .map((line) => {
        const isLeft = line.toUpperCase().startsWith("L:");
        const isRight = line.toUpperCase().startsWith("R:");
        if (!isLeft && !isRight) {
          throw new Error(
            `Invalid sibling format: "${line}". Expected "L:hash" or "R:hash".`
          );
        }
        return {
          hash: line.slice(2).trim(),
          isLeft,
        };
      });
  }

  const handleVerify = useCallback(async () => {
    setError(null);
    try {
      const siblings = parseSiblings(siblingsText);
      const index = parseInt(leafIndex, 10);
      if (isNaN(index) || index < 0) {
        throw new Error("Leaf index must be a non-negative integer");
      }

      const r = verifyMerkleProof({
        leafHash,
        index,
        siblings,
        expectedRoot,
      });

      const merkleResult: MerkleVerifyResult = {
        valid: r.valid,
        computedRoot: r.computedRoot,
        expectedRoot: r.expectedRoot,
        pathLength: r.pathLength,
        timeMs: r.timeMs,
      };
      setResult(merkleResult);
      return { valid: r.valid, timeMs: r.timeMs };
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : "Unknown error";
      setError(msg);
      setResult(null);
      throw e;
    }
  }, [leafHash, leafIndex, siblingsText, expectedRoot]);

  return (
    <div className="flex flex-col gap-5">
      {/* Input Card */}
      <div className="card-flat p-5">
        <h3 className="text-[14px] font-semibold mb-4 flex items-center gap-2">
          <GitBranch className="w-4 h-4 text-[var(--accent)]" />
          Merkle Proof Data
        </h3>

        <div className="flex flex-col gap-4">
          {/* Leaf Hash */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Leaf Hash (the tx hash)
            </label>
            <input
              type="text"
              value={leafHash}
              onChange={(e) => setLeafHash(e.target.value)}
              placeholder="e.g. 0x7a3b1c..."
              className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
            />
          </div>

          {/* Leaf Index */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Leaf Index
            </label>
            <input
              type="number"
              min="0"
              value={leafIndex}
              onChange={(e) => setLeafIndex(e.target.value)}
              placeholder="0"
              className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
            />
          </div>

          {/* Sibling Hashes */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Sibling Hashes (one per line: L:hash or R:hash)
            </label>
            <textarea
              value={siblingsText}
              onChange={(e) => setSiblingsText(e.target.value)}
              placeholder={`L:a1b2c3d4...\nR:e5f6a7b8...\nL:c9d0e1f2...`}
              rows={5}
              className="w-full px-3 py-2.5 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors resize-y"
            />
            <p className="text-[10px] text-[var(--text-tertiary)] mt-1">
              L = sibling is on the left, R = sibling is on the right.
              Prefix indicates the sibling&apos;s position in the pair.
            </p>
          </div>

          {/* Expected Root */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Expected Merkle Root
            </label>
            <input
              type="text"
              value={expectedRoot}
              onChange={(e) => setExpectedRoot(e.target.value)}
              placeholder="e.g. 0xf9c8b7..."
              className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
            />
          </div>

          {/* Verify button */}
          <div className="pt-1">
            <VerifyButton
              onVerify={handleVerify}
              label="ARC Verify"
            />
          </div>
        </div>
      </div>

      {/* Error */}
      {error && (
        <div className="bg-[var(--shield-red-bg)] border border-[var(--shield-red)]/30 rounded-xl p-4 animate-slide-up">
          <div className="flex items-center gap-2 mb-1">
            <XCircle className="w-4 h-4 text-[var(--shield-red)]" />
            <span className="text-[13px] font-semibold text-[var(--shield-red)]">
              Verification Error
            </span>
          </div>
          <p className="text-[12px] text-[var(--shield-red)] font-hash ml-6">
            {error}
          </p>
        </div>
      )}

      {/* Result Card */}
      {result && (
        <div className="card-flat overflow-hidden animate-slide-up">
          {/* Result header */}
          <div
            className={`px-5 py-4 border-b flex items-center gap-3 ${
              result.valid
                ? "border-[var(--shield-green)]/20 bg-[var(--shield-green-bg)]"
                : "border-[var(--shield-red)]/20 bg-[var(--shield-red-bg)]"
            }`}
          >
            {result.valid ? (
              <ShieldCheck className="w-5 h-5 text-[var(--shield-green)] animate-proof-check" />
            ) : (
              <XCircle className="w-5 h-5 text-[var(--shield-red)]" />
            )}
            <div>
              <div
                className={`text-[14px] font-semibold ${
                  result.valid
                    ? "text-[var(--shield-green)]"
                    : "text-[var(--shield-red)]"
                }`}
              >
                {result.valid
                  ? "\u2713 Merkle Root Verified"
                  : "\u2717 Root Mismatch"}
              </div>
              <div className="text-[11px] text-[var(--text-tertiary)] flex items-center gap-1.5 mt-0.5">
                <Clock className="w-3 h-3" />
                Verified in {formatTime(result.timeMs)} &middot;{" "}
                {result.pathLength} levels
              </div>
            </div>
          </div>

          {/* Root comparison */}
          <div className="p-5 flex flex-col gap-4">
            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1">
                Computed Root
              </span>
              <div className="flex items-center gap-2">
                <span className="text-[11px] font-hash text-[var(--accent)] break-all">
                  {result.computedRoot}
                </span>
                <CopyButton text={result.computedRoot} />
              </div>
            </div>

            <div>
              <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1">
                Expected Root
              </span>
              <div className="flex items-center gap-2">
                <span className="text-[11px] font-hash text-[var(--text-secondary)] break-all">
                  {result.expectedRoot}
                </span>
                <CopyButton text={result.expectedRoot} />
              </div>
            </div>

            {/* Path visualization */}
            <div className="mt-2 border-t border-[var(--border-light)] pt-4">
              <span className="text-[11px] font-semibold text-[var(--text-secondary)] uppercase tracking-label block mb-3">
                Merkle Path
              </span>
              <div className="flex flex-col gap-0">
                {/* Leaf */}
                <div className="flex items-center gap-3">
                  <div className="w-5 flex justify-center">
                    <div className="w-2.5 h-2.5 rounded-full bg-[var(--accent)]" />
                  </div>
                  <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label w-12 shrink-0">
                    Leaf
                  </span>
                  <span className="text-[11px] font-hash text-[var(--accent)] truncate">
                    {leafHash.length > 20
                      ? `${leafHash.slice(0, 10)}...${leafHash.slice(-10)}`
                      : leafHash}
                  </span>
                </div>

                {/* Sibling levels */}
                {siblingsText
                  .split("\n")
                  .map((l) => l.trim())
                  .filter((l) => l.length > 0)
                  .map((line, i) => {
                    const isLeft = line.toUpperCase().startsWith("L:");
                    const hash = line.slice(2).trim();
                    return (
                      <div key={i} className="flex items-center gap-3">
                        <div className="w-5 flex justify-center">
                          <div className="w-px h-5 bg-[var(--border)]" />
                        </div>
                        <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label w-12 shrink-0">
                          L{i + 1} {isLeft ? "\u2190" : "\u2192"}
                        </span>
                        <span className="text-[11px] font-hash text-[var(--text-secondary)] truncate">
                          {hash.length > 20
                            ? `${hash.slice(0, 10)}...${hash.slice(-10)}`
                            : hash}
                        </span>
                      </div>
                    );
                  })}

                {/* Connector */}
                <div className="flex items-center gap-3">
                  <div className="w-5 flex justify-center">
                    <div className="w-px h-5 bg-[var(--border)]" />
                  </div>
                </div>

                {/* Root */}
                <div className="flex items-center gap-3">
                  <div className="w-5 flex justify-center">
                    <div
                      className={`w-3 h-3 rounded-full ${
                        result.valid
                          ? "bg-[var(--shield-green)]"
                          : "bg-[var(--shield-red)]"
                      }`}
                    />
                  </div>
                  <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label w-12 shrink-0">
                    Root
                  </span>
                  <span
                    className={`text-[11px] font-hash truncate ${
                      result.valid
                        ? "text-[var(--shield-green)]"
                        : "text-[var(--shield-red)]"
                    }`}
                  >
                    {result.computedRoot.length > 20
                      ? `${result.computedRoot.slice(0, 10)}...${result.computedRoot.slice(-10)}`
                      : result.computedRoot}
                  </span>
                </div>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Tab 3: Raw BLAKE3 Hash
// ---------------------------------------------------------------------------

function RawHashTab() {
  const [hexInput, setHexInput] = useState("");
  const [mode, setMode] = useState<"plain" | "derive">("plain");
  const [context, setContext] = useState("");
  const [result, setResult] = useState<RawHashResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  const handleCompute = useCallback(() => {
    setError(null);
    try {
      const data = hexToBytes(hexInput);
      const t0 = performance.now();

      let hash: string;
      if (mode === "derive") {
        if (!context.trim()) {
          throw new Error("Context string is required for derive key mode");
        }
        hash = computeBlake3Hash(hexInput, context.trim());
      } else {
        // Plain hash: blake3() with no context = Hasher::new() mode
        const digest = blake3(data);
        hash = bytesToHex(digest);
      }

      const t1 = performance.now();
      const timeMs = Math.round((t1 - t0) * 1000) / 1000;

      setResult({
        hash,
        timeMs,
        inputByteLen: data.length,
      });
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : "Unknown error";
      setError(msg);
      setResult(null);
    }
  }, [hexInput, mode, context]);

  return (
    <div className="flex flex-col gap-5">
      {/* Input Card */}
      <div className="card-flat p-5">
        <h3 className="text-[14px] font-semibold mb-4 flex items-center gap-2">
          <FileCode className="w-4 h-4 text-[var(--accent)]" />
          Raw Hash Input
        </h3>

        <div className="flex flex-col gap-4">
          {/* Hex input */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
              Hex Data
            </label>
            <textarea
              value={hexInput}
              onChange={(e) => setHexInput(e.target.value)}
              placeholder="Paste hex bytes (with or without 0x prefix)..."
              rows={4}
              className="w-full px-3 py-2.5 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors resize-y"
            />
          </div>

          {/* Mode radio */}
          <div>
            <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-2">
              Hash Mode
            </label>
            <div className="flex items-center gap-4">
              <label className="flex items-center gap-2 cursor-pointer">
                <input
                  type="radio"
                  name="hash-mode"
                  checked={mode === "plain"}
                  onChange={() => setMode("plain")}
                  className="accent-[var(--accent)]"
                />
                <span className="text-[12px] text-[var(--text-secondary)]">
                  Plain hash
                </span>
              </label>
              <label className="flex items-center gap-2 cursor-pointer">
                <input
                  type="radio"
                  name="hash-mode"
                  checked={mode === "derive"}
                  onChange={() => setMode("derive")}
                  className="accent-[var(--accent)]"
                />
                <span className="text-[12px] text-[var(--text-secondary)]">
                  Derive key
                </span>
              </label>
            </div>
          </div>

          {/* Context string (derive mode) */}
          {mode === "derive" && (
            <div>
              <label className="text-[11px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1.5">
                Context String
              </label>
              <input
                type="text"
                value={context}
                onChange={(e) => setContext(e.target.value)}
                placeholder="e.g. ARC-chain-tx-v1"
                className="w-full h-10 px-3 rounded-lg bg-[var(--bg-input)] border border-[var(--border)] text-[12px] font-hash placeholder:text-[var(--text-tertiary)] focus:outline-none focus:border-[var(--accent)] transition-colors"
              />
            </div>
          )}

          {/* Compute button */}
          <div className="pt-1">
            <button
              onClick={handleCompute}
              className="inline-flex items-center gap-1.5 px-3.5 py-2 rounded-lg text-[12px] font-medium border border-[var(--accent)]/40 bg-[var(--accent-light)] text-[var(--accent)] hover:border-[var(--accent)] hover:shadow-[0_0_16px_var(--accent-glow)] active:scale-[0.97] transition-all select-none focus:outline-none focus-visible:ring-2 focus-visible:ring-[var(--accent)]/50"
            >
              <Hash className="w-3.5 h-3.5" strokeWidth={2.5} />
              Compute Hash
            </button>
          </div>
        </div>
      </div>

      {/* Error */}
      {error && (
        <div className="bg-[var(--shield-red-bg)] border border-[var(--shield-red)]/30 rounded-xl p-4 animate-slide-up">
          <div className="flex items-center gap-2 mb-1">
            <XCircle className="w-4 h-4 text-[var(--shield-red)]" />
            <span className="text-[13px] font-semibold text-[var(--shield-red)]">
              Error
            </span>
          </div>
          <p className="text-[12px] text-[var(--shield-red)] font-hash ml-6">
            {error}
          </p>
        </div>
      )}

      {/* Result Card */}
      {result && (
        <div className="card-flat overflow-hidden animate-slide-up">
          <div className="px-5 py-4 border-b border-[var(--accent)]/20 bg-[var(--accent-light)]">
            <div className="flex items-center gap-3">
              <CheckCircle className="w-5 h-5 text-[var(--accent)]" />
              <div>
                <div className="text-[14px] font-semibold text-[var(--accent)]">
                  Hash Computed
                </div>
                <div className="text-[11px] text-[var(--text-tertiary)] flex items-center gap-1.5 mt-0.5">
                  <Clock className="w-3 h-3" />
                  {formatTime(result.timeMs)} &middot;{" "}
                  {result.inputByteLen} bytes input &middot;{" "}
                  {mode === "derive" ? "derive_key" : "plain"}
                </div>
              </div>
            </div>
          </div>

          <div className="p-5">
            <span className="text-[10px] text-[var(--text-tertiary)] uppercase tracking-label block mb-1">
              BLAKE3 Output (32 bytes / 64 hex chars)
            </span>
            <div className="flex items-center gap-2 bg-[var(--bg)] border border-[var(--border-light)] rounded-lg px-3 py-2.5">
              <span className="text-[12px] font-hash text-[var(--accent)] break-all select-all flex-1">
                {result.hash}
              </span>
              <CopyButton text={result.hash} />
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export default function VerifyPage() {
  const [activeTab, setActiveTab] = useState<Tab>("transaction");

  return (
    <div className="flex flex-col gap-6 max-w-[900px] mx-auto">
      {/* Hero */}
      <div className="text-center py-8">
        <div className="inline-flex items-center gap-2 px-3 py-1.5 rounded-full bg-[var(--accent-light)] border border-[var(--accent)]/20 mb-4">
          <Shield className="w-3.5 h-3.5 text-[var(--accent)]" />
          <span className="text-[12px] font-medium text-[var(--accent)]">
            Independent Verification
          </span>
        </div>
        <h1 className="text-[28px] md:text-[36px] font-bold tracking-tight flex items-center justify-center gap-3">
          <Shield className="w-8 h-8 text-[var(--accent)] hidden sm:block" />
          ARC Proof Verification Engine
        </h1>
        <p className="text-[14px] text-[var(--text-secondary)] mt-2 max-w-xl mx-auto">
          Independently verify any transaction on the ARC Chain &mdash; your
          browser recomputes every hash. No trust required.
        </p>
      </div>

      {/* Tab Bar */}
      <div className="flex items-center gap-1 card-flat p-1.5 overflow-x-auto">
        {TABS.map(({ key, label, icon: Icon }) => {
          const active = activeTab === key;
          return (
            <button
              key={key}
              onClick={() => setActiveTab(key)}
              className={`flex items-center gap-1.5 px-3 py-2 rounded-lg text-[12px] sm:text-[13px] font-medium transition-all whitespace-nowrap ${
                active
                  ? "bg-[var(--accent-light)] text-[var(--accent)] border border-[var(--accent)]/20"
                  : "text-[var(--text-secondary)] hover:text-[var(--text)] hover:bg-[var(--bg)]"
              }`}
            >
              <Icon className="w-3.5 h-3.5" />
              {label}
            </button>
          );
        })}
      </div>

      {/* Tab Content */}
      {activeTab === "transaction" && <VerifyTransactionTab />}
      {activeTab === "merkle" && <VerifyMerkleTab />}
      {activeTab === "raw" && <RawHashTab />}

      {/* Footer note */}
      <div className="text-center py-4 border-t border-[var(--border-light)]">
        <div className="flex items-center justify-center gap-2 text-[11px] text-[var(--text-tertiary)]">
          <ShieldCheck className="w-3.5 h-3.5 text-[var(--shield-green)]" />
          <span>
            All computations run locally in your browser using{" "}
            <span className="font-hash text-[var(--text-secondary)]">
              @noble/hashes
            </span>{" "}
            (pure JS, zero WASM, audited by Ethereum Foundation)
          </span>
        </div>
      </div>
    </div>
  );
}
