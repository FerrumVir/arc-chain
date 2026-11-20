import { useQuery } from "@tanstack/react-query";
import { TrendingUp } from "lucide-react";
import { Card, CardHeader } from "./Card";
import { SkeletonLines } from "./Skeleton";
import { NotAvailable } from "./NotAvailable";
import { api } from "../lib/tauri";
import { formatArc, formatInt } from "../lib/format";
import { hostLabel, hostLabelVerbose } from "../lib/hosts";
import type { EarningsProjection, RewardEconomics } from "../lib/types";

const PROJECTION_DAYS = 7;

const BOND_DIGITS = 6;

interface Derived {
  netPerAttestation: number | null;
  perDay: number | null;
  perWeek: number | null;
}

function projectionEvidenceAttributes(
  projection: EarningsProjection,
  econ: RewardEconomics | undefined,
  state: "numeric" | "unavailable" | "rollout_inactive" | "no_rate",
  unavailableReason: string,
) {
  return {
    "data-projection-state": state,
    "data-source-host": projection.sourceHost,
    "data-economics-source-host": econ?.sourceHost ?? "",
    "data-projected-daily-arc": projection.projectedDailyArc ?? "",
    "data-unavailable-reason": unavailableReason,
    "data-reward-per-attestation": projection.rewardPerAttestation ?? "",
    "data-reward-rate-source": projection.rewardRateSource,
    "data-reward-policy-hash": projection.rewardPolicyHash ?? "",
    "data-community-rewards-enabled":
      projection.communityRewardsEnabled == null
        ? ""
        : String(projection.communityRewardsEnabled),
    "data-issuance-ready-for-worker":
      projection.issuanceReadyForWorker == null
        ? ""
        : String(projection.issuanceReadyForWorker),
    "data-reward-program": projection.rewardProgram ?? "",
  };
}

function derive(
  p: EarningsProjection | undefined,
  _econ: RewardEconomics | undefined,
): Derived {
  const reward = p?.rewardPerAttestation ?? null;
  const perDay = p?.projectedDailyArc ?? null;
  return {
    netPerAttestation: reward,
    perDay,
    perWeek: perDay === null ? null : perDay * PROJECTION_DAYS,
  };
}

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
        . Do not aggregate host-scoped evidence across sources without verified
        canonical agreement.
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
        {remaining !== null && (
          <>
            {" "}
            — enough for about <strong>{formatInt(remaining)}</strong> more
            successful mined <code>0x25</code> reward receipts on this host
          </>
        )}
        . Rewards stop when it is empty. This is a finite host-scoped treasury,
        not a balance reserved for you; do not aggregate it with another source
        unless canonical agreement is independently verified.
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

  if (!projection) {
    return (
      <Card data-testid="projection-loading" aria-busy="true">
        <CardHeader title="Observed-rate reward projection" />
        <p
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-sm)",
            margin: "0 0 var(--space-4)",
          }}
        >
          Reading the chain host…
        </p>
        <SkeletonLines lines={compact ? 2 : 4} gap={10} />
      </Card>
    );
  }

  if (projection.unavailable) {
    return (
      <Card
        data-testid="projection-card"
        {...projectionEvidenceAttributes(
          projection,
          econ,
          "unavailable",
          projection.unavailable,
        )}
      >
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

  if (projection.communityRewardsEnabled !== true) {
    const inactive = projection.communityRewardsEnabled === false;
    const rolloutReason = inactive
      ? `Community reward settlement is inactive on ${hostLabelVerbose(projection.sourceHost)}. Work may still run, but this coordinator reports that protocol activation and/or validator-approval collection is not ready, so it cannot create a payable 0x25 transaction.`
      : "This host did not confirm both community-reward protocol activation and validator-approval collection. No future reward figure is shown.";
    return (
      <Card
        data-testid="projection-card"
        {...projectionEvidenceAttributes(
          projection,
          econ,
          "rollout_inactive",
          rolloutReason,
        )}
      >
        <CardHeader title="Observed-rate reward projection" action={<FundingLabel />} />
        <NotAvailable
          reason={rolloutReason}
          testId="projection-rollout-inactive"
        />
        <TreasuryLine econ={econ} compact={compact} />
        <BudgetLine projection={projection} compact={compact} />
      </Card>
    );
  }

  if (d.perDay === null) {
    const noRateReason =
      projection.projectedDailyUnavailableReason ??
      "This host withheld an authoritative daily reward projection.";
    return (
      <Card
        data-testid="projection-card"
        {...projectionEvidenceAttributes(
          projection,
          econ,
          "no_rate",
          noRateReason,
        )}
      >
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
            {noRateReason}{" "}
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

  return (
    <Card
      data-testid="projection-card"
      featured={!compact}
      {...projectionEvidenceAttributes(projection, econ, "numeric", "")}
    >
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

export const ProjectionIcon = TrendingUp;
