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
 * 1. **A projection needs a measured rate.** `attestationsPerDay` comes from
 *    the chain host or it is absent. This component never derives a rate
 *    itself: deriving one means assuming a block time, and block production is
 *    stalled on four of six seeds, so any assumed block time is wrong by an
 *    unknown factor. With no rate, it shows what one attestation pays and says
 *    a rate needs history. It does not extrapolate from zero.
 * 2. **The treasury is finite.** The remaining balance and the number of
 *    attestations it can still pay for are shown whenever known. A per-day
 *    figure with no stated ceiling implies an unlimited payout, and that is the
 *    dishonest version of this feature.
 * 3. **The bond is netted out, conservatively.** A bond is locked when an
 *    attestation is submitted. The host may report that it is refunded after a
 *    challenge period; the projection still nets it out and says so, because a
 *    refund this app cannot verify is not something to build a projection on.
 * 4. **This is a testnet treasury transfer, not revenue.** Stated on every
 *    variant. No fiat figure and no currency symbol appears anywhere.
 * 5. **Every number is attributable.** The host each figure came from is named
 *    inline, because the seeds are separate chains.
 *
 * Two reads are joined here: `/worker/earnings/{addr}` supplies the rate and
 * the reward, `/economics/rewards` supplies the bond and the treasury ceiling.
 * Either can 404 independently, and losing one must not take down the other.
 */

/** Horizon for the secondary figure. A week is the longest span a measured
 *  daily rate can be extended over without the stalls dominating it. */
const PROJECTION_DAYS = 7;

/** Decimals for the bond, which is ~1e-6 ARC and vanishes at 2dp. */
const BOND_DIGITS = 6;

interface Derived {
  /** Net ARC per settled attestation, bond removed when known. */
  netPerAttestation: number | null;
  perDay: number | null;
  perWeek: number | null;
}

/**
 * Combine the two reads into the figures shown.
 *
 * The bond arrives from `/economics/rewards`, NOT from the earnings endpoint —
 * worth stating because reading it from the wrong place silently yields null
 * and quietly stops netting anything out.
 */
function derive(
  p: EarningsProjection | undefined,
  econ: RewardEconomics | undefined,
): Derived {
  const reward = p?.rewardPerAttestation ?? null;
  if (reward === null) {
    return { netPerAttestation: null, perDay: null, perWeek: null };
  }
  const bond = econ?.bondPerAttestation ?? null;
  const net = bond !== null ? reward - bond : reward;
  const rate = p?.attestationsPerDay ?? null;
  // No rate → no projection. Deliberately not `rate ?? 0`.
  if (rate === null) {
    return { netPerAttestation: net, perDay: null, perWeek: null };
  }
  return {
    netPerAttestation: net,
    perDay: net * rate,
    perWeek: net * rate * PROJECTION_DAYS,
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
 * the three facts a reader needs — the rate and where it was measured, the
 * reward per attestation, and whether a bond was netted out.
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
    let rate = `${formatArc(p.attestationsPerDay, 1)} attestations/day, measured on ${host}`;
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
        ? `${formatArc(p.rewardPerAttestation)} ARC per settled attestation`
        : `${formatArc(p.rewardPerAttestation)} ARC per settled attestation (${rateSourceLabel(p)})`,
    );
  }

  // The bond, netted out on the conservative assumption that it stays locked —
  // even when the host says it comes back.
  const bond = econ?.bondPerAttestation ?? null;
  if (bond !== null) {
    if (compact) {
      out.push(`${formatArc(bond, BOND_DIGITS)} ARC bond netted out`);
    } else if (econ?.bondRefundedAfterChallengePeriod === true) {
      const period =
        econ.challengePeriodBlocks !== null
          ? `${formatInt(econ.challengePeriodBlocks)}-block `
          : "";
      out.push(
        `${formatArc(bond, BOND_DIGITS)} ARC bond netted out — this host reports it is refunded after the ${period}challenge period, so the figure above is the conservative one that treats the bond as still locked`,
      );
    } else {
      out.push(
        `${formatArc(bond, BOND_DIGITS)} ARC bond netted out; this host does not report whether it is ever released`,
      );
    }
  } else if (econ?.unavailable) {
    out.push(
      compact
        ? `bond unknown, so none is netted out`
        : `the bond could not be read from this host, so nothing is netted out of the reward above`,
    );
  } else {
    out.push(
      compact
        ? `no bond figure from this host, so none is netted out`
        : `no bond figure reported by this host, so nothing is netted out of the reward above`,
    );
  }

  return out;
}

/**
 * The finite-treasury line.
 *
 * Shows the remaining balance AND how many attestations it can still pay for.
 * That count comes from the host, not from arithmetic here — dividing a
 * network-wide pot by one node's rate would produce a "days remaining" figure
 * describing nothing, so this deliberately does no such division.
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
        network-wide
        {remaining !== null && (
          <> — about {formatInt(remaining)} more settled attestations</>
        )}
        , shared by every worker. Rewards stop when it is empty.
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
        network-wide
        {/* A COUNT the host computed — not currency, and not our arithmetic. */}
        {remaining !== null && (
          <>
            {" "}
            — enough for about <strong>{formatInt(remaining)}</strong> more
            settled attestations across the whole network
          </>
        )}
        . Rewards stop when it is empty. This is a network-wide pot shared by
        every worker, not a balance reserved for you.
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
      title="Rewards are moved from a testnet treasury. They are not income, and this app never shows a fiat value for them."
    >
      Testnet treasury transfer, not revenue
    </span>
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
        <CardHeader title="Projected rewards" />
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
        <CardHeader title="Projected rewards" action={<FundingLabel />} />
        <NotAvailable
          reason={projection.unavailable}
          testId="projection-unavailable"
        />
        <TreasuryLine econ={econ} compact={compact} />
      </Card>
    );
  }

  // ── No measured rate: show the rate card, project nothing ──────────────
  if (d.perDay === null) {
    return (
      <Card data-testid="projection-card">
        <CardHeader title="Projected rewards" action={<FundingLabel />} />
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
            <span className="unit">ARC per settled attestation</span>
          </div>
          <p
            style={{
              color: "var(--text-secondary)",
              fontSize: "var(--text-sm)",
              lineHeight: 1.7,
              margin: 0,
            }}
          >
            {projection.rateUnavailableReason ??
              "This host reports no observed attestation rate."}{" "}
            <strong>
              No per-day figure is shown: a rate has to be measured, and there
              is nothing yet to measure.
            </strong>{" "}
            Nothing here is extrapolated from zero.
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
      </Card>
    );
  }

  // ── Full projection: every input is real ───────────────────────────────
  return (
    <Card data-testid="projection-card" featured={!compact}>
      <CardHeader title="Projected rewards" action={<FundingLabel />} />
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
    </Card>
  );
}

/** Icon re-export so callers can label a section without importing lucide. */
export const ProjectionIcon = TrendingUp;
