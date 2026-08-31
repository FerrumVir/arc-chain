import { useMutation, useQuery } from "@tanstack/react-query";
import {
  AlertTriangle,
  ArrowUpRight,
  Blocks,
  ChevronDown,
  ChevronRight,
  FileSignature,
  Search,
  Server,
  Users,
  Waypoints,
} from "lucide-react";
import { useEffect, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { EmptyState } from "../components/EmptyState";
import { NotAvailable } from "../components/NotAvailable";
import { api } from "../lib/tauri";
import { formatHash, formatInt, formatRelativeTime, formatUptime } from "../lib/format";
import { hostLabel, hostLabelVerbose } from "../lib/hosts";
import { useAppStore } from "../lib/store";
import type {
  Attestation,
  NetworkOverview,
  RecentBlocks,
} from "../lib/types";

/**
 * The Network screen: check the chain, any time, without leaving the app.
 *
 * Everything here reads one selected chain host and attributes every value to
 * it. Fleet-wide canonicality, the signed legacy checkpoint, and preserved
 * forks belong in the composite explorer, which audits those sources instead
 * of blending them into one unqualified number.
 *
 * This screen replaced an "Explorer" button that opened
 * `http://140.82.16.112:3200` — a hardcoded LAX IP, serving a network
 * dashboard rather than a block explorer, for a chain that is usually not the
 * one the session is reading.
 */

/**
 * Block age past which the chain is lagging.
 *
 * Blocks seal continuously on a healthy host, so hundreds of seconds already
 * means something is wrong. Kept well below the stall threshold so a
 * degrading host is visible before it is fully stopped.
 */
const BLOCK_AGE_WARN_SECS = 300;

/**
 * Block age past which the host is not sealing blocks at all.
 *
 * This exists because `/health` cannot be trusted to say so. Four of the six
 * live seeds have not produced a block in roughly six days while still
 * answering `status: "ok"` with a healthy peer count, because their DAG rounds
 * keep advancing after block production stops. A user looking at a green
 * health pill has no way to know their work can never settle. One hour is far
 * beyond any legitimate gap.
 */
const BLOCK_AGE_STALL_SECS = 3600;

/** ARC base units per whole ARC, for rendering validator stake. */
const ARC_BASE_UNITS = 1_000_000_000;

/** Public composite explorer deployed from this repository's Pages artifact. */
const COMPOSITE_EXPLORER_URL =
  "https://ferrumvir.github.io/arc-chain/explorer/";

export function Network() {
  const pendingLookup = useAppStore((s) => s.pendingLookup);
  const clearPendingLookup = useAppStore((s) => s.clearPendingLookup);

  const { data: overview } = useQuery({
    queryKey: ["network-overview"],
    queryFn: api.fetchNetworkOverview,
    refetchInterval: 10_000,
  });
  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 5_000,
  });
  const { data: recentBlocks } = useQuery({
    queryKey: ["recent-blocks"],
    queryFn: () => api.fetchRecentBlocks(10),
    refetchInterval: 15_000,
  });
  const { data: attestations } = useQuery({
    queryKey: ["attestations"],
    queryFn: () => api.fetchAttestations(50),
    refetchInterval: 15_000,
  });

  // The host every number on this screen came from. Falls back to the status
  // poll's copy so the header can still attribute figures while the overview
  // request is in flight.
  const host = overview?.sourceHost ?? status?.chainHost ?? null;

  const age = overview?.lastBlockAgeSecs ?? null;
  // Two independent signals: our own block-age reading, and the host's own
  // verdict. Either is enough to warn. They are combined rather than one
  // preferred, because a host that has stopped sealing is exactly the host
  // whose self-report is least worth trusting alone.
  const hostSaysStopped = overview?.isBlockProducing === false;
  const stalled =
    (age !== null && age >= BLOCK_AGE_STALL_SECS) ||
    (hostSaysStopped && (age === null || age >= BLOCK_AGE_WARN_SECS));
  const lagging =
    !stalled && age !== null && age >= BLOCK_AGE_WARN_SECS;

  return (
    <div className="main-inner" data-testid="network-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Network</h1>
          {/* The subtitle never asserts which network this is unless the host
              said so. `/info` reports the constant string "ARC Chain" on every
              deployment, so it cannot tell a testnet from a mainnet and is
              deliberately not used as a substitute. */}
          <p className="page-subtitle" data-testid="network-identity">
            {overview?.networkName ? (
              <>
                <strong>{overview.networkName}</strong>, read from{" "}
                {hostLabelVerbose(host)}
                {overview.hostVersion && <> running arc-node {overview.hostVersion}</>}.
                {/* The ONLY statement this app makes about mainnet, and only
                    when the host's genesis declares it. `null` means the host
                    did not say, and then neither does the UI. */}
                {overview.declaresMainnet === true && (
                  <>
                    {" "}
                    <strong>This host declares itself mainnet.</strong>
                  </>
                )}
                {overview.declaresMainnet === false && (
                  <> Its genesis does not declare mainnet.</>
                )}
              </>
            ) : (
              <>
                Reading {hostLabelVerbose(host)}
                {overview?.hostVersion && <> (arc-node {overview.hostVersion})</>}.{" "}
                <strong>Network name unknown</strong> —{" "}
                {overview?.networkNameUnavailableReason ??
                  "this host does not report a network name"}
                . The app will not name the network on its behalf.
              </>
            )}
          </p>
        </div>
        <div style={{ display: "flex", gap: "var(--space-2)", flexWrap: "wrap" }}>
          <button
            className="btn btn-primary"
            onClick={() => api.openExternal(COMPOSITE_EXPLORER_URL)}
            data-testid="btn-open-composite-explorer"
            title="Opens ARC's canonical checkpoint, protocol-v3 continuation, and explicit preserved-fork views."
          >
            <ArrowUpRight size={14} /> Composite explorer
          </button>
          {host && (
            <button
              className="btn btn-secondary"
              onClick={() => api.openExternal(`${host}/block/latest`)}
              data-testid="btn-open-raw-json"
              title={`Opens ${host}/block/latest in your browser — the newest block header as this host serves it.`}
            >
              <ArrowUpRight size={14} /> Raw block JSON ({hostLabel(host)})
            </button>
          )}
        </div>
      </div>

      {overview?.unavailable && (
        <div style={{ marginBottom: "var(--space-6)" }}>
          <NotAvailable
            reason={overview.unavailable}
            title="Could not read the chain"
            testId="overview-unavailable"
          />
        </div>
      )}

      {/* The warning that `/health` cannot give. */}
      {(stalled || lagging) && (
        <div
          role="alert"
          data-testid={stalled ? "not-sealing-banner" : "block-lag-banner"}
          style={{
            margin: "0 0 var(--space-6)",
            padding: "12px 16px",
            border: `1px solid ${stalled ? "var(--danger)" : "rgba(251, 191, 36, 0.4)"}`,
            background: stalled
              ? "var(--danger-bg)"
              : "var(--warning-bg)",
            color: stalled ? "var(--danger)" : "var(--warning)",
            borderRadius: 8,
            fontSize: 13,
            display: "flex",
            gap: 12,
            alignItems: "flex-start",
            lineHeight: 1.6,
          }}
        >
          <AlertTriangle size={16} style={{ flexShrink: 0, marginTop: 2 }} />
          <div>
            {stalled ? (
              <>
                <strong>This host is not sealing blocks.</strong>{" "}
                {age !== null && <>Its newest block is {formatUptime(age)} old. </>}
                It still reports healthy — its DAG round keeps advancing after
                block production stops, so the health check cannot see this.{" "}
                <strong>
                  This host cannot mine a new claim or reward receipt while it
                  is not sealing blocks.
                </strong>
                {overview?.isBlockProducingBasis && (
                  <>
                    {" "}
                    <span style={{ opacity: 0.85 }}>
                      The host's own basis: {overview.isBlockProducingBasis}.
                    </span>
                  </>
                )}
              </>
            ) : (
              <>
                <strong>Blocks are lagging.</strong> The newest block on this
                host is {formatUptime(age!)} old. Anything under a few seconds
                is normal; this is not.
              </>
            )}
          </div>
        </div>
      )}

      <div className="grid-stats" style={{ marginBottom: "var(--space-6)" }}>
        <StatCard
          label="Block height"
          icon={Blocks}
          value={overview?.height != null ? formatInt(overview.height) : null}
          note={host ? `on ${hostLabel(host)}` : undefined}
        />
        <StatCard
          label="Last block"
          icon={Blocks}
          value={age != null ? `${formatUptime(age)} ago` : null}
          tone={stalled ? "danger" : lagging ? "warning" : undefined}
          note="from the block header's own timestamp"
        />
        <StatCard
          label="Validators"
          icon={Users}
          value={
            overview?.validatorsActive != null &&
            overview?.validatorsRegistered != null
              ? `${formatInt(overview.validatorsActive)} / ${formatInt(overview.validatorsRegistered)}`
              : null
          }
          note="active (stake > 0) of registered"
        />
        <StatCard
          label="Peers"
          icon={Server}
          value={overview?.peers != null ? formatInt(overview.peers) : null}
          note={`this host's peers, not yours`}
        />
        <StatCard
          label="DAG round"
          icon={Waypoints}
          value={
            overview?.dagRound != null ? formatInt(overview.dagRound) : null
          }
          note={
            overview?.dagCommitted != null
              ? `committed ${formatInt(overview.dagCommitted)}`
              : undefined
          }
        />
      </div>

      <TxLookupCard
        prefill={pendingLookup}
        onConsumePrefill={clearPendingLookup}
      />

      <ValidatorSplit overview={overview} />

      <RecentBlocksCard blocks={recentBlocks} />

      <RecentInferenceCard attestations={attestations} host={host} />
    </div>
  );
}

/** A stat that renders an em dash and a reason-free blank rather than a zero
 *  when the host did not report it. */
function StatCard({
  label,
  value,
  icon: Icon,
  note,
  tone,
}: {
  label: string;
  value: string | null;
  icon: typeof Blocks;
  note?: string;
  tone?: "warning" | "danger";
}) {
  // The value carries the testid, not the card: a testid on the wrapper makes
  // `toHaveText` match the label and footnote too, which turns an assertion
  // about a number into an assertion about prose.
  const slug = label.toLowerCase().replace(/[^a-z]+/g, "-");
  return (
    <Card data-testid={`net-card-${slug}`}>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: "var(--space-3)",
        }}
      >
        <span className="stat-label">{label}</span>
        <Icon size={14} style={{ color: "var(--text-muted)" }} />
      </div>
      <div
        className="stat-value"
        data-testid={`net-stat-${slug}`}
        style={{
          color:
            tone === "danger"
              ? "var(--danger)"
              : tone === "warning"
                ? "var(--warning)"
                : undefined,
        }}
      >
        {/* null = the host did not report it. Never rendered as 0. */}
        {value ?? <span style={{ color: "var(--text-muted)" }}>—</span>}
      </div>
      {note && (
        <div
          style={{
            marginTop: "var(--space-2)",
            fontSize: "var(--text-xs)",
            color: "var(--text-muted)",
          }}
        >
          {note}
        </div>
      )}
    </Card>
  );
}

/**
 * Paste a hash, get an honest answer.
 *
 * The three outcomes are deliberately distinct. A 404 from `/tx/{hash}` is NOT
 * evidence that a hash is fake: the endpoint returns a receipt, and a
 * transaction still in the mempool has none. Telling a user who just submitted
 * an attestation that their hash is "invalid" would be wrong and would send
 * them looking for a bug that isn't there.
 */
function TxLookupCard({
  prefill,
  onConsumePrefill,
}: {
  prefill: string | null;
  onConsumePrefill: () => void;
}) {
  const [hash, setHash] = useState("");

  const lookup = useMutation({
    mutationFn: (h: string) => api.lookupTx(h),
  });

  // Arriving here from an attestation row's "look up" button.
  useEffect(() => {
    if (!prefill) return;
    setHash(prefill);
    lookup.mutate(prefill);
    onConsumePrefill();
    // `lookup` is a stable mutation object; re-running on its identity would
    // loop the request.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [prefill]);

  const result = lookup.data;

  return (
    <Card style={{ marginBottom: "var(--space-6)" }} data-testid="tx-lookup">
      <CardHeader title="Look up a transaction or attestation" />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (hash.trim()) lookup.mutate(hash.trim());
        }}
        style={{ display: "flex", gap: "var(--space-2)", flexWrap: "wrap" }}
      >
        <input
          className="input input-mono"
          style={{ flex: 1, minWidth: 220 }}
          placeholder="Paste a tx or attestation hash (0x optional)"
          value={hash}
          onChange={(e) => setHash(e.target.value)}
          data-testid="tx-lookup-input"
          aria-label="Transaction hash"
        />
        <button
          className="btn btn-primary"
          type="submit"
          disabled={!hash.trim() || lookup.isPending}
          data-testid="tx-lookup-submit"
        >
          <Search size={14} /> {lookup.isPending ? "Looking up…" : "Look up"}
        </button>
      </form>

      <p
        style={{
          marginTop: "var(--space-3)",
          marginBottom: 0,
          fontSize: "var(--text-xs)",
          color: "var(--text-muted)",
          lineHeight: 1.6,
        }}
      >
        Resolved against the selected chain host. For checkpoint-spanning
        history, replica agreement, or a preserved fork, use the composite
        explorer; this host-scoped lookup never blends sources.
      </p>

      {lookup.error && (
        <div style={{ marginTop: "var(--space-4)" }}>
          <NotAvailable
            reason={String(lookup.error)}
            title="Lookup failed"
            testId="tx-lookup-error"
          />
        </div>
      )}

      {result && (
        <div style={{ marginTop: "var(--space-4)" }} data-testid="tx-lookup-result">
          {result.status === "mined" && (
            <>
              <div
                className="status-pill"
                data-testid="tx-status-mined"
                style={{
                  background: "var(--success-bg)",
                  color: "var(--success)",
                  marginBottom: "var(--space-3)",
                }}
              >
                In a block
              </div>
              <div className="kv">
                <dt>Block</dt>
                <dd>
                  {result.blockHeight != null
                    ? `#${formatInt(result.blockHeight)}`
                    : "—"}
                </dd>
                <dt>Position in block</dt>
                <dd>{result.txIndex != null ? result.txIndex : "—"}</dd>
                <dt>Result</dt>
                <dd>
                  {result.success == null
                    ? "—"
                    : result.success
                      ? "succeeded"
                      : "failed"}
                </dd>
                <dt>Gas used</dt>
                <dd>
                  {result.gasUsed != null ? formatInt(result.gasUsed) : "—"}
                </dd>
                <dt>Block hash</dt>
                <dd className="mono" style={{ wordBreak: "break-all" }}>
                  {result.blockHash ? formatHash(result.blockHash, 16) : "—"}
                </dd>
              </div>
              <p
                style={{
                  margin: "var(--space-3) 0 0",
                  color: "var(--text-muted)",
                  fontSize: "var(--text-xs)",
                  lineHeight: 1.6,
                }}
              >
                This receipt proves inclusion and execution status on this
                host, but this endpoint does not expose transaction type. A
                community payment must separately be identified as a
                successful <code>0x25</code> reward transaction; a mined raw
                <code>0x16</code> inference claim pays nothing.
              </p>
            </>
          )}

          {/* The important one. "Not found" is what a pending tx looks like. */}
          {result.status === "not_found" && (
            <div data-testid="tx-status-not-found">
              <div
                className="status-pill info"
                style={{ marginBottom: "var(--space-3)" }}
              >
                Not in a block yet
              </div>
              <p
                style={{
                  margin: 0,
                  fontSize: "var(--text-sm)",
                  color: "var(--text-secondary)",
                  lineHeight: 1.7,
                }}
              >
                {hostLabelVerbose(result.sourceHost)} has no receipt for this
                hash. That means one of two things, and this app cannot tell
                them apart:{" "}
                <strong>it is waiting in the mempool</strong> and has not been
                sealed into a block yet, or it was never submitted to{" "}
                <em>this</em> chain. It does not mean the hash is invalid — a
                receipt only exists once a block includes the transaction.
              </p>
            </div>
          )}

          {result.status === "invalid_hash" && (
            <div data-testid="tx-status-invalid">
              <div
                className="status-pill"
                style={{
                  background: "var(--warning-bg)",
                  color: "var(--warning)",
                  marginBottom: "var(--space-3)",
                }}
              >
                Not a valid hash
              </div>
              <p
                style={{
                  margin: 0,
                  fontSize: "var(--text-sm)",
                  color: "var(--text-secondary)",
                }}
              >
                {result.unavailable}
              </p>
            </div>
          )}

          {result.status === "error" && result.unavailable && (
            <NotAvailable reason={result.unavailable} testId="tx-status-error" />
          )}
        </div>
      )}
    </Card>
  );
}

/**
 * Active vs registered validators.
 *
 * `/validators` reports one flat list with no such distinction, so the split is
 * derived here by counting stake > 0. It matters: zero-stake entries are
 * counted by `/health` and by the endpoint's own `count`, which inflates the
 * apparent size of the validator set above the number of nodes that can
 * actually lead a round.
 */
function ValidatorSplit({
  overview,
}: {
  overview: NetworkOverview | undefined;
}) {
  const [open, setOpen] = useState(false);
  const list = overview?.validators ?? [];
  const inactive = list.filter((v) => !v.active).length;

  if (list.length === 0) return null;

  return (
    <Card style={{ marginBottom: "var(--space-6)" }} data-testid="validator-split">
      <CardHeader
        title="Validators"
        action={
          <button
            className="btn btn-ghost btn-sm"
            onClick={() => setOpen((o) => !o)}
            data-testid="btn-toggle-validators"
          >
            {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
            {open ? "Hide" : "Show all"}
          </button>
        }
      />
      <p
        style={{
          margin: 0,
          fontSize: "var(--text-sm)",
          color: "var(--text-secondary)",
          lineHeight: 1.7,
        }}
      >
        <strong>{formatInt(overview?.validatorsActive ?? 0)}</strong> of{" "}
        <strong>{formatInt(overview?.validatorsRegistered ?? list.length)}</strong>{" "}
        registered validators hold stake above zero.
        {inactive > 0 && (
          <>
            {" "}
            The other {formatInt(inactive)} are counted in the set and in{" "}
            <code>/health</code> but hold no stake, so they cannot lead a round
            — the reported set is larger than the set that can produce blocks.
          </>
        )}{" "}
        <span style={{ color: "var(--text-muted)" }}>
          {overview?.validatorSplitDerived
            ? "The split is derived here by counting stake above zero, because this host does not report it."
            : overview?.minActiveStake != null
              ? `Reported by this host, which counts a validator active at ${formatInt(overview.minActiveStake)} stake or more.`
              : "Reported by this host."}
        </span>
      </p>

      {open && (
        <div className="feed" style={{ marginTop: "var(--space-4)" }} data-testid="validator-list">
          {list.map((v) => (
            <div key={v.address} className="feed-item">
              <div className="feed-item-body">
                <div
                  className="feed-item-title mono"
                  style={{ wordBreak: "break-all", fontSize: "var(--text-sm)" }}
                >
                  {formatHash(v.address, 20)}
                </div>
                <div className="feed-item-meta">
                  <span>
                    {v.stake > 0
                      ? `${formatInt(Math.round(v.stake / ARC_BASE_UNITS))} ARC staked`
                      : "no stake"}
                  </span>
                </div>
              </div>
              <div
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 600,
                  color: v.active ? "var(--success)" : "var(--text-muted)",
                  flexShrink: 0,
                }}
              >
                {v.active ? "active" : "zero-stake"}
              </div>
            </div>
          ))}
        </div>
      )}
    </Card>
  );
}

/** Recent blocks, newest first, with on-demand transaction expansion. */
function RecentBlocksCard({ blocks }: { blocks: RecentBlocks | undefined }) {
  const [expanded, setExpanded] = useState<number | null>(null);

  return (
    <Card style={{ marginBottom: "var(--space-6)" }} data-testid="recent-blocks">
      <CardHeader
        title="Recent blocks"
        action={
          blocks && !blocks.unavailable ? (
            <span
              style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}
            >
              {formatInt(blocks.blocks.length)} newest
            </span>
          ) : null
        }
      />
      {blocks?.unavailable ? (
        <NotAvailable reason={blocks.unavailable} testId="blocks-unavailable" />
      ) : !blocks || blocks.blocks.length === 0 ? (
        <EmptyState
          icon={Blocks}
          title="No blocks returned"
          description="This host answered but listed no blocks."
        />
      ) : (
        <div className="feed" data-testid="block-list">
          {blocks.blocks.map((b) => (
            <div key={b.height}>
              <div className="feed-item">
                <div className="feed-item-icon">
                  <Blocks />
                </div>
                <div className="feed-item-body">
                  <div className="feed-item-title">
                    Block #{formatInt(b.height)}
                  </div>
                  <div className="feed-item-meta">
                    <span className="mono">{formatHash(b.hash, 10)}</span>
                    {/* Omitted rather than shown as 0 when unreported. */}
                    {b.txCount != null && (
                      <span>
                        {formatInt(b.txCount)} tx
                        {b.txCount === 1 ? "" : "s"}
                      </span>
                    )}
                    {b.timestampMs != null && (
                      <span>{formatRelativeTime(b.timestampMs)}</span>
                    )}
                  </div>
                </div>
                {b.txCount != null && b.txCount > 0 && (
                  <button
                    className="btn btn-ghost btn-sm"
                    onClick={() =>
                      setExpanded((cur) => (cur === b.height ? null : b.height))
                    }
                    data-testid={`btn-expand-block-${b.height}`}
                    aria-expanded={expanded === b.height}
                  >
                    {expanded === b.height ? (
                      <ChevronDown size={13} />
                    ) : (
                      <ChevronRight size={13} />
                    )}
                  </button>
                )}
              </div>
              {expanded === b.height && <BlockTxList height={b.height} />}
            </div>
          ))}
        </div>
      )}
    </Card>
  );
}

/** Fetched only when a block is expanded — never on the polling path. */
function BlockTxList({ height }: { height: number }) {
  const lookupHash = useAppStore((s) => s.lookupHash);
  const { data, isLoading } = useQuery({
    queryKey: ["block-txs", height],
    queryFn: () => api.fetchBlockTxs(height),
  });

  if (isLoading) {
    return (
      <div
        style={{
          padding: "var(--space-3) var(--space-6)",
          fontSize: "var(--text-sm)",
          color: "var(--text-muted)",
        }}
      >
        Reading block #{formatInt(height)}…
      </div>
    );
  }
  if (!data) return null;
  if (data.unavailable) {
    return (
      <div style={{ padding: "var(--space-3) var(--space-6)" }}>
        <NotAvailable reason={data.unavailable} />
      </div>
    );
  }

  return (
    <div
      style={{ padding: "0 var(--space-4) var(--space-3) var(--space-6)" }}
      data-testid={`block-txs-${height}`}
    >
      {data.txs.length === 0 ? (
        <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
          This host listed no transaction bodies for the block.
        </div>
      ) : (
        data.txs.map((t) => (
          <div
            key={`${t.index}-${t.hash}`}
            style={{
              display: "flex",
              alignItems: "center",
              gap: "var(--space-3)",
              padding: "6px 0",
              fontSize: "var(--text-sm)",
              borderTop: "1px solid var(--border)",
            }}
          >
            <span style={{ color: "var(--text-muted)", minWidth: 24 }}>
              {t.index}
            </span>
            <span className="mono" style={{ flex: 1, wordBreak: "break-all" }}>
              {formatHash(t.hash, 16)}
            </span>
            {t.txType && (
              <span style={{ color: "var(--text-muted)" }}>{t.txType}</span>
            )}
            <button
              className="btn btn-ghost btn-sm"
              onClick={() => lookupHash(t.hash)}
              aria-label={`Look up transaction ${t.hash}`}
            >
              <Search size={12} />
            </button>
          </div>
        ))
      )}
    </div>
  );
}

/**
 * Recent mined inference activity, filtered to protocol inference records.
 *
 * The deployed seeds pad `/inference/attestations` with unrelated transactions
 * tagged `tx_type: "Other"` once genuine rows run out — at `limit=500` some
 * seeds returned 500 padding rows and zero real ones. Presenting those as
 * inference evidence on a screen whose whole job is letting someone check the
 * chain would be the worst place in the app to get this wrong. Rows are kept
 * only when the host labelled them `Inference` or
 * `CommunityInferenceReward`; how many were dropped is
 * stated rather than hidden.
 */
function RecentInferenceCard({
  attestations,
  host,
}: {
  attestations: Attestation[] | undefined;
  host: string | null;
}) {
  const lookupHash = useAppStore((s) => s.lookupHash);

  const all = attestations ?? [];
  // A row with no tx_type at all is kept: older adapters did not carry the
  // field, and dropping those would empty the list on hosts that are fine.
  const real = all.filter((a) =>
    a.txType == null
    || a.txType === "Inference"
    || a.txType === "InferenceAttestation"
    || a.txType === "CommunityInferenceReward");
  const dropped = all.length - real.length;

  return (
    <Card data-testid="recent-inference">
      <CardHeader
        title="Recent inference activity"
        action={
          <span style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
            {formatInt(real.length)} shown
            {dropped > 0 && <> · {formatInt(dropped)} filtered</>}
          </span>
        }
      />
      {dropped > 0 && (
        <p
          style={{
            margin: "0 0 var(--space-3)",
            fontSize: "var(--text-xs)",
            color: "var(--text-muted)",
            lineHeight: 1.6,
          }}
          data-testid="padding-filtered-note"
        >
          {formatInt(dropped)} row{dropped === 1 ? "" : "s"} from{" "}
          {hostLabel(host)} {dropped === 1 ? "was" : "were"} not an inference
          record{dropped === 1 ? "" : "s"} — this endpoint tops its
          list up with unrelated transactions once real attestations run out.
          They are excluded here.
        </p>
      )}
      <div className="feed">
        {real.length === 0 ? (
          <EmptyState
            icon={FileSignature}
            title="No inference claims on this host"
            description="Raw 0x16 computation claims appear after submission. Payment requires a separate successful mined 0x25 reward receipt."
          />
        ) : (
          real.slice(0, 12).map((a) => (
            <div key={a.txHash} className="feed-item">
              <div className="feed-item-icon">
                <FileSignature />
              </div>
              <div className="feed-item-body">
                <div className="feed-item-title">
                  {a.inputPreview || (
                    <span style={{ color: "var(--text-muted)" }}>
                      Inference activity
                    </span>
                  )}
                </div>
                <div className="feed-item-meta">
                  <span className="mono">{formatHash(a.txHash, 10)}</span>
                  {a.tokens != null && <span>{a.tokens} tok</span>}
                  {a.latencyMs != null && <span>{a.latencyMs}ms</span>}
                  {a.blockHeight != null ? (
                    <span>#{formatInt(a.blockHeight)}</span>
                  ) : (
                    <span>not in a block yet</span>
                  )}
                  <span>
                    {a.timestamp != null
                      ? formatRelativeTime(a.timestamp)
                      : "recent"}
                  </span>
                </div>
              </div>
              <div
                data-testid={`activity-status-${a.txHash.slice(0, 10)}`}
                style={{
                  color: a.paid ? "var(--success)" : "var(--text-muted)",
                  fontFamily: "var(--font-mono)",
                  fontSize: "var(--text-xs)",
                  fontWeight: 600,
                  whiteSpace: "nowrap",
                }}
              >
                {a.paid ? "COMPUTED + PAID" : "COMPUTED · NOT PAYMENT"}
              </div>
              <button
                className="btn btn-ghost btn-sm"
                onClick={() => lookupHash(a.txHash)}
                data-testid={`btn-lookup-${a.txHash.slice(0, 10)}`}
                aria-label="Look up on the pinned chain host"
              >
                <Search size={13} />
              </button>
            </div>
          ))
        )}
      </div>
    </Card>
  );
}
