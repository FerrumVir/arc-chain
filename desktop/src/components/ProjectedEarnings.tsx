import { useQuery } from "@tanstack/react-query";
import { TrendingUp } from "lucide-react";
import { Card, CardHeader } from "./Card";
import { NotAvailable } from "./NotAvailable";
import { api } from "../lib/tauri";
import { formatArc, formatInt } from "../lib/format";
import { hostLabel, hostLabelVerbose } from "../lib/hosts";
import type { EarningsProjection, RewardEconomics } from "../lib/types";

/**
 * Projected rewards — the only forward-looking number in the app.
 *
 * The rules it follows, in order of how badly breaking each one would hurt:
 *
 * 1. **The forecast is backend-authoritative.** `projectedDailyArc` is either
 *    supplied after the coordinator applies readiness, treasury, and
 *    consensus-budget policy, or it is absent with a reason. This component
 *    never reconstructs it from receipt rate or reward amount.
 * 2. **The treasury is finite.** The remaining balance and the number of
 *    reward receipts it can still fund are shown whenever known. A per-day
 *    figure with no stated ceiling implies an unlimited payout, and that is the
 *    dishonest version of this feature.
 * 3. **Settlement must be active.** The selected coordinator must explicitly
 *    report that community reward v1 is enabled. Jobs can execute while reward
 *    settlement is paused, so a protocol constant alone is not permission to
 *    display projected earnings.
 * 4. **Worker bond terms are exact.** Community certificates currently carry
 *    a zero bond. The unrelated local-attestation bond is never subtracted
 *    from a worker reward.
 * 5. **This is a testnet treasury transfer, not revenue.** Stated on every
 *    variant. No fiat figure and no currency symbol appears anywhere.
 * 6. **Every number is attributable.** The host each figure came from is named
 *    inline, because the seeds are separate chains.
 *
 * Two reads are joined here: `/worker/earnings/{addr}` supplies the rate and
 * the reward and rollout gate, `/economics/rewards` supplies worker certificate
 * terms and the treasury ceiling.
 * Either can 404 independently, and losing one must not take down the other.
 */

/** Horizon for the secondary figure. A week is the longest span a measured
 *  daily rate can be extended over without the stalls dominating it. */
const PROJECTION_DAYS = 7;

/** Precision if a future community-certificate contract reports a small bond. */
const BOND_DIGITS = 6;

interface Derived {
  /** Explicit reward amount for one mined receipt; never used to synthesize a forecast. */
  netPerAttestation: number | null;
  perDay: number | null;
  perWeek: number | null;
}

/**
 * Combine the two reads into the figures shown.
 *
 * Worker certificate terms arrive from `/economics/rewards`, not from the
 * earnings endpoint. They must never be confused with the coordinator's local
 * attestation bond.
 */
function derive(
  p: EarningsProjection | undefined,
  _econ: RewardEconomics | undefined,
): Derived {
  const reward = p?.rewardPerAttestation ?? null;
  // The coordinator already applies readiness, budget, treasury and observed
  // receipt policy. Reconstructing `(reward - bond) * observedRate` here can
  // display a forecast the coordinator explicitly withheld. A worker
  // certificate bond is also not a deduction from this authoritative field.
  const perDay = p?.projectedDailyArc ?? null;
  return {
    netPerAttestation: reward,
    perDay,
    perWeek: perDay === null ? null : perDay * PROJECTION_DAYS,
  };
}

/** The label that says where a rate came from. Never "assumed". */
function rateSourceLabel(p: EarningsProjection): string {
  switch (p.rewardRateSource) {
    case "chain":
      return "rate reported by this host";
    case "constant":
      return "flat testnet rate, a named constant in this build";
    default:
      return "source unknown";
  }
}

/**
 * The assumptions line, built as clauses so a missing input drops its clause
 * instead of rendering "over null blocks".
 *
 * `compact` shortens each clause for the Dashboard tile without dropping any of
 * the facts a reader needs — the successful mined reward-receipt rate and
 * where it was measured, the 0x25 reward amount, and certificate terms.
 */
function assumptionClauses(
  p: EarningsProjection,
  econ: RewardEconomics | undefined,
  compact = false,
): string[] {
  const out: string[] = [];
  const host = compact
    ? hostLabel(p.sourceHost)
    : hostLabelVerbose(p.sourceHost);

  if (p.attestationsPerDay !== null) {
    let rate = `${formatArc(p.attestationsPerDay, 1)} mined reward receipts/day, measured on ${host}`;
    if (p.observedOverBlocks !== null) {
      rate += ` over ${formatInt(p.observedOverBlocks)} blocks`;
    }
    if (!compact && p.firstAttestationBlock !== null) {
      rate += ` since block #${formatInt(p.firstAttestationBlock)}`;
    }
    out.push(rate);
  }

  if (p.rewardPerAttestation !== null) {
    out.push(
      compact
        ? `${formatArc(p.rewardPerAttestation)} ARC per successful mined 0x25 receipt`
        : `${formatArc(p.rewardPerAttestation)} ARC per successful mined 0x25 community-reward receipt (${rateSourceLabel(p)})`,
    );
  }

  out.push(
    compact
      ? "reward protocol and approval collection reported ready"
      : "the selected coordinator reports both reward-protocol activation and validator-approval collection ready",
  );

  // Community reward certificate bond only. A zero here is meaningful: the
  // worker signs a certificate but posts no collateral.
  const bond = econ?.bondPerAttestation ?? null;
  if (bond !== null) {
    if (bond === 0) {
      out.push(
        compact
          ? "no worker bond required"
          : "no worker bond is required for a verified community reward certificate",
      );
    } else if (compact) {
      out.push(`${formatArc(bond, BOND_DIGITS)} ARC worker bond (not deducted here)`);
    } else {
      out.push(
        `${formatArc(bond, BOND_DIGITS)} ARC worker certificate bond reported separately; the backend projection is not recomputed or reduced here`,
      );
    }
  } else if (econ?.unavailable) {
    out.push(
      compact
        ? `worker bond terms unavailable; no deduction assumed`
        : `worker certificate bond terms could not be read from this host, so the projection uses the gross reward and assumes no deduction`,
    );
  } else {
    out.push(
      compact
        ? `worker bond terms unreported; no deduction assumed`
        : `no worker certificate bond figure was reported by this host, so the projection uses the gross reward and assumes no deduction`,
    );
  }

  return out;
}

/**
 * The finite-treasury line.
 *
 * Shows the remaining balance AND how many reward receipts it can still fund.
 * That count comes from the selected host, not from arithmetic here. Dividing
 * a host-scoped treasury by one worker's rate would produce a "days remaining"
 * figure describing nothing, so this deliberately does no such division.
 */
function TreasuryLine({
  econ,
  compact = false,
}: {
  econ: RewardEconomics | undefined;
  compact?: boolean;
}) {
  if (!econ) return null;

  const unavailableStyle = {
    margin: "var(--space-3) 0 0",
    fontSize: "var(--text-xs)",
    color: "var(--text-muted)",
    lineHeight: 1.6,
  } as const;

  if (econ.unavailable) {
    return (
      <p style={unavailableStyle} data-testid="treasury-unavailable">
        Remaining treasury unknown: {econ.unavailable}
      </p>
    );
  }

  const balance = econ.treasuryBalanceArc;
  const remaining = econ.attestationsRemaining;

  // The endpoint answered but carried neither figure — give the reason it
  // supplied rather than dropping the ceiling silently.
  if (balance === null && remaining === null) {
    const why =
      econ.treasuryBalanceUnavailableReason ??
      econ.attestationsRemainingUnavailableReason;
    if (!why) return null;
    return (
      <p style={unavailableStyle} data-testid="treasury-unavailable">
        Remaining treasury unknown: {why}
      </p>
    );
  }

  if (compact) {
    return (
      <p style={unavailableStyle} data-testid="treasury-remaining">
        Finite reward treasury:{" "}
        {balance !== null && <>{formatArc(balance)} ARC remains </>}
        on this chain host
        {remaining !== null && (
          <> — about {formatInt(remaining)} more successful 0x25 receipts</>
        )}
        . Do not aggregate it across the currently divergent public seeds.
      </p>
    );
  }

  return (
    <div style={{ marginTop: "var(--space-4)" }} data-testid="treasury-remaining">
      <div
        style={{
          fontSize: "var(--text-sm)",
          color: "var(--text-secondary)",
          lineHeight: 1.6,
        }}
      >
        The reward treasury is finite:{" "}
        {balance !== null && (
          <>
            <strong>{formatArc(balance)} ARC</strong> remains{" "}
          </>
        )}
        on this selected chain host
        {/* A COUNT the host computed — not currency, and not our arithmetic. */}
        {remaining !== null && (
          <>
            {" "}
            — enough for about <strong>{formatInt(remaining)}</strong> more
            successful mined <code>0x25</code> reward receipts on this host
          </>
        )}
        . Rewards stop when it is empty. This is a finite host-scoped treasury,
        not a balance reserved for you; do not combine it with another seed
        while the public fleet is divergent.
      </div>
      {econ.fundingDetail && (
        <div
          style={{
            marginTop: "var(--space-2)",
            fontSize: "var(--text-xs)",
            color: "var(--text-muted)",
            lineHeight: 1.6,
          }}
          data-testid="treasury-funding-detail"
        >
          {econ.fundingDetail}
        </div>
      )}
    </div>
  );
}

/** Shown on every variant. The single most important sentence here. */
function FundingLabel() {
  return (
    <span
      className="status-pill info"
      data-testid="projection-funding-label"
      style={{ fontSize: "var(--text-xs)", fontWeight: 500 }}
      title="Only successful mined 0x25 receipts move promotional testnet treasury ARC. Validator recomputation is not proof of customer demand; raw 0x16 attestations are not payment."
    >
      Promotional testnet subsidy, not demand or revenue
    </span>
  );
}

function BudgetLine({
  projection,
  compact = false,
}: {
  projection: EarningsProjection;
  compact?: boolean;
}) {
  const remaining = projection.rewardsRemainingThisEpoch;
  const worker = projection.workerRewardsRemainingThisEpoch;
  const coordinator = projection.coordinatorRewardsRemainingThisEpoch;
  if (remaining === null && worker === null && coordinator === null) return null;
  const policy = projection.rewardPolicyHash;
  return (
    <p
      data-testid="projection-reward-budget"
      style={{
        margin: "var(--space-3) 0 0",
        fontSize: "var(--text-xs)",
        color: "var(--text-muted)",
        lineHeight: 1.6,
      }}
    >
      Promotional cap
      {projection.rewardBudgetEpoch !== null
        ? `, epoch ${formatInt(projection.rewardBudgetEpoch)}`
        : ""}
      : {remaining === null ? "—" : formatInt(remaining)} global, {worker === null ? "—" : formatInt(worker)} for this worker, and {coordinator === null ? "—" : formatInt(coordinator)} for this coordinator remain.
      {!compact && policy
        ? ` Policy ${policy.slice(0, 12)}… is consensus-sealed.`
        : ""}
    </p>
  );
}

/** The host's own caveat about how it derived the rate, shown verbatim. */
function RateCaveat({ p }: { p: EarningsProjection }) {
  if (!p.rateCaveat) return null;
  return (
    <p
      style={{
        margin: "var(--space-2) 0 0",
        fontSize: "var(--text-xs)",
        color: "var(--text-muted)",
        lineHeight: 1.6,
      }}
      data-testid="projection-rate-caveat"
    >
      {p.rateCaveat}
    </p>
  );
}

export function ProjectedEarnings({
  variant = "full",
}: {
  variant?: "full" | "tile";
}) {
  const { data: projection } = useQuery({
    queryKey: ["earnings-projection"],
    queryFn: api.fetchEarningsProjection,
    refetchInterval: 15_000,
  });
  const { data: econ } = useQuery({
    queryKey: ["reward-economics"],
    queryFn: api.fetchRewardEconomics,
    refetchInterval: 60_000,
  });

  const d = derive(projection, econ);
  const compact = variant === "tile";

  // ── Still reading ──────────────────────────────────────────────────────
  if (!projection) {
    return (
      <Card data-testid="projection-loading">
        <CardHeader title="Observed-rate reward projection" />
        <p
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-sm)",
            margin: 0,
          }}
        >
          Reading the chain host…
        </p>
      </Card>
    );
  }

  // ── The earnings read failed ───────────────────────────────────────────
  if (projection.unavailable) {
    return (
      <Card data-testid="projection-card">
        <CardHeader title="Observed-rate reward projection" action={<FundingLabel />} />
        <NotAvailable
          reason={projection.unavailable}
          testId="projection-unavailable"
        />
        <TreasuryLine econ={econ} compact={compact} />
        <BudgetLine projection={projection} compact={compact} />
      </Card>
    );
  }

  // Jobs and rewards have separate rollout gates. Never turn a configured
  // protocol amount into projected earnings unless this coordinator confirms
  // that it can actually settle community reward transactions.
  if (projection.communityRewardsEnabled !== true) {
    const inactive = projection.communityRewardsEnabled === false;
    return (
      <Card data-testid="projection-card">
        <CardHeader title="Observed-rate reward projection" action={<FundingLabel />} />
        <NotAvailable
          reason={
            inactive
              ? `Community reward settlement is inactive on ${hostLabelVerbose(projection.sourceHost)}. Work may still run, but this coordinator reports that protocol activation and/or validator-approval collection is not ready, so it cannot create a payable 0x25 transaction.`
              : `This host did not confirm both community-reward protocol activation and validator-approval collection. No future reward figure is shown.`
          }
          testId="projection-rollout-inactive"
        />
        <TreasuryLine econ={econ} compact={compact} />
        <BudgetLine projection={projection} compact={compact} />
      </Card>
    );
  }

  // ── No measured rate: show the rate card, project nothing ──────────────
  if (d.perDay === null) {
    return (
      <Card data-testid="projection-card">
        <CardHeader title="Observed-rate reward projection" action={<FundingLabel />} />
        <div data-testid="projection-no-rate">
          <div
            className="big-number"
            style={{ marginBottom: "var(--space-2)" }}
          >
            <span data-testid="projection-per-attestation">
              {projection.rewardPerAttestation !== null
                ? formatArc(projection.rewardPerAttestation)
                : "—"}
            </span>
            <span className="unit">ARC per successful mined 0x25 receipt</span>
          </div>
          <p
            style={{
              color: "var(--text-secondary)",
              fontSize: "var(--text-sm)",
              lineHeight: 1.7,
              margin: 0,
            }}
          >
            {projection.projectedDailyUnavailableReason ??
              "This host withheld an authoritative daily reward projection."}{" "}
            <strong>
              No per-day figure is shown unless the coordinator explicitly
              supplies one after applying readiness and reward-budget policy.
            </strong>{" "}
            Nothing here is reconstructed from receipt count, reward amount,
            or bond terms.
          </p>
          <p
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-xs)",
              lineHeight: 1.6,
              marginTop: "var(--space-3)",
              marginBottom: 0,
            }}
            data-testid="projection-assumptions"
          >
            {assumptionClauses(projection, econ, compact).join(" · ")}
          </p>
        </div>
        <TreasuryLine econ={econ} compact={compact} />
        <BudgetLine projection={projection} compact={compact} />
      </Card>
    );
  }

  // ── Full projection: every input is real ───────────────────────────────
  return (
    <Card data-testid="projection-card" featured={!compact}>
      <CardHeader title="Observed-rate reward projection" action={<FundingLabel />} />
      <div data-testid="projection-figures">
        <div
          style={{
            display: compact ? "block" : "grid",
            gridTemplateColumns: compact ? undefined : "1fr 1fr",
            gap: "var(--space-6)",
          }}
        >
          <div>
            <div className="stat-label">Per day</div>
            <div
              className="big-number gradient"
              style={{ marginTop: "var(--space-2)" }}
            >
              <span data-testid="projection-per-day">
                {formatArc(d.perDay)}
              </span>
              <span className="unit">ARC</span>
            </div>
          </div>
          {!compact && (
            <div>
              <div className="stat-label">Per {PROJECTION_DAYS} days</div>
              <div
                className="big-number"
                style={{ marginTop: "var(--space-2)" }}
              >
                <span data-testid="projection-per-week">
                  {formatArc(d.perWeek!)}
                </span>
                <span className="unit">ARC</span>
              </div>
            </div>
          )}
        </div>

        <p
          style={{
            marginTop: "var(--space-4)",
            marginBottom: 0,
            fontSize: "var(--text-xs)",
            color: "var(--text-muted)",
            lineHeight: 1.7,
          }}
          data-testid="projection-assumptions"
        >
          <strong style={{ color: "var(--text-secondary)" }}>Assumes:</strong>{" "}
          {assumptionClauses(projection, econ, compact).join(" · ")}.
        </p>
        {!compact && <RateCaveat p={projection} />}
      </div>
      <TreasuryLine econ={econ} compact={compact} />
      <BudgetLine projection={projection} compact={compact} />
    </Card>
  );
}

/** Icon re-export so callers can label a section without importing lucide. */
export const ProjectionIcon = TrendingUp;
