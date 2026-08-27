import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  ClipboardCheck,
  Copy,
  Droplet,
  QrCode,
  Send,
  Wallet as WalletIcon,
} from "lucide-react";
import { FormEvent, useEffect, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { InfoPopover } from "../components/InfoPopover";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { formatArcExact, formatInt } from "../lib/format";
import type { TxLookup, WalletTxResult } from "../lib/types";

function receiptView(result: WalletTxResult, lookup?: TxLookup) {
  const mined = lookup ? lookup.status === "mined" : result.mined;
  const success = lookup
    ? lookup.status === "mined"
      ? lookup.success
      : null
    : result.success;
  const blockHeight = lookup?.blockHeight ?? result.blockHeight;
  if (mined && success === true) {
    return {
      tone: "var(--success)",
      text: `Confirmed ${formatArcExact(result.amountArc)} ARC${blockHeight != null ? ` in block #${formatInt(blockHeight)}` : ""}`,
    };
  }
  if (mined && success === false) {
    return {
      tone: "var(--danger)",
      text: `Mined but failed${blockHeight != null ? ` in block #${formatInt(blockHeight)}` : ""} — no credit is claimed`,
    };
  }
  if (lookup?.status === "error" || lookup?.status === "invalid_hash") {
    return {
      tone: "var(--warning)",
      text: "Submitted; receipt status is currently unavailable",
    };
  }
  if (!lookup && result.receiptStatus === "receipt_unavailable") {
    return {
      tone: "var(--warning)",
      text: "Submitted; receipt status is currently unavailable",
    };
  }
  if (!lookup && result.mined && result.success === null) {
    return {
      tone: "var(--warning)",
      text: "Mined, but the receipt did not report execution success",
    };
  }
  return {
    tone: "var(--warning)",
    text: `Submitted ${formatArcExact(result.amountArc)} ARC · waiting for a mined receipt`,
  };
}

export function Wallet() {
  const queryClient = useQueryClient();
  const identity = useAppStore((s) => s.identity);

  const {
    data: balance,
    isError: balanceIsError,
    error: balanceError,
  } = useQuery({
    queryKey: ["balance"],
    queryFn: api.fetchBalance,
    refetchInterval: 4000,
  });

  const [recipient, setRecipient] = useState("");
  const [amountArc, setAmountArc] = useState("");
  const [trackedTx, setTrackedTx] = useState<WalletTxResult | null>(null);

  const faucet = useMutation({
    mutationFn: () => api.faucetClaim(),
    onSuccess: setTrackedTx,
  });

  const send = useMutation({
    mutationFn: () => api.sendArc(recipient.trim(), amountArc.trim()),
    onSuccess: (result) => {
      setTrackedTx(result);
      setAmountArc("");
    },
  });

  const { data: trackedReceipt } = useQuery({
    queryKey: ["wallet-receipt", trackedTx?.txHash],
    queryFn: () => api.lookupTx(trackedTx!.txHash),
    enabled: Boolean(trackedTx?.txHash),
    refetchInterval: (query) =>
      query.state.data?.status === "mined" ? false : 2000,
  });

  useEffect(() => {
    if (trackedReceipt?.status === "mined") {
      queryClient.invalidateQueries({ queryKey: ["balance"] });
    }
  }, [queryClient, trackedReceipt?.status]);

  const [copied, setCopied] = useState(false);

  const copyAddr = async () => {
    if (!identity?.address) return;
    await navigator.clipboard.writeText(identity.address);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  const submitTransfer = (event: FormEvent) => {
    event.preventDefault();
    send.mutate();
  };

  const receiptFor = (result: WalletTxResult | undefined) =>
    result?.txHash === trackedTx?.txHash ? trackedReceipt : undefined;

  return (
    <div className="main-inner" data-testid="wallet-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Wallet</h1>
          <p className="page-subtitle">
            Manage your ARC balance, receive funds, or claim from the testnet
            faucet.
          </p>
        </div>
      </div>

      <Card featured style={{ marginBottom: "var(--space-6)" }}>
        <CardHeader
          title={
            <span style={{ display: "inline-flex", alignItems: "center" }}>
              Balance
              <InfoPopover title="On-chain balance">
                <p>
                  Your balance is fetched from the chain via{" "}
                  <code>GET /account/&lt;address&gt;</code>. It reflects every
                  successful transaction retained by this selected host:
                  faucet credits, transfers, and mined community-reward
                  transactions (<code>0x25</code>). A raw inference attestation
                  (<code>0x16</code>) is not payment.
                </p>
                <p style={{ color: "var(--text-muted)", fontSize: 11 }}>
                  Updates every 4 seconds.
                </p>
              </InfoPopover>
            </span>
          }
          action={
            <span
              className="status-pill info"
              data-testid="wallet-nonce"
              title="Transaction counter - increments with every tx you send"
            >
              Nonce {balance ? formatInt(balance.nonce) : "—"}
            </span>
          }
        />
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "1fr auto",
            gap: "var(--space-6)",
            alignItems: "flex-end",
          }}
        >
          <div>
            <div
              className="big-number gradient"
              data-testid="wallet-balance"
              style={{ fontSize: "var(--text-4xl)" }}
            >
              {balance ? formatArcExact(balance.balanceArc) : "—"}
              <span className="unit">ARC</span>
            </div>
            {balanceIsError && (
              <div
                data-testid="wallet-balance-error"
                style={{
                  marginTop: "var(--space-2)",
                  color: "var(--warning)",
                  fontSize: "var(--text-xs)",
                }}
              >
                Balance unavailable: {(balanceError as Error).message}
              </div>
            )}
            {balance && balance.stakedBalanceBase !== "0" && (
              <div
                style={{
                  marginTop: "var(--space-3)",
                  color: "var(--text-muted)",
                  fontSize: "var(--text-sm)",
                }}
              >
                {formatArcExact(balance.stakedBalanceArc)} ARC staked
              </div>
            )}
          </div>
          <div>
            <button
              className="btn btn-primary btn-lg"
              onClick={() => faucet.mutate()}
              disabled={faucet.isPending}
              data-testid="btn-faucet"
            >
              <Droplet size={16} />{" "}
              {faucet.isPending ? "Submitting…" : "Claim 1 ARC"}
            </button>
            {faucet.isSuccess && (
              <div
                style={{
                  marginTop: "var(--space-2)",
                  fontSize: "var(--text-xs)",
                  color: receiptView(
                    faucet.data,
                    receiptFor(faucet.data),
                  ).tone,
                  textAlign: "right",
                  fontFamily: "var(--font-mono)",
                }}
                data-testid="faucet-success"
              >
                {receiptView(faucet.data, receiptFor(faucet.data)).text}
                <br />tx {faucet.data.txHash.slice(0, 10)}…
              </div>
            )}
            {faucet.isError && (
              <div
                style={{
                  marginTop: "var(--space-2)",
                  fontSize: "var(--text-xs)",
                  color: "var(--danger)",
                  textAlign: "right",
                }}
                data-testid="faucet-error"
              >
                {(faucet.error as Error).message}
              </div>
            )}
          </div>
        </div>
      </Card>

      <div
        style={{
          display: "grid",
          gridTemplateColumns: "1fr 1fr",
          gap: "var(--space-4)",
          marginBottom: "var(--space-6)",
        }}
      >
        <Card>
          <CardHeader
            title={
              <span style={{ display: "inline-flex", alignItems: "center" }}>
                Receive <QrCode size={14} style={{ marginLeft: 8, color: "var(--text-muted)" }} />
              </span>
            }
          />
          <p
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-sm)",
              marginBottom: "var(--space-3)",
            }}
          >
            Share your address to receive ARC.
          </p>
          <div
            style={{
              padding: "var(--space-3) var(--space-4)",
              background: "var(--bg)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-md)",
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-xs)",
              color: "var(--text)",
              wordBreak: "break-all",
              marginBottom: "var(--space-3)",
              lineHeight: 1.6,
            }}
            data-testid="receive-address"
          >
            {identity?.address ?? "-"}
          </div>
          <button
            className="btn btn-secondary"
            onClick={copyAddr}
            disabled={!identity}
            data-testid="btn-copy-receive"
            style={{ width: "100%", justifyContent: "center" }}
          >
            {copied ? (
              <>
                <ClipboardCheck size={14} /> Copied to clipboard
              </>
            ) : (
              <>
                <Copy size={14} /> Copy address
              </>
            )}
          </button>
        </Card>

        <Card>
          <CardHeader
            title={
              <span style={{ display: "inline-flex", alignItems: "center" }}>
                Send
                <InfoPopover title="Send ARC">
                  <p>
                    Every transfer is signed in the native Rust process with
                    this wallet&apos;s ed25519 key. The recovery phrase is never
                    sent to the WebView or included in an IPC command.
                  </p>
                  <p>
                    ARC uses exactly nine decimal places. Extra digits are
                    rejected rather than rounded, and the app reports a
                    transfer as confirmed only after a successful mined
                    receipt exists.
                  </p>
                </InfoPopover>
              </span>
            }
          />
          <form onSubmit={submitTransfer} data-testid="send-arc-form">
            <label className="field-label" htmlFor="send-recipient">
              Recipient (64 hex characters)
            </label>
            <input
              id="send-recipient"
              className="input input-mono"
              value={recipient}
              onChange={(event) => setRecipient(event.target.value)}
              placeholder="0x…"
              autoComplete="off"
              spellCheck={false}
              data-testid="send-recipient"
              style={{ marginBottom: "var(--space-3)" }}
            />
            <label className="field-label" htmlFor="send-amount">
              Amount (ARC)
            </label>
            <input
              id="send-amount"
              className="input input-mono"
              value={amountArc}
              onChange={(event) => setAmountArc(event.target.value)}
              placeholder="0.000000001"
              inputMode="decimal"
              autoComplete="off"
              data-testid="send-amount"
              style={{ marginBottom: "var(--space-3)" }}
            />
            <button
              className="btn btn-primary"
              type="submit"
              disabled={
                send.isPending || recipient.trim() === "" || amountArc.trim() === ""
              }
              data-testid="btn-send-arc"
              style={{ width: "100%", justifyContent: "center" }}
            >
              <Send size={14} /> {send.isPending ? "Signing and submitting…" : "Send ARC"}
            </button>
          </form>
          {send.isSuccess && (
            <div
              data-testid="send-status"
              style={{
                marginTop: "var(--space-3)",
                color: receiptView(send.data, receiptFor(send.data)).tone,
                fontFamily: "var(--font-mono)",
                fontSize: "var(--text-xs)",
                lineHeight: 1.6,
              }}
            >
              {receiptView(send.data, receiptFor(send.data)).text}
              <br />tx {send.data.txHash.slice(0, 12)}…
            </div>
          )}
          {send.isError && (
            <div
              data-testid="send-error"
              style={{
                marginTop: "var(--space-3)",
                color: "var(--danger)",
                fontSize: "var(--text-xs)",
                lineHeight: 1.5,
              }}
            >
              {(send.error as Error).message}
            </div>
          )}
        </Card>
      </div>

      <Card>
        <CardHeader title="Testnet notice" />
        <div
          style={{
            display: "flex",
            gap: "var(--space-3)",
            alignItems: "flex-start",
            color: "var(--text-secondary)",
            fontSize: "var(--text-sm)",
            lineHeight: 1.55,
          }}
        >
          <AlertTriangle
            size={16}
            style={{ color: "var(--warning)", flexShrink: 0, marginTop: 2 }}
          />
          <div>
            These tokens are testnet ARC - no monetary value. Mainnet uses a
            separate address space: <code>0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499</code> (ETH L1, ERC-20).
            Your testnet identity will not port to mainnet.
          </div>
        </div>
      </Card>
    </div>
  );
}

export { WalletIcon };
