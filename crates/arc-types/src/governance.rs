// Add to lib.rs: pub mod governance;

use serde::{Deserialize, Serialize};
use std::fmt;

// ─── Governance errors ──────────────────────────────────────────────────────

/// Errors that can occur during governance operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceError {
    ProposalNotFound,
    VotingNotActive,
    AlreadyVoted,
    InsufficientStake,
    NotProposer,
    ProposalNotPassed,
    ExecutionWindowExpired,
    TooManyActiveProposals,
    CooldownNotExpired,
    VotingEnded,
}

impl fmt::Display for GovernanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProposalNotFound => write!(f, "proposal not found"),
            Self::VotingNotActive => write!(f, "voting is not currently active"),
            Self::AlreadyVoted => write!(f, "address has already voted on this proposal"),
            Self::InsufficientStake => write!(f, "insufficient stake to perform this action"),
            Self::NotProposer => write!(f, "caller is not the proposal author"),
            Self::ProposalNotPassed => write!(f, "proposal has not passed"),
            Self::ExecutionWindowExpired => write!(f, "execution window has expired"),
            Self::TooManyActiveProposals => write!(f, "maximum active proposals reached"),
            Self::CooldownNotExpired => write!(f, "proposer cooldown has not expired"),
            Self::VotingEnded => write!(f, "voting period has ended"),
        }
    }
}

impl std::error::Error for GovernanceError {}

// ─── Vote choice ────────────────────────────────────────────────────────────

/// The three-way choice a voter can make on a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoteChoice {
    For,
    Against,
    Abstain,
}

// ─── Vote ───────────────────────────────────────────────────────────────────

/// A single vote cast on a proposal, weighted by the voter's stake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    /// Voter address (32-byte public key hash).
    pub voter: [u8; 32],
    /// Stake-weighted voting power at time of vote.
    pub stake_weight: u128,
    /// The voter's choice.
    pub choice: VoteChoice,
    /// Block height when the vote was cast.
    pub cast_at: u64,
    /// Optional reason or rationale for the vote.
    pub reason: Option<String>,
}

// ─── Proposal type ──────────────────────────────────────────────────────────

/// The category of a governance proposal, carrying type-specific data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProposalType {
    /// Upgrade the protocol to a new version.
    ProtocolUpgrade {
        version: String,
        features: Vec<String>,
    },
    /// Change a protocol parameter.
    ParameterChange {
        parameter: String,
        old_value: String,
        new_value: String,
    },
    /// Spend funds from the on-chain treasury.
    TreasurySpend {
        recipient: [u8; 32],
        amount: u64,
        reason: String,
    },
    /// Emergency protocol action (elevated threshold).
    EmergencyAction { action: String },
    /// Add a new validator to the active set.
    AddValidator {
        address: [u8; 32],
        initial_stake: u64,
    },
    /// Remove a validator from the active set.
    RemoveValidator {
        address: [u8; 32],
        reason: String,
    },
    /// Toggle a feature flag on or off.
    FeatureFlagToggle { feature: String, enabled: bool },
    /// Arbitrary governance payload.
    Custom { data: Vec<u8> },
}

// ─── Proposal status ────────────────────────────────────────────────────────

/// Lifecycle status of a governance proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    /// Proposal created but voting has not started.
    Draft,
    /// Voting is open.
    Active,
    /// Quorum met and approval threshold exceeded.
    Passed,
    /// Quorum not met or approval threshold not reached.
    Failed,
    /// Successfully executed on-chain.
    Executed,
    /// Cancelled by the proposer before execution.
    Cancelled,
    /// Execution window expired without execution.
    Expired,
    /// Vetoed by the security council.
    Vetoed,
}

// ─── Proposal ───────────────────────────────────────────────────────────────

/// A governance proposal with full voting state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// Unique proposal identifier (monotonically increasing).
    pub id: u64,
    /// Address of the proposer (must be Core tier staker).
    pub proposer: [u8; 32],
    /// Short human-readable title.
    pub title: String,
    /// Detailed description of the proposal.
    pub description: String,
    /// The type-specific payload for this proposal.
    pub proposal_type: ProposalType,
    /// Block height when the proposal was created.
    pub created_at: u64,
    /// Block height when voting opens.
    pub voting_starts: u64,
    /// Block height when voting closes.
    pub voting_ends: u64,
    /// Number of blocks after approval before execution is allowed.
    pub execution_delay: u64,
    /// Current lifecycle status.
    pub status: ProposalStatus,
    /// Total stake-weighted votes in favour.
    pub votes_for: u128,
    /// Total stake-weighted votes against.
    pub votes_against: u128,
    /// Total stake-weighted abstentions.
    pub votes_abstain: u128,
    /// Minimum total votes required for the result to be valid.
    pub quorum_required: u128,
    /// Approval threshold in basis points (6000 = 60%).
    pub approval_threshold_bps: u16,
    /// All individual votes cast on this proposal.
    pub voters: Vec<Vote>,
}

// ─── Governance config ──────────────────────────────────────────────────────

/// Protocol-level governance parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceConfig {
    /// Minimum stake to create a proposal (must be Core tier: 50M ARC).
    pub min_proposal_stake: u64,
    /// Duration of the voting period in blocks (~7 days at 400 ms/block).
    pub voting_period_blocks: u64,
    /// Timelock delay after approval before execution (~2 days).
    pub execution_delay_blocks: u64,
    /// Quorum as a percentage of total staked supply (basis points; 4000 = 40%).
    pub quorum_percentage_bps: u16,
    /// Standard approval threshold (basis points; 6000 = 60%).
    pub approval_threshold_bps: u16,
    /// Emergency proposal approval threshold (basis points; 7500 = 75%).
    pub emergency_threshold_bps: u16,
    /// Maximum number of proposals in Active status at once.
    pub max_active_proposals: u32,
    /// Minimum blocks between proposals from the same address.
    pub proposal_cooldown_blocks: u64,
}

impl GovernanceConfig {
    /// Sensible defaults for ARC Chain mainnet.
    ///
    /// Block time: ~400 ms
    /// Voting period: ~7 days (1_512_000 blocks)
    /// Execution delay: ~2 days (432_000 blocks)
    /// Execution window: ~3 days after delay
    pub fn default_config() -> Self {
        Self {
            min_proposal_stake: 50_000_000_000_000_000, // 50M ARC (Core tier)
            voting_period_blocks: 1_512_000,             // ~7 days
            execution_delay_blocks: 432_000,             // ~2 days
            quorum_percentage_bps: 4000,                 // 40%
            approval_threshold_bps: 6000,                // 60%
            emergency_threshold_bps: 7500,               // 75%
            max_active_proposals: 10,
            proposal_cooldown_blocks: 216_000,           // ~1 day
        }
    }

    /// Calculate the absolute quorum amount from a percentage of total staked supply.
    pub fn quorum_amount(&self, total_staked: u128) -> u128 {
        total_staked * self.quorum_percentage_bps as u128 / 10_000
    }
}

// ─── Governance state ───────────────────────────────────────────────────────

/// Aggregate on-chain governance state.
#[derive(Debug, Clone, Default)]
pub struct GovernanceState {
    /// All proposals (active, passed, failed, etc.).
    pub proposals: Vec<Proposal>,
    /// The next proposal ID to assign.
    pub next_proposal_id: u64,
    /// Total ARC currently staked across all validators and delegators.
    pub total_staked_supply: u128,
    /// Number of proposals currently in Active status.
    pub active_proposal_count: u32,
    /// Lifetime count of proposals created.
    pub total_proposals_created: u64,
    /// Lifetime count of proposals that passed.
    pub total_proposals_passed: u64,
    /// Lifetime count of proposals that failed.
    pub total_proposals_failed: u64,
}

/// Maximum blocks after the execution delay during which a passed proposal
/// can still be executed. After this window the proposal expires.
const EXECUTION_WINDOW_BLOCKS: u64 = 648_000; // ~3 days

impl GovernanceState {
    /// Create a fresh governance state with no proposals.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new governance proposal.
    ///
    /// The proposer must meet the minimum stake requirement. The proposal
    /// enters `Active` status immediately with voting starting at the current
    /// block height.
    pub fn create_proposal(
        &mut self,
        proposer: [u8; 32],
        title: String,
        description: String,
        proposal_type: ProposalType,
        current_height: u64,
        config: &GovernanceConfig,
    ) -> Result<u64, GovernanceError> {
        // Enforce max active proposals.
        if self.active_proposal_count >= config.max_active_proposals {
            return Err(GovernanceError::TooManyActiveProposals);
        }

        // Enforce per-proposer cooldown: find the most recent proposal by this
        // proposer and ensure enough blocks have elapsed.
        let last_proposal_height = self
            .proposals
            .iter()
            .rev()
            .filter(|p| p.proposer == proposer)
            .map(|p| p.created_at)
            .next();

        if let Some(last_height) = last_proposal_height {
            if current_height < last_height + config.proposal_cooldown_blocks {
                return Err(GovernanceError::CooldownNotExpired);
            }
        }

        let voting_starts = current_height;
        let voting_ends = current_height + config.voting_period_blocks;

        let threshold_bps = match &proposal_type {
            ProposalType::EmergencyAction { .. } => config.emergency_threshold_bps,
            _ => config.approval_threshold_bps,
        };

        let quorum_required = config.quorum_amount(self.total_staked_supply);

        let id = self.next_proposal_id;
        let proposal = Proposal {
            id,
            proposer,
            title,
            description,
            proposal_type,
            created_at: current_height,
            voting_starts,
            voting_ends,
            execution_delay: config.execution_delay_blocks,
            status: ProposalStatus::Active,
            votes_for: 0,
            votes_against: 0,
            votes_abstain: 0,
            quorum_required,
            approval_threshold_bps: threshold_bps,
            voters: Vec::new(),
        };

        self.proposals.push(proposal);
        self.next_proposal_id += 1;
        self.active_proposal_count += 1;
        self.total_proposals_created += 1;

        Ok(id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// The vote must be cast while the proposal is Active and within the
    /// voting window. Each address may only vote once per proposal.
    pub fn cast_vote(
        &mut self,
        proposal_id: u64,
        vote: Vote,
        current_height: u64,
    ) -> Result<(), GovernanceError> {
        let proposal = self
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::VotingNotActive);
        }

        if current_height > proposal.voting_ends {
            return Err(GovernanceError::VotingEnded);
        }

        if current_height < proposal.voting_starts {
            return Err(GovernanceError::VotingNotActive);
        }

        // Check for duplicate votes.
        if proposal.voters.iter().any(|v| v.voter == vote.voter) {
            return Err(GovernanceError::AlreadyVoted);
        }

        // Accumulate stake-weighted totals.
        match vote.choice {
            VoteChoice::For => proposal.votes_for += vote.stake_weight,
            VoteChoice::Against => proposal.votes_against += vote.stake_weight,
            VoteChoice::Abstain => proposal.votes_abstain += vote.stake_weight,
        }

        proposal.voters.push(vote);
        Ok(())
    }

    /// Finalize a proposal after the voting period ends.
    ///
    /// Checks quorum and approval threshold. Transitions the proposal to
    /// `Passed` or `Failed`.
    pub fn finalize_proposal(
        &mut self,
        proposal_id: u64,
        current_height: u64,
        total_staked: u128,
        config: &GovernanceConfig,
    ) -> Result<ProposalStatus, GovernanceError> {
        let proposal = self
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::VotingNotActive);
        }

        // Voting must have ended.
        if current_height < proposal.voting_ends {
            return Err(GovernanceError::VotingNotActive);
        }

        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum = config.quorum_amount(total_staked);

        // Check quorum (total participation).
        if total_votes < quorum {
            proposal.status = ProposalStatus::Failed;
            self.active_proposal_count = self.active_proposal_count.saturating_sub(1);
            self.total_proposals_failed += 1;
            return Ok(ProposalStatus::Failed);
        }

        // Check approval threshold (For votes as percentage of For + Against).
        let decisive_votes = proposal.votes_for + proposal.votes_against;
        let passes = if decisive_votes == 0 {
            false
        } else {
            // For / (For + Against) >= threshold / 10_000
            // Rearranged to avoid floats: For * 10_000 >= threshold * (For + Against)
            proposal.votes_for * 10_000
                >= proposal.approval_threshold_bps as u128 * decisive_votes
        };

        if passes {
            proposal.status = ProposalStatus::Passed;
            self.active_proposal_count = self.active_proposal_count.saturating_sub(1);
            self.total_proposals_passed += 1;
            Ok(ProposalStatus::Passed)
        } else {
            proposal.status = ProposalStatus::Failed;
            self.active_proposal_count = self.active_proposal_count.saturating_sub(1);
            self.total_proposals_failed += 1;
            Ok(ProposalStatus::Failed)
        }
    }

    /// Look up a proposal by ID.
    pub fn get_proposal(&self, id: u64) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.id == id)
    }

    /// Return all proposals that are currently in Active status.
    pub fn active_proposals(&self) -> Vec<&Proposal> {
        self.proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Active)
            .collect()
    }

    /// Check whether a passed proposal is ready to execute (timelock elapsed,
    /// execution window not expired).
    pub fn can_execute(&self, proposal_id: u64, current_height: u64) -> bool {
        let Some(proposal) = self.get_proposal(proposal_id) else {
            return false;
        };
        if proposal.status != ProposalStatus::Passed {
            return false;
        }
        let earliest = proposal.voting_ends + proposal.execution_delay;
        let latest = earliest + EXECUTION_WINDOW_BLOCKS;
        current_height >= earliest && current_height <= latest
    }

    /// Execute a passed proposal after the timelock delay.
    ///
    /// Returns the proposal on success. Actual side-effects (parameter
    /// changes, treasury transfers, etc.) are handled by the caller.
    pub fn execute_proposal(
        &mut self,
        proposal_id: u64,
        current_height: u64,
    ) -> Result<&Proposal, GovernanceError> {
        // First validate without borrowing mutably.
        let (status, voting_ends, execution_delay) = {
            let proposal = self
                .proposals
                .iter()
                .find(|p| p.id == proposal_id)
                .ok_or(GovernanceError::ProposalNotFound)?;
            (proposal.status, proposal.voting_ends, proposal.execution_delay)
        };

        if status != ProposalStatus::Passed {
            return Err(GovernanceError::ProposalNotPassed);
        }

        let earliest = voting_ends + execution_delay;
        let latest = earliest + EXECUTION_WINDOW_BLOCKS;

        if current_height < earliest {
            return Err(GovernanceError::ProposalNotPassed);
        }
        if current_height > latest {
            return Err(GovernanceError::ExecutionWindowExpired);
        }

        // Transition to Executed.
        let proposal = self
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        proposal.status = ProposalStatus::Executed;

        // Re-borrow as immutable for the return value.
        Ok(self
            .proposals
            .iter()
            .find(|p| p.id == proposal_id)
            .expect("proposal was just modified"))
    }

    /// Cancel a proposal. Only the original proposer may cancel, and only
    /// while the proposal is Active or Draft.
    pub fn cancel_proposal(
        &mut self,
        proposal_id: u64,
        caller: &[u8; 32],
    ) -> Result<(), GovernanceError> {
        let proposal = self
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        if &proposal.proposer != caller {
            return Err(GovernanceError::NotProposer);
        }

        if proposal.status == ProposalStatus::Active || proposal.status == ProposalStatus::Draft {
            if proposal.status == ProposalStatus::Active {
                self.active_proposal_count = self.active_proposal_count.saturating_sub(1);
            }
            proposal.status = ProposalStatus::Cancelled;
            Ok(())
        } else {
            // Cannot cancel proposals that are already finalized.
            Err(GovernanceError::VotingNotActive)
        }
    }

    /// Check whether a specific address has already voted on a proposal.
    pub fn has_voted(&self, proposal_id: u64, voter: &[u8; 32]) -> bool {
        self.get_proposal(proposal_id)
            .map(|p| p.voters.iter().any(|v| &v.voter == voter))
            .unwrap_or(false)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(n: u8) -> [u8; 32] {
        let mut addr = [0u8; 32];
        addr[0] = n;
        addr
    }

    fn default_state_and_config() -> (GovernanceState, GovernanceConfig) {
        let mut state = GovernanceState::new();
        // Set a reasonable total staked supply so quorum calculations work.
        state.total_staked_supply = 500_000_000_000_000_000; // 500M ARC
        (state, GovernanceConfig::default_config())
    }

    fn make_vote(voter: u8, weight: u128, choice: VoteChoice) -> Vote {
        Vote {
            voter: test_address(voter),
            stake_weight: weight,
            choice,
            cast_at: 100,
            reason: None,
        }
    }

    // 1. Create proposal with correct fields.
    #[test]
    fn test_create_proposal() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        let id = state
            .create_proposal(
                proposer,
                "Upgrade to v2".to_string(),
                "Adds sharding support".to_string(),
                ProposalType::ProtocolUpgrade {
                    version: "2.0.0".to_string(),
                    features: vec!["sharding".to_string()],
                },
                100,
                &config,
            )
            .expect("should create proposal");

        assert_eq!(id, 0);
        assert_eq!(state.proposals.len(), 1);
        assert_eq!(state.next_proposal_id, 1);
        assert_eq!(state.active_proposal_count, 1);
        assert_eq!(state.total_proposals_created, 1);

        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.title, "Upgrade to v2");
        assert_eq!(p.proposer, proposer);
        assert_eq!(p.status, ProposalStatus::Active);
        assert_eq!(p.voting_starts, 100);
        assert_eq!(p.voting_ends, 100 + config.voting_period_blocks);
        assert_eq!(p.votes_for, 0);
        assert_eq!(p.votes_against, 0);
        assert_eq!(p.votes_abstain, 0);
    }

    // 2. Vote For increments votes_for.
    #[test]
    fn test_cast_vote_for() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Test".to_string(),
                "Test proposal".to_string(),
                ProposalType::ParameterChange {
                    parameter: "block_size".to_string(),
                    old_value: "1MB".to_string(),
                    new_value: "2MB".to_string(),
                },
                100,
                &config,
            )
            .unwrap();

        let vote = make_vote(2, 1_000_000, VoteChoice::For);
        state.cast_vote(0, vote, 200).unwrap();

        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.votes_for, 1_000_000);
        assert_eq!(p.votes_against, 0);
        assert_eq!(p.votes_abstain, 0);
        assert_eq!(p.voters.len(), 1);
    }

    // 3. Vote Against increments votes_against.
    #[test]
    fn test_cast_vote_against() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Test".to_string(),
                "Test".to_string(),
                ProposalType::FeatureFlagToggle {
                    feature: "zk_proofs".to_string(),
                    enabled: true,
                },
                100,
                &config,
            )
            .unwrap();

        let vote = make_vote(2, 500_000, VoteChoice::Against);
        state.cast_vote(0, vote, 200).unwrap();

        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.votes_for, 0);
        assert_eq!(p.votes_against, 500_000);
    }

    // 4. Double vote by the same address is rejected.
    #[test]
    fn test_double_vote_rejected() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Test".to_string(),
                "Test".to_string(),
                ProposalType::Custom {
                    data: vec![0x42],
                },
                100,
                &config,
            )
            .unwrap();

        let vote1 = make_vote(2, 1_000, VoteChoice::For);
        state.cast_vote(0, vote1, 200).unwrap();

        let vote2 = make_vote(2, 2_000, VoteChoice::Against);
        let result = state.cast_vote(0, vote2, 201);
        assert_eq!(result, Err(GovernanceError::AlreadyVoted));

        // Totals should reflect only the first vote.
        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.votes_for, 1_000);
        assert_eq!(p.votes_against, 0);
        assert_eq!(p.voters.len(), 1);
    }

    // 5. Proposal passes when quorum is met and threshold exceeded.
    #[test]
    fn test_proposal_passes_with_quorum() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        // Total staked: 500M ARC. Quorum = 40% = 200M ARC.
        // Threshold = 60%.
        state
            .create_proposal(
                proposer,
                "Treasury spend".to_string(),
                "Fund dev team".to_string(),
                ProposalType::TreasurySpend {
                    recipient: test_address(10),
                    amount: 1_000_000,
                    reason: "Development".to_string(),
                },
                100,
                &config,
            )
            .unwrap();

        // Cast 250M For (well above quorum, 100% approval).
        let vote_for = Vote {
            voter: test_address(2),
            stake_weight: 250_000_000_000_000_000,
            choice: VoteChoice::For,
            cast_at: 200,
            reason: None,
        };
        state.cast_vote(0, vote_for, 200).unwrap();

        // Finalize after voting ends.
        let after_voting = 100 + config.voting_period_blocks + 1;
        let status = state
            .finalize_proposal(0, after_voting, state.total_staked_supply, &config)
            .unwrap();

        assert_eq!(status, ProposalStatus::Passed);
        assert_eq!(state.total_proposals_passed, 1);
        assert_eq!(state.active_proposal_count, 0);
    }

    // 6. Proposal fails when quorum is not met.
    #[test]
    fn test_proposal_fails_no_quorum() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        // Quorum = 40% of 500M = 200M ARC.
        state
            .create_proposal(
                proposer,
                "Minor change".to_string(),
                "Tiny tweak".to_string(),
                ProposalType::ParameterChange {
                    parameter: "fee_rate".to_string(),
                    old_value: "100".to_string(),
                    new_value: "200".to_string(),
                },
                100,
                &config,
            )
            .unwrap();

        // Only 1M ARC votes — way below 200M quorum.
        let vote = Vote {
            voter: test_address(2),
            stake_weight: 1_000_000_000_000_000, // 1M ARC
            choice: VoteChoice::For,
            cast_at: 200,
            reason: None,
        };
        state.cast_vote(0, vote, 200).unwrap();

        let after_voting = 100 + config.voting_period_blocks + 1;
        let status = state
            .finalize_proposal(0, after_voting, state.total_staked_supply, &config)
            .unwrap();

        assert_eq!(status, ProposalStatus::Failed);
        assert_eq!(state.total_proposals_failed, 1);
    }

    // 7. Proposal fails when quorum met but approval threshold not reached.
    #[test]
    fn test_proposal_fails_threshold() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Controversial".to_string(),
                "Split vote".to_string(),
                ProposalType::Custom { data: vec![] },
                100,
                &config,
            )
            .unwrap();

        // 120M For, 130M Against = 250M total (above 200M quorum).
        // For% = 120/250 = 48%, below 60% threshold.
        let vote_for = Vote {
            voter: test_address(2),
            stake_weight: 120_000_000_000_000_000,
            choice: VoteChoice::For,
            cast_at: 200,
            reason: None,
        };
        let vote_against = Vote {
            voter: test_address(3),
            stake_weight: 130_000_000_000_000_000,
            choice: VoteChoice::Against,
            cast_at: 201,
            reason: None,
        };
        state.cast_vote(0, vote_for, 200).unwrap();
        state.cast_vote(0, vote_against, 201).unwrap();

        let after_voting = 100 + config.voting_period_blocks + 1;
        let status = state
            .finalize_proposal(0, after_voting, state.total_staked_supply, &config)
            .unwrap();

        assert_eq!(status, ProposalStatus::Failed);
    }

    // 8. Execution only allowed after delay, not before; and not after window.
    #[test]
    fn test_execute_after_delay() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Upgrade".to_string(),
                "Protocol upgrade".to_string(),
                ProposalType::ProtocolUpgrade {
                    version: "3.0.0".to_string(),
                    features: vec![],
                },
                100,
                &config,
            )
            .unwrap();

        // Cast enough votes to pass.
        let vote = Vote {
            voter: test_address(2),
            stake_weight: 300_000_000_000_000_000, // 300M — above quorum
            choice: VoteChoice::For,
            cast_at: 200,
            reason: None,
        };
        state.cast_vote(0, vote, 200).unwrap();

        // Finalize.
        let voting_ends = 100 + config.voting_period_blocks;
        state
            .finalize_proposal(
                0,
                voting_ends + 1,
                state.total_staked_supply,
                &config,
            )
            .unwrap();

        let earliest_execution = voting_ends + config.execution_delay_blocks;

        // Too early — cannot execute.
        assert!(!state.can_execute(0, earliest_execution - 1));

        // Exactly at the earliest allowed block.
        assert!(state.can_execute(0, earliest_execution));

        // Within the execution window.
        assert!(state.can_execute(0, earliest_execution + 100));

        // Execute.
        let p = state.execute_proposal(0, earliest_execution).unwrap();
        assert_eq!(p.status, ProposalStatus::Executed);

        // Cannot execute again.
        assert!(!state.can_execute(0, earliest_execution + 1));
    }

    // 9. Proposer can cancel their own proposal.
    #[test]
    fn test_cancel_proposal() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Cancel me".to_string(),
                "Will be cancelled".to_string(),
                ProposalType::Custom { data: vec![] },
                100,
                &config,
            )
            .unwrap();

        assert_eq!(state.active_proposal_count, 1);

        state.cancel_proposal(0, &proposer).unwrap();

        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.status, ProposalStatus::Cancelled);
        assert_eq!(state.active_proposal_count, 0);
    }

    // 10. Non-proposer cannot cancel.
    #[test]
    fn test_cancel_not_proposer_fails() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);
        let imposter = test_address(99);

        state
            .create_proposal(
                proposer,
                "Don't cancel".to_string(),
                "Should stay".to_string(),
                ProposalType::Custom { data: vec![] },
                100,
                &config,
            )
            .unwrap();

        let result = state.cancel_proposal(0, &imposter);
        assert_eq!(result, Err(GovernanceError::NotProposer));

        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.status, ProposalStatus::Active);
    }

    // 11. GovernanceConfig defaults are sensible.
    #[test]
    fn test_governance_config_defaults() {
        let config = GovernanceConfig::default_config();

        // Core tier minimum = 50M ARC.
        assert_eq!(config.min_proposal_stake, 50_000_000_000_000_000);

        // Voting period: ~7 days (1_512_000 blocks at 400 ms/block).
        assert_eq!(config.voting_period_blocks, 1_512_000);

        // Execution delay: ~2 days.
        assert_eq!(config.execution_delay_blocks, 432_000);

        // Quorum: 40%.
        assert_eq!(config.quorum_percentage_bps, 4000);

        // Standard threshold: 60%.
        assert_eq!(config.approval_threshold_bps, 6000);

        // Emergency threshold: 75%.
        assert_eq!(config.emergency_threshold_bps, 7500);

        // Max active proposals.
        assert!(config.max_active_proposals > 0);

        // Quorum calculation: 40% of 1000 = 400.
        assert_eq!(config.quorum_amount(1000), 400);
    }

    // 12. active_proposals only returns Active status proposals.
    #[test]
    fn test_active_proposals_filter() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        // Create 3 proposals with sufficient spacing to avoid cooldown.
        let spacing = config.proposal_cooldown_blocks;

        state
            .create_proposal(
                proposer,
                "P0".to_string(),
                "First".to_string(),
                ProposalType::Custom { data: vec![] },
                100,
                &config,
            )
            .unwrap();

        state
            .create_proposal(
                proposer,
                "P1".to_string(),
                "Second".to_string(),
                ProposalType::Custom { data: vec![] },
                100 + spacing,
                &config,
            )
            .unwrap();

        state
            .create_proposal(
                proposer,
                "P2".to_string(),
                "Third".to_string(),
                ProposalType::Custom { data: vec![] },
                100 + spacing * 2,
                &config,
            )
            .unwrap();

        assert_eq!(state.active_proposals().len(), 3);

        // Cancel P0.
        state.cancel_proposal(0, &proposer).unwrap();

        let active = state.active_proposals();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|p| p.status == ProposalStatus::Active));
        assert!(active.iter().all(|p| p.id != 0));
    }
}
