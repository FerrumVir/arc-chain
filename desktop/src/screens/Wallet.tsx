import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  ClipboardCheck,
  Copy,
  Droplet,
  Info,
  QrCode,
  Wallet as WalletIcon,
} from "lucide-react";
import { useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { InfoPopover } from "../components/InfoPopover";
import { NumberTicker } from "../components/NumberTicker";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { formatInt } from "../lib/format";

export function Wallet() {
  const queryClient = useQueryClient();
  const identity = useAppStore((s) => s.identity);

  const { data: balance } = useQuery({
    queryKey: ["balance"],
    queryFn: api.fetchBalance,
    refetchInterval: 4000,
  });

  const faucet = useMutation({
    mutationFn: () => api.faucetClaim(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["balance"] });
    },
  });

  const [copied, setCopied] = useState(false);

  const copyAddr = async () => {
    if (!identity?.address) return;
    await navigator.clipboard.writeText(identity.address);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

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
                  confirmed transaction: faucet drops, earnings from verified
                  inference, and transfers in or out.
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
              title="Transaction counter — increments with every tx you send"
            >
              Nonce {formatInt(balance?.nonce ?? 0)}
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
              <NumberTicker value={balance?.balance ?? 0} digits={0} />
              <span className="unit">ARC</span>
            </div>
            {balance && balance.stakedBalance > 0 && (
              <div
                style={{
                  marginTop: "var(--space-3)",
                  color: "var(--text-muted)",
                  fontSize: "var(--text-sm)",
                }}
              >
                {formatInt(balance.stakedBalance)} ARC staked
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
              {faucet.isPending ? "Claiming…" : "Claim 10,000 ARC"}
            </button>
            {faucet.isSuccess && (
              <div
                style={{
                  marginTop: "var(--space-2)",
                  fontSize: "var(--text-xs)",
                  color: "var(--success)",
                  textAlign: "right",
                  fontFamily: "var(--font-mono)",
                }}
                data-testid="faucet-success"
              >
                +{formatInt(faucet.data?.amount ?? 0)} ARC · tx{" "}
                {faucet.data?.txHash.slice(0, 10)}…
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
            {identity?.address ?? "—"}
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
                    On testnet, transactions are unsigned-OK so you can send
                    without a hardware wallet.
                  </p>
                  <p>
                    On <strong>mainnet</strong>, transactions must be ed25519-
                    signed. This app will move signing into the OS keychain
                    before mainnet ships.
                  </p>
                </InfoPopover>
              </span>
            }
          />
          <div
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-sm)",
              marginBottom: "var(--space-4)",
            }}
          >
            Coming in v0.2 — secure ed25519 signing is a prerequisite and
            lands with keychain integration.
          </div>
          <div
            style={{
              padding: "var(--space-3) var(--space-4)",
              background: "var(--warning-bg)",
              border: "1px solid rgba(245, 181, 81, 0.2)",
              borderRadius: "var(--radius-sm)",
              display: "flex",
              gap: "var(--space-3)",
              alignItems: "flex-start",
              fontSize: "var(--text-sm)",
            }}
          >
            <Info
              size={14}
              style={{ color: "var(--warning)", flexShrink: 0, marginTop: 2 }}
            />
            <div>
              For now, send from the CLI wallet:
              <br />
              <code style={{ color: "var(--arc-ink)" }}>
                arc-cli send &lt;to&gt; &lt;amount&gt;
              </code>
            </div>
          </div>
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
            These tokens are testnet ARC — no monetary value. Mainnet uses a
            separate address space: <code>0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499</code> (ETH L1, ERC-20).
            Your testnet identity will not port to mainnet.
          </div>
        </div>
      </Card>
    </div>
  );
}

export { WalletIcon };
