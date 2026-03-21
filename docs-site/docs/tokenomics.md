---
title: Tokenomics
sidebar_position: 5
id: tokenomics
---

# Tokenomics

ARC Chain uses a **no-burn** economic model with a fixed token supply. All fees are distributed to network participants -- nothing is destroyed.

## $ARC Token

| Property | Value |
|----------|-------|
| **Total supply** | 1,030,000,000 ARC (1.03B) |
| **Burn mechanism** | None -- supply is fixed forever |
| **Token type** | Native L1 token (also bridgeable as ERC-20 on Ethereum) |

## Fee Distribution

100% of all transaction fees are distributed to network participants:

| Recipient | Share | Description |
|-----------|-------|-------------|
| **Proposers** | 40% | Block producers who run full execution |
| **Verifiers** | 25% | Validators who check state transitions |
| **Observers** | 15% | Lightweight nodes that attest to block validity |
| **Treasury** | 20% | Protocol development and ecosystem grants |

No tokens are ever burned. The fixed supply of 1.03B ARC is fully preserved.

### Zero-Fee Settlements

The `Settle` transaction type (0x02) carries zero base fee. AI agents settling with each other pay nothing in gas. This is a core design decision: agent-to-agent coordination should be free to encourage high-frequency autonomous interactions.

### TPS-Aware Fee Scaling

The base fee auto-adjusts at high TPS to keep fees sustainable. When the network is under load, fees increase proportionally to maintain quality of service.

## Staking Tiers

Validators stake ARC tokens to participate in consensus. Higher stakes unlock more responsibilities and higher rewards.

| Tier | Minimum Stake | APY | Unbonding Period | Role |
|------|---------------|-----|------------------|------|
| **Lite** | -- | 5% | 1 day | Token holder staking |
| **Spark** (Observer) | 50,000 ARC | 8% | 7 days | Monitor network, attest to blocks |
| **Arc** (Verifier) | 500,000 ARC | 15% | 14 days | Validate transactions, check state |
| **Core** (Proposer) | 5,000,000 ARC | 25% | 30 days | Produce blocks, run full execution, governance |

### Slashing Penalties

Validators face progressive slashing for misbehavior:

- **Equivocation** (double-proposing in the same round): automatic stake reduction
- **Liveness failure**: progressive penalties by tier (10% / 20% / 30%)
- Validators slashed below the Spark threshold (50,000 ARC) are ejected from the validator set

## Bootstrap Fund

| Property | Value |
|----------|-------|
| **Amount** | 40,000,000 ARC (40M) |
| **Duration** | 2 years |
| **Purpose** | Early validator subsidies |

The bootstrap fund ensures validators are profitable before fee volume ramps up. It subsidizes rewards during the initial growth phase so that running a node is economically viable from day one.

## Home Node Economics

ARC Chain is designed so regular people can participate from home hardware:

| Role | Hardware | Stake | Fee Share | Est. Cost |
|------|----------|-------|-----------|-----------|
| **Observer** | Raspberry Pi / laptop | 50,000 ARC | 15% of fees | ~$1/mo electricity |
| **Verifier** | Mac Mini / desktop | 500,000 ARC | 25% of fees | ~$3/mo electricity |
| **Proposer** | GPU server | 5,000,000 ARC | 40% of fees | Server-class hardware |

## Trading Tax Revenue

Bridge transactions between Ethereum and ARC Chain carry a small tax that flows to the treasury. This creates a sustainable revenue stream for protocol development independent of on-chain transaction volume.

## Governance

On-chain governance supports seven proposal types:

| Proposal Type | Description |
|---------------|-------------|
| ProtocolUpgrade | Software version upgrades |
| ParameterChange | Modify chain parameters |
| TreasurySpend | Allocate treasury funds |
| AddValidator | Whitelist a new validator |
| RemoveValidator | Eject a validator |
| FeatureFlagToggle | Enable/disable features |
| EmergencyAction | Emergency protocol changes |

Governance thresholds:
- **Quorum**: 40% of staked ARC must vote
- **Approval**: 60% of votes must be in favor
- **Emergency**: 75% approval required
- **Timelock**: 2-day delay + 3-day execution window after passing
