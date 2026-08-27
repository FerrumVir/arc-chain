import { useQuery } from "@tanstack/react-query";
import { FileSignature, Search } from "lucide-react";
import { Card, CardHeader } from "../components/Card";
import { EmptyState } from "../components/EmptyState";
import { NumberTicker } from "../components/NumberTicker";
import { ProjectedEarnings } from "../components/ProjectedEarnings";
import { api } from "../lib/tauri";
import { formatHash, formatInt, formatRelativeTime } from "../lib/format";
import { useAppStore } from "../lib/store";

export function Earnings() {
  const lookupHash = useAppStore((s) => s.lookupHash);
  const { data: earnings } = useQuery({
    queryKey: ["earnings"],
    queryFn: api.fetchEarnings,
    refetchInterval: 3000,
  });
  const { data: attestations } = useQuery({
    queryKey: ["attestations"],
    queryFn: () => api.fetchAttestations(200),
    refetchInterval: 5000,
  });

  const mineCount = (attestations ?? []).filter((a) => a.mine).length;
  // The legacy attestation feed is 0x16 computation claims. It must never
  // make the reward screen look funded. Only the selected host's mined reward
  // receipt index can establish that a receipt exists; a zero-valued receipt
  // must not be relabelled as "no receipt."
  const hasConfirmedRewards =
    earnings?.fromChain === true && earnings.attestations > 0;

  return (
    <div className="main-inner" data-testid="earnings-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Earnings</h1>
          <p className="page-subtitle">
            Successful mined community-reward receipts reported by the selected
            chain host. Testnet ARC has no monetary value.
          </p>
        </div>
      </div>

      {/* Primary home for the projection. Placed above lifetime totals
          because "what will this earn me" is the question people arrive with,
          and it is the one figure that must never be guessed. */}
      <div style={{ marginBottom: "var(--space-6)" }} data-testid="earnings-projection">
        <ProjectedEarnings />
      </div>

      {/* When nothing has been earned, three cards reading "0.00 ARC" next
          to a "Last 7 days" chart of empty bars told the user nothing and
          implied something had gone wrong. Explain the mechanism instead. */}
      {!hasConfirmedRewards ? (
        <Card style={{ marginBottom: "var(--space-6)" }} data-testid="earnings-empty">
          <CardHeader title="No mined reward receipts yet" />
          <div
            style={{
              color: "var(--text-secondary)",
              fontSize: "var(--text-sm)",
              lineHeight: 1.7,
            }}
          >
            <p style={{ marginTop: 0 }}>
              A raw <code>InferenceAttestation</code> (<code>0x16</code>) is a
              computation claim and pays nothing. The candidate&rsquo;s
              configured <strong>2.50 ARC</strong> amount applies only when a
              separate <code>CommunityInferenceReward</code> (<code>0x25</code>)
              transaction succeeds in a mined block.
            </p>
            <p>All of these gates have to pass first:</p>
            <ol style={{ paddingLeft: "1.2em" }}>
              <li>
                The node fully loaded the <strong>exact model artifact</strong>{" "}
                requested; a matching filename or model shape is not enough.
              </li>
              <li>
                A coordinator actually <strong>assigned work</strong> to this
                worker and independently verified every token through its
                authenticated range quorum.
              </li>
              <li>
                Reward activation and validator approval collection are ready,
                and the signed worker certificate receives strict
                greater-than-two-thirds validator identity and active-stake
                approval. Six equal validators require five approvals.
              </li>
              <li>
                The resulting <code>0x25</code> transaction is included with a
                <strong>successful mined receipt</strong> on this same chain
                host. Pending, failed, pruned, or raw <code>0x16</code> records
                are not counted.
              </li>
            </ol>
            <p style={{ marginBottom: 0, color: "var(--text-muted)" }}>
              Reward issuance fails closed whenever activation, validator
              approval collection, treasury capacity, or mined-receipt
              evidence is unavailable. Running a prompt from the Inference tab
              tests the selected inference path; it does not guarantee that
              your worker receives the job or a reward.
            </p>
          </div>
        </Card>
      ) : (
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(3, 1fr)",
            gap: "var(--space-4)",
            marginBottom: "var(--space-6)",
          }}
        >
          <Card>
            <div className="stat-label">Today</div>
            <div className="big-number" style={{ marginTop: "var(--space-2)" }}>
              {/* null ≠ 0. The chain not reporting a daily figure is not the
                  same as having earned nothing today. */}
              {earnings?.todayArc != null ? (
                <>
                  <NumberTicker value={earnings.todayArc} digits={2} />
                  <span className="unit">ARC</span>
                </>
              ) : (
                <span
                  style={{ color: "var(--text-muted)" }}
                  title="This chain host doesn't report a daily breakdown."
                >
                  —
                </span>
              )}
            </div>
          </Card>
          <Card>
            <div className="stat-label">Lifetime</div>
            <div
              className="big-number gradient"
              style={{ marginTop: "var(--space-2)" }}
            >
              <NumberTicker value={earnings?.totalArc ?? 0} digits={2} />
              <span className="unit">ARC</span>
            </div>
          </Card>
          <Card>
            <div className="stat-label">Last reward receipt</div>
            {/* Replaces the old invented "Pending" card. The candidate
                endpoint reports the block of the last successful mined 0x25
                reward receipt; absent remains absent. */}
            <div className="big-number" style={{ marginTop: "var(--space-2)" }}>
              {earnings?.lastPayoutBlock != null ? (
                <span style={{ fontSize: "0.7em" }}>
                  block #{formatInt(earnings.lastPayoutBlock)}
                </span>
              ) : earnings?.lastPayoutAt != null ? (
                <span style={{ fontSize: "0.7em" }}>
                  {formatRelativeTime(earnings.lastPayoutAt)}
                </span>
              ) : (
                <span style={{ color: "var(--text-muted)" }}>—</span>
              )}
            </div>
          </Card>
        </div>
      )}

      <Card>
        <CardHeader
          title="Inference claims (not payments)"
          action={
            <span
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
              }}
            >
              {/* Only address attribution is claimed. These 0x16 records are
                  never converted into income. */}
              {formatInt(mineCount)} yours ·{" "}
              {formatInt(attestations?.length ?? 0)} shown
            </span>
          }
        />
        <div className="feed" data-testid="all-attestations">
          {!attestations || attestations.length === 0 ? (
            <EmptyState
              icon={FileSignature}
              title="No inference claims on this host"
              description="Running a prompt can test the selected inference path, but it does not guarantee worker assignment or payment."
            />
          ) : (
            attestations.map((a) => (
              <div key={a.txHash} className="feed-item">
                <div className="feed-item-icon">
                  <FileSignature />
                </div>
                <div className="feed-item-body">
                  <div className="feed-item-title">
                    {a.inputPreview || (
                      <span style={{ color: "var(--text-muted)" }}>
                        Inference attestation
                      </span>
                    )}
                  </div>
                  <div className="feed-item-meta">
                    <span>{formatHash(a.txHash, 10)}</span>
                    {a.tokens != null && <span>{a.tokens} tok</span>}
                    {a.latencyMs != null && <span>{a.latencyMs}ms</span>}
                    {a.blockHeight != null && (
                      <span>#{formatInt(a.blockHeight)}</span>
                    )}
                    <span>
                      {a.timestamp != null
                        ? formatRelativeTime(a.timestamp)
                        : "recent"}
                    </span>
                  </div>
                </div>
                <div
                  style={{
                    fontFamily: "var(--font-mono)",
                    fontSize: "var(--text-sm)",
                    color: a.mine ? "var(--success)" : "var(--text-muted)",
                    fontWeight: 600,
                    flexShrink: 0,
                    whiteSpace: "nowrap",
                  }}
                  title={
                    a.mine
                      ? "Submitted by your address. A raw 0x16 claim is not payment."
                      : "Submitted by another address. A raw 0x16 claim is not payment."
                  }
                >
                  {a.mine ? "your claim" : "network claim"}
                </div>
                {/* Was openExternal to a hardcoded LAX :3200 URL — the wrong
                    host for this session and not a block explorer. Resolves
                    against the pinned chain host instead, which is the only
                    place this hash can be confirmed. */}
                <button
                  className="btn btn-ghost btn-sm"
                  onClick={() => lookupHash(a.txHash)}
                  data-testid={`btn-lookup-earnings-${a.txHash.slice(0, 10)}`}
                  aria-label="Look up on the pinned chain host"
                >
                  <Search size={13} />
                </button>
              </div>
            ))
          )}
        </div>
      </Card>
    </div>
  );
}
