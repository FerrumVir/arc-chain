use serde::{Deserialize, Serialize};
use std::fmt;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoteChoice {
    For,
    Against,
    Abstain,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub voter: [u8; 32],
    pub stake_weight: u128,
    pub choice: VoteChoice,
    pub cast_at: u64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProposalType {
    ProtocolUpgrade {
        version: String,
        features: Vec<String>,
    },
    ParameterChange {
        parameter: String,
        old_value: String,
        new_value: String,
    },
    TreasurySpend {
        recipient: [u8; 32],
        amount: u64,
        reason: String,
    },
    EmergencyAction {
        action: String,
    },
    AddValidator {
        address: [u8; 32],
        initial_stake: u64,
    },
    RemoveValidator {
        address: [u8; 32],
        reason: String,
    },
    FeatureFlagToggle {
        feature: String,
        enabled: bool,
    },
    Custom {
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    Draft,
    Active,
    Passed,
    Failed,
    Executed,
    Cancelled,
    Expired,
    Vetoed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: u64,
    pub proposer: [u8; 32],
    pub title: String,
    pub description: String,
    pub proposal_type: ProposalType,
    pub created_at: u64,
    pub voting_starts: u64,
    pub voting_ends: u64,
    pub execution_delay: u64,
    pub status: ProposalStatus,
    pub votes_for: u128,
    pub votes_against: u128,
    pub votes_abstain: u128,
    pub quorum_required: u128,
    pub approval_threshold_bps: u16,
    pub voters: Vec<Vote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceConfig {
    pub min_proposal_stake: u64,
    pub voting_period_blocks: u64,
    pub execution_delay_blocks: u64,
    pub quorum_percentage_bps: u16,
    pub approval_threshold_bps: u16,
    pub emergency_threshold_bps: u16,
    pub max_active_proposals: u32,
    pub proposal_cooldown_blocks: u64,
}

impl GovernanceConfig {
    pub fn default_config() -> Self {
        Self {
            min_proposal_stake: 50_000_000_000_000_000,
            voting_period_blocks: 1_512_000,
            execution_delay_blocks: 432_000,
            quorum_percentage_bps: 4000,
            approval_threshold_bps: 6000,
            emergency_threshold_bps: 7500,
            max_active_proposals: 10,
            proposal_cooldown_blocks: 216_000,
        }
    }

    pub fn quorum_amount(&self, total_staked: u128) -> u128 {
        total_staked * self.quorum_percentage_bps as u128 / 10_000
    }
}

#[derive(Debug, Clone, Default)]
pub struct GovernanceState {
    pub proposals: Vec<Proposal>,
    pub next_proposal_id: u64,
    pub total_staked_supply: u128,
    pub active_proposal_count: u32,
    pub total_proposals_created: u64,
    pub total_proposals_passed: u64,
    pub total_proposals_failed: u64,
}

const EXECUTION_WINDOW_BLOCKS: u64 = 648_000;

impl GovernanceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_proposal(
        &mut self,
        proposer: [u8; 32],
        title: String,
        description: String,
        proposal_type: ProposalType,
        current_height: u64,
        config: &GovernanceConfig,
    ) -> Result<u64, GovernanceError> {
        if self.active_proposal_count >= config.max_active_proposals {
            return Err(GovernanceError::TooManyActiveProposals);
        }

        let last_proposal_height = self
            .proposals
            .iter()
            .rev()
            .filter(|p| p.proposer == proposer)
            .map(|p| p.created_at)
            .next();

        if let Some(last_height) = last_proposal_height
            && current_height < last_height + config.proposal_cooldown_blocks
        {
            return Err(GovernanceError::CooldownNotExpired);
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

        if proposal.voters.iter().any(|v| v.voter == vote.voter) {
            return Err(GovernanceError::AlreadyVoted);
        }

        match vote.choice {
            VoteChoice::For => proposal.votes_for += vote.stake_weight,
            VoteChoice::Against => proposal.votes_against += vote.stake_weight,
            VoteChoice::Abstain => proposal.votes_abstain += vote.stake_weight,
        }

        proposal.voters.push(vote);
        Ok(())
    }

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

        if current_height < proposal.voting_ends {
            return Err(GovernanceError::VotingNotActive);
        }

        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum = config.quorum_amount(total_staked);

        if total_votes < quorum {
            proposal.status = ProposalStatus::Failed;
            self.active_proposal_count = self.active_proposal_count.saturating_sub(1);
            self.total_proposals_failed += 1;
            return Ok(ProposalStatus::Failed);
        }

        let decisive_votes = proposal.votes_for + proposal.votes_against;
        let passes = if decisive_votes == 0 {
            false
        } else {
            proposal.votes_for * 10_000 >= proposal.approval_threshold_bps as u128 * decisive_votes
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

    pub fn get_proposal(&self, id: u64) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.id == id)
    }

    pub fn active_proposals(&self) -> Vec<&Proposal> {
        self.proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Active)
            .collect()
    }

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

    pub fn execute_proposal(
        &mut self,
        proposal_id: u64,
        current_height: u64,
    ) -> Result<&Proposal, GovernanceError> {
        let (status, voting_ends, execution_delay) = {
            let proposal = self
                .proposals
                .iter()
                .find(|p| p.id == proposal_id)
                .ok_or(GovernanceError::ProposalNotFound)?;
            (
                proposal.status,
                proposal.voting_ends,
                proposal.execution_delay,
            )
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

        let proposal = self
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        proposal.status = ProposalStatus::Executed;

        Ok(self
            .proposals
            .iter()
            .find(|p| p.id == proposal_id)
            .expect("proposal was just modified"))
    }

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
            Err(GovernanceError::VotingNotActive)
        }
    }

    pub fn has_voted(&self, proposal_id: u64, voter: &[u8; 32]) -> bool {
        self.get_proposal(proposal_id)
            .map(|p| p.voters.iter().any(|v| &v.voter == voter))
            .unwrap_or(false)
    }
}

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
        state.total_staked_supply = 500_000_000_000_000_000;
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

    #[test]
    fn test_double_vote_rejected() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

        state
            .create_proposal(
                proposer,
                "Test".to_string(),
                "Test".to_string(),
                ProposalType::Custom { data: vec![0x42] },
                100,
                &config,
            )
            .unwrap();

        let vote1 = make_vote(2, 1_000, VoteChoice::For);
        state.cast_vote(0, vote1, 200).unwrap();

        let vote2 = make_vote(2, 2_000, VoteChoice::Against);
        let result = state.cast_vote(0, vote2, 201);
        assert_eq!(result, Err(GovernanceError::AlreadyVoted));

        let p = state.get_proposal(0).unwrap();
        assert_eq!(p.votes_for, 1_000);
        assert_eq!(p.votes_against, 0);
        assert_eq!(p.voters.len(), 1);
    }

    #[test]
    fn test_proposal_passes_with_quorum() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

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

        let vote_for = Vote {
            voter: test_address(2),
            stake_weight: 250_000_000_000_000_000,
            choice: VoteChoice::For,
            cast_at: 200,
            reason: None,
        };
        state.cast_vote(0, vote_for, 200).unwrap();

        let after_voting = 100 + config.voting_period_blocks + 1;
        let status = state
            .finalize_proposal(0, after_voting, state.total_staked_supply, &config)
            .unwrap();

        assert_eq!(status, ProposalStatus::Passed);
        assert_eq!(state.total_proposals_passed, 1);
        assert_eq!(state.active_proposal_count, 0);
    }

    #[test]
    fn test_proposal_fails_no_quorum() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

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

        let vote = Vote {
            voter: test_address(2),
            stake_weight: 1_000_000_000_000_000,
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

        let vote = Vote {
            voter: test_address(2),
            stake_weight: 300_000_000_000_000_000,
            choice: VoteChoice::For,
            cast_at: 200,
            reason: None,
        };
        state.cast_vote(0, vote, 200).unwrap();

        let voting_ends = 100 + config.voting_period_blocks;
        state
            .finalize_proposal(0, voting_ends + 1, state.total_staked_supply, &config)
            .unwrap();

        let earliest_execution = voting_ends + config.execution_delay_blocks;

        assert!(!state.can_execute(0, earliest_execution - 1));

        assert!(state.can_execute(0, earliest_execution));

        assert!(state.can_execute(0, earliest_execution + 100));

        let p = state.execute_proposal(0, earliest_execution).unwrap();
        assert_eq!(p.status, ProposalStatus::Executed);

        assert!(!state.can_execute(0, earliest_execution + 1));
    }

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

    #[test]
    fn test_governance_config_defaults() {
        let config = GovernanceConfig::default_config();

        assert_eq!(config.min_proposal_stake, 50_000_000_000_000_000);

        assert_eq!(config.voting_period_blocks, 1_512_000);

        assert_eq!(config.execution_delay_blocks, 432_000);

        assert_eq!(config.quorum_percentage_bps, 4000);

        assert_eq!(config.approval_threshold_bps, 6000);

        assert_eq!(config.emergency_threshold_bps, 7500);

        assert!(config.max_active_proposals > 0);

        assert_eq!(config.quorum_amount(1000), 400);
    }

    #[test]
    fn test_active_proposals_filter() {
        let (mut state, config) = default_state_and_config();
        let proposer = test_address(1);

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

        state.cancel_proposal(0, &proposer).unwrap();

        let active = state.active_proposals();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|p| p.status == ProposalStatus::Active));
        assert!(active.iter().all(|p| p.id != 0));
    }
}
