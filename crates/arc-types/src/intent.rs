// Add to lib.rs: pub mod intent;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Intent ─────────────────────────────────────────────────────────────────

/// User intent — a declarative transaction request.
///
/// Users declare WHAT they want (e.g. "swap 100 USDC for at least 99 DAI"),
/// not HOW it happens. Solvers compete to fulfill intents optimally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    /// Unique identifier (BLAKE3 hash of signable content).
    pub intent_id: [u8; 32],
    /// Address of the user who created this intent.
    pub creator: [u8; 32],
    /// The declarative operation requested.
    pub intent_type: IntentType,
    /// Execution constraints the solver must satisfy.
    pub constraints: Vec<Constraint>,
    /// Block height after which this intent expires.
    pub deadline: u64,
    /// Maximum gas the user is willing to pay.
    pub max_gas: u64,
    /// Tip paid to the winning solver (in ARC).
    pub tip: u64,
    /// Block height at which this intent was created.
    pub created_at: u64,
    /// Current lifecycle status.
    pub status: IntentStatus,
    /// User nonce (replay protection).
    pub nonce: u64,
}

/// The kind of operation an intent declares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IntentType {
    /// Swap token A for token B at the best available price.
    Swap {
        token_in: [u8; 32],
        token_out: [u8; 32],
        amount_in: u128,
        min_amount_out: u128,
    },
    /// Transfer tokens to a recipient.
    Transfer {
        token: [u8; 32],
        recipient: [u8; 32],
        amount: u128,
    },
    /// Bridge tokens to another chain.
    Bridge {
        token: [u8; 32],
        dest_chain: u64,
        dest_address: [u8; 20],
        amount: u128,
    },
    /// Provide liquidity optimally across available pools.
    ProvideLiquidity {
        token_a: [u8; 32],
        token_b: [u8; 32],
        amount: u128,
        /// Optional min/max price for concentrated liquidity positions.
        price_range: Option<(u128, u128)>,
    },
    /// Stake ARC tokens for yield.
    Stake {
        amount: u64,
        /// Minimum acceptable APY in basis points.
        min_apy_bps: u16,
    },
    /// Batch multiple operations atomically.
    Batch {
        intents: Vec<IntentType>,
        /// If true, all sub-intents must succeed or the entire batch reverts.
        require_all: bool,
    },
    /// Custom intent with arbitrary action descriptor and payload.
    Custom {
        action: String,
        data: Vec<u8>,
    },
}

/// Constraint on how an intent may be executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Constraint {
    /// Maximum price slippage in basis points (e.g. 50 = 0.5%).
    MaxSlippage(u16),
    /// Maximum gas price the user will accept.
    MaxGasPrice(u64),
    /// Minimum output amount required.
    MinOutputAmount(u128),
    /// Exact output amount required (no more, no less).
    ExactOutputAmount(u128),
    /// Block height deadline for execution.
    DeadlineBlock(u64),
    /// Prefer this solver if available.
    PreferredSolver([u8; 32]),
    /// Exclude this solver from consideration.
    ExcludeSolver([u8; 32]),
    /// Maximum number of blocks the execution may take.
    MaxExecutionTime(u64),
    /// Require all solution steps to execute atomically.
    RequireAtomicExecution,
}

/// Lifecycle status of an intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentStatus {
    /// Submitted to the pool, awaiting a solver.
    Pending,
    /// A solver has committed to fulfilling this intent.
    Matched,
    /// Currently being executed on-chain.
    Executing,
    /// Successfully completed.
    Fulfilled,
    /// Execution failed.
    Failed,
    /// Deadline block height passed without fulfillment.
    Expired,
    /// Cancelled by the creator.
    Cancelled,
}

// ─── Solution ───────────────────────────────────────────────────────────────

/// A solver's proposed execution plan for fulfilling an intent.
///
/// Solvers stake tokens as collateral — if the solution fails or violates
/// constraints, the stake is slashed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solution {
    /// Unique identifier (BLAKE3 hash of solution content).
    pub solution_id: [u8; 32],
    /// The intent this solution fulfills.
    pub intent_id: [u8; 32],
    /// Address of the solver submitting this solution.
    pub solver: [u8; 32],
    /// Ordered execution steps.
    pub steps: Vec<SolutionStep>,
    /// Total estimated gas for all steps.
    pub estimated_gas: u64,
    /// Expected output amount for the user.
    pub expected_output: u128,
    /// Solver's collateral backing this solution.
    pub solver_stake: u64,
    /// Quality score (higher = better for the user). Used for ranking.
    pub score: u64,
    /// Block height when this solution was submitted.
    pub submitted_at: u64,
}

/// A single atomic step within a solution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionStep {
    /// The type of on-chain action.
    pub action: StepAction,
    /// Target contract address for the call.
    pub target_contract: [u8; 32],
    /// ABI-encoded call data.
    pub call_data: Vec<u8>,
    /// Value transferred with the call (in native token).
    pub value: u128,
    /// Estimated gas for this step.
    pub gas_estimate: u64,
}

/// Action type for a solution step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepAction {
    Swap,
    Transfer,
    Approve,
    Bridge,
    Stake,
    Custom(String),
}

// ─── Solver ─────────────────────────────────────────────────────────────────

/// Registered solver that competes to fulfill intents.
///
/// Solvers stake ARC tokens as collateral and build reputation through
/// successful fulfillments. Higher reputation unlocks priority matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solver {
    /// Solver's on-chain address.
    pub address: [u8; 32],
    /// Staked ARC tokens (collateral).
    pub stake: u64,
    /// Reputation score based on historical performance.
    pub reputation_score: u64,
    /// Total number of intents successfully solved.
    pub total_solved: u64,
    /// Total number of intents where execution failed.
    pub total_failed: u64,
    /// Types of intents this solver specializes in.
    pub specializations: Vec<String>,
    /// Whether this solver is currently accepting intents.
    pub is_active: bool,
    /// Block height when this solver registered.
    pub registered_at: u64,
}

// ─── Intent Pool ────────────────────────────────────────────────────────────

/// In-memory pool of pending intents, proposed solutions, and registered solvers.
///
/// Analogous to a mempool but for declarative intents rather than raw transactions.
pub struct IntentPool {
    pending: Vec<Intent>,
    solutions: Vec<Solution>,
    solvers: Vec<Solver>,
    capacity: usize,
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Errors that can occur during intent pool operations.
#[derive(Debug, Error)]
pub enum IntentError {
    #[error("intent pool is at capacity ({0} intents)")]
    PoolFull(usize),
    #[error("intent has expired (deadline passed)")]
    IntentExpired,
    #[error("duplicate intent already exists in pool")]
    DuplicateIntent,
    #[error("invalid constraint on intent")]
    InvalidConstraint,
    #[error("no solver registered that can handle this intent")]
    NoMatchingSolver,
    #[error("solver is not registered in the pool")]
    SolverNotRegistered,
    #[error("solver stake ({have}) is below minimum ({need})")]
    InsufficientSolverStake { have: u64, need: u64 },
}

// ─── Implementations ────────────────────────────────────────────────────────

impl Intent {
    /// Create a new swap intent.
    pub fn new_swap(
        creator: [u8; 32],
        token_in: [u8; 32],
        token_out: [u8; 32],
        amount_in: u128,
        min_out: u128,
        deadline: u64,
        nonce: u64,
    ) -> Self {
        let mut intent = Self {
            intent_id: [0u8; 32],
            creator,
            intent_type: IntentType::Swap {
                token_in,
                token_out,
                amount_in,
                min_amount_out: min_out,
            },
            constraints: Vec::new(),
            deadline,
            max_gas: 0,
            tip: 0,
            created_at: 0,
            status: IntentStatus::Pending,
            nonce,
        };
        intent.intent_id = intent.compute_id();
        intent
    }

    /// Create a new transfer intent.
    pub fn new_transfer(
        creator: [u8; 32],
        token: [u8; 32],
        recipient: [u8; 32],
        amount: u128,
        deadline: u64,
        nonce: u64,
    ) -> Self {
        let mut intent = Self {
            intent_id: [0u8; 32],
            creator,
            intent_type: IntentType::Transfer {
                token,
                recipient,
                amount,
            },
            constraints: Vec::new(),
            deadline,
            max_gas: 0,
            tip: 0,
            created_at: 0,
            status: IntentStatus::Pending,
            nonce,
        };
        intent.intent_id = intent.compute_id();
        intent
    }

    /// Compute the intent ID as the BLAKE3 hash of all signable fields.
    ///
    /// Covers: `creator || intent_type || constraints || deadline || max_gas || tip || nonce`
    /// Does NOT include `intent_id`, `created_at`, or `status`.
    pub fn compute_id(&self) -> [u8; 32] {
        let type_bytes = bincode::serialize(&self.intent_type).expect("serializable");
        let constraint_bytes = bincode::serialize(&self.constraints).expect("serializable");

        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-intent-v1");
        hasher.update(&self.creator);
        hasher.update(&type_bytes);
        hasher.update(&constraint_bytes);
        hasher.update(&self.deadline.to_le_bytes());
        hasher.update(&self.max_gas.to_le_bytes());
        hasher.update(&self.tip.to_le_bytes());
        hasher.update(&self.nonce.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Returns true if this intent has passed its deadline.
    pub fn is_expired(&self, current_height: u64) -> bool {
        current_height >= self.deadline
    }

    /// Add an execution constraint to this intent.
    pub fn add_constraint(&mut self, constraint: Constraint) {
        self.constraints.push(constraint);
    }

    /// Returns the max slippage constraint (in basis points), if set.
    pub fn max_slippage(&self) -> Option<u16> {
        self.constraints.iter().find_map(|c| match c {
            Constraint::MaxSlippage(bps) => Some(*bps),
            _ => None,
        })
    }
}

impl Solution {
    /// Create a new solution for an intent.
    pub fn new(
        intent_id: [u8; 32],
        solver: [u8; 32],
        steps: Vec<SolutionStep>,
        expected_output: u128,
        solver_stake: u64,
    ) -> Self {
        let mut solution = Self {
            solution_id: [0u8; 32],
            intent_id,
            solver,
            steps,
            estimated_gas: 0,
            expected_output,
            solver_stake,
            score: 0,
            submitted_at: 0,
        };
        solution.estimated_gas = solution.total_gas();
        solution.solution_id = solution.compute_id();
        solution
    }

    /// Compute the solution ID as the BLAKE3 hash of all fields.
    pub fn compute_id(&self) -> [u8; 32] {
        let steps_bytes = bincode::serialize(&self.steps).expect("serializable");

        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-solution-v1");
        hasher.update(&self.intent_id);
        hasher.update(&self.solver);
        hasher.update(&steps_bytes);
        hasher.update(&self.expected_output.to_le_bytes());
        hasher.update(&self.solver_stake.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Total gas estimate across all steps.
    pub fn total_gas(&self) -> u64 {
        self.steps.iter().map(|s| s.gas_estimate).sum()
    }

    /// Number of execution steps.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }
}

impl Solver {
    /// Create a new solver with the given address and initial stake.
    pub fn new(address: [u8; 32], stake: u64) -> Self {
        Self {
            address,
            stake,
            reputation_score: 0,
            total_solved: 0,
            total_failed: 0,
            specializations: Vec::new(),
            is_active: true,
            registered_at: 0,
        }
    }

    /// Fraction of intents that were solved successfully (0.0 to 1.0).
    ///
    /// Returns 0.0 if no intents have been attempted.
    pub fn success_rate(&self) -> f64 {
        let total = self.total_solved + self.total_failed;
        if total == 0 {
            return 0.0;
        }
        self.total_solved as f64 / total as f64
    }

    /// Record a successful intent fulfillment.
    pub fn record_success(&mut self) {
        self.total_solved += 1;
        // Reputation grows with successful solves.
        self.reputation_score = self.reputation_score.saturating_add(10);
    }

    /// Record a failed intent fulfillment.
    pub fn record_failure(&mut self) {
        self.total_failed += 1;
        // Reputation decays on failure.
        self.reputation_score = self.reputation_score.saturating_sub(20);
    }
}

impl IntentPool {
    /// Create a new intent pool with the given maximum capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: Vec::new(),
            solutions: Vec::new(),
            solvers: Vec::new(),
            capacity,
        }
    }

    /// Submit a new intent to the pool.
    ///
    /// Rejects duplicates, expired intents, and submissions when the pool is full.
    pub fn submit_intent(&mut self, intent: Intent) -> Result<(), IntentError> {
        if self.pending.len() >= self.capacity {
            return Err(IntentError::PoolFull(self.capacity));
        }
        // Check for duplicate intent IDs.
        if self.pending.iter().any(|i| i.intent_id == intent.intent_id) {
            return Err(IntentError::DuplicateIntent);
        }
        // Reject already-expired intents (deadline 0 is treated as immediate expiry).
        if intent.status != IntentStatus::Pending {
            return Err(IntentError::InvalidConstraint);
        }
        self.pending.push(intent);
        Ok(())
    }

    /// Submit a solution for a pending intent.
    ///
    /// The solver must be registered in the pool.
    pub fn submit_solution(&mut self, solution: Solution) -> Result<(), IntentError> {
        // Verify the solver is registered.
        let solver_registered = self.solvers.iter().any(|s| s.address == solution.solver);
        if !solver_registered {
            return Err(IntentError::SolverNotRegistered);
        }
        self.solutions.push(solution);
        Ok(())
    }

    /// Find the best solution for a given intent, selected by highest score.
    ///
    /// Returns `None` if no solutions exist for this intent.
    /// Removes the winning solution from the pool on match.
    pub fn match_intent(&mut self, intent_id: &[u8; 32]) -> Option<Solution> {
        // Find the index of the best-scoring solution for this intent.
        let best_idx = self
            .solutions
            .iter()
            .enumerate()
            .filter(|(_, s)| &s.intent_id == intent_id)
            .max_by_key(|(_, s)| s.score)
            .map(|(idx, _)| idx);

        best_idx.map(|idx| self.solutions.remove(idx))
    }

    /// View all pending intents.
    pub fn pending_intents(&self) -> &[Intent] {
        &self.pending
    }

    /// Expire and remove all intents whose deadline has passed.
    ///
    /// Returns the number of intents expired.
    pub fn expire_old(&mut self, current_height: u64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|i| !i.is_expired(current_height));
        before - self.pending.len()
    }

    /// Register a new solver in the pool.
    pub fn register_solver(&mut self, solver: Solver) {
        self.solvers.push(solver);
    }

    /// Number of registered solvers.
    pub fn solver_count(&self) -> usize {
        self.solvers.len()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_addr(n: u8) -> [u8; 32] {
        let mut addr = [0u8; 32];
        addr[0] = n;
        addr
    }

    fn test_token(n: u8) -> [u8; 32] {
        let mut tok = [0u8; 32];
        tok[31] = n;
        tok
    }

    // ── Intent creation ──

    #[test]
    fn test_create_swap_intent() {
        let intent = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            0,
        );

        assert_eq!(intent.creator, test_addr(1));
        assert_eq!(intent.deadline, 100);
        assert_eq!(intent.nonce, 0);
        assert_eq!(intent.status, IntentStatus::Pending);
        assert_ne!(intent.intent_id, [0u8; 32], "ID must be computed");

        match &intent.intent_type {
            IntentType::Swap {
                token_in,
                token_out,
                amount_in,
                min_amount_out,
            } => {
                assert_eq!(*token_in, test_token(10));
                assert_eq!(*token_out, test_token(20));
                assert_eq!(*amount_in, 1000);
                assert_eq!(*min_amount_out, 990);
            }
            _ => panic!("expected Swap intent type"),
        }
    }

    #[test]
    fn test_create_transfer_intent() {
        let intent = Intent::new_transfer(
            test_addr(1),
            test_token(10),
            test_addr(2),
            500,
            200,
            1,
        );

        assert_eq!(intent.creator, test_addr(1));
        assert_eq!(intent.deadline, 200);
        assert_eq!(intent.nonce, 1);
        assert_eq!(intent.status, IntentStatus::Pending);
        assert_ne!(intent.intent_id, [0u8; 32]);

        match &intent.intent_type {
            IntentType::Transfer {
                token,
                recipient,
                amount,
            } => {
                assert_eq!(*token, test_token(10));
                assert_eq!(*recipient, test_addr(2));
                assert_eq!(*amount, 500);
            }
            _ => panic!("expected Transfer intent type"),
        }
    }

    // ── Expiration ──

    #[test]
    fn test_intent_expiration() {
        let intent = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            0,
        );

        assert!(!intent.is_expired(50), "should not be expired before deadline");
        assert!(!intent.is_expired(99), "should not be expired one block before");
        assert!(intent.is_expired(100), "should be expired at deadline");
        assert!(intent.is_expired(200), "should be expired after deadline");
    }

    // ── Constraints ──

    #[test]
    fn test_intent_constraints() {
        let mut intent = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            0,
        );

        assert_eq!(intent.max_slippage(), None);

        intent.add_constraint(Constraint::MaxSlippage(50));
        intent.add_constraint(Constraint::RequireAtomicExecution);
        intent.add_constraint(Constraint::MaxGasPrice(1000));

        assert_eq!(intent.max_slippage(), Some(50));
        assert_eq!(intent.constraints.len(), 3);
    }

    // ── Solutions ──

    #[test]
    fn test_solution_creation() {
        let steps = vec![
            SolutionStep {
                action: StepAction::Approve,
                target_contract: test_addr(10),
                call_data: vec![0xAA],
                value: 0,
                gas_estimate: 50_000,
            },
            SolutionStep {
                action: StepAction::Swap,
                target_contract: test_addr(11),
                call_data: vec![0xBB, 0xCC],
                value: 0,
                gas_estimate: 150_000,
            },
        ];

        let solution = Solution::new(
            [1u8; 32],
            test_addr(5),
            steps,
            995,
            1000,
        );

        assert_eq!(solution.intent_id, [1u8; 32]);
        assert_eq!(solution.solver, test_addr(5));
        assert_eq!(solution.step_count(), 2);
        assert_eq!(solution.total_gas(), 200_000);
        assert_eq!(solution.estimated_gas, 200_000);
        assert_eq!(solution.expected_output, 995);
        assert_eq!(solution.solver_stake, 1000);
        assert_ne!(solution.solution_id, [0u8; 32], "ID must be computed");
    }

    // ── Solver reputation ──

    #[test]
    fn test_solver_reputation() {
        let mut solver = Solver::new(test_addr(1), 10_000);

        assert_eq!(solver.success_rate(), 0.0);
        assert_eq!(solver.total_solved, 0);
        assert_eq!(solver.total_failed, 0);
        assert!(solver.is_active);

        // Record 8 successes, 2 failures.
        for _ in 0..8 {
            solver.record_success();
        }
        for _ in 0..2 {
            solver.record_failure();
        }

        assert_eq!(solver.total_solved, 8);
        assert_eq!(solver.total_failed, 2);
        assert!((solver.success_rate() - 0.8).abs() < f64::EPSILON);
        // Reputation: 8*10 - 2*20 = 40
        assert_eq!(solver.reputation_score, 40);
    }

    // ── Intent Pool ──

    #[test]
    fn test_intent_pool_submit() {
        let mut pool = IntentPool::new(100);

        let intent = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            0,
        );
        let id = intent.intent_id;

        pool.submit_intent(intent).expect("submit ok");
        assert_eq!(pool.pending_intents().len(), 1);
        assert_eq!(pool.pending_intents()[0].intent_id, id);
    }

    #[test]
    fn test_intent_pool_capacity() {
        let mut pool = IntentPool::new(2);

        let i1 = Intent::new_swap(test_addr(1), test_token(1), test_token(2), 100, 90, 100, 0);
        let i2 = Intent::new_swap(test_addr(2), test_token(1), test_token(2), 200, 180, 100, 0);
        let i3 = Intent::new_swap(test_addr(3), test_token(1), test_token(2), 300, 270, 100, 0);

        pool.submit_intent(i1).expect("first ok");
        pool.submit_intent(i2).expect("second ok");

        let err = pool.submit_intent(i3).unwrap_err();
        assert!(matches!(err, IntentError::PoolFull(2)));
    }

    #[test]
    fn test_match_best_solution() {
        let mut pool = IntentPool::new(100);

        let intent = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            0,
        );
        let intent_id = intent.intent_id;
        pool.submit_intent(intent).expect("submit ok");

        // Register two solvers.
        pool.register_solver(Solver::new(test_addr(50), 5000));
        pool.register_solver(Solver::new(test_addr(51), 8000));

        // Solver A: score 100.
        let mut sol_a = Solution::new(intent_id, test_addr(50), vec![], 990, 5000);
        sol_a.score = 100;
        pool.submit_solution(sol_a).expect("solution a ok");

        // Solver B: score 200 (better).
        let mut sol_b = Solution::new(intent_id, test_addr(51), vec![], 995, 8000);
        sol_b.score = 200;
        pool.submit_solution(sol_b).expect("solution b ok");

        // Best match should be solver B (higher score).
        let best = pool.match_intent(&intent_id).expect("should find match");
        assert_eq!(best.solver, test_addr(51));
        assert_eq!(best.score, 200);
    }

    #[test]
    fn test_expire_old_intents() {
        let mut pool = IntentPool::new(100);

        let i1 = Intent::new_swap(test_addr(1), test_token(1), test_token(2), 100, 90, 50, 0);
        let i2 = Intent::new_swap(test_addr(2), test_token(1), test_token(2), 200, 180, 150, 0);
        let i3 = Intent::new_swap(test_addr(3), test_token(1), test_token(2), 300, 270, 80, 1);

        pool.submit_intent(i1).expect("ok");
        pool.submit_intent(i2).expect("ok");
        pool.submit_intent(i3).expect("ok");

        assert_eq!(pool.pending_intents().len(), 3);

        // Expire at block 100: intents with deadline <= 100 are removed.
        let expired = pool.expire_old(100);
        assert_eq!(expired, 2, "two intents should expire (deadline 50 and 80)");
        assert_eq!(pool.pending_intents().len(), 1);
        assert_eq!(pool.pending_intents()[0].deadline, 150);
    }

    #[test]
    fn test_batch_intent() {
        let batch_type = IntentType::Batch {
            intents: vec![
                IntentType::Swap {
                    token_in: test_token(1),
                    token_out: test_token(2),
                    amount_in: 1000,
                    min_amount_out: 990,
                },
                IntentType::Transfer {
                    token: test_token(2),
                    recipient: test_addr(3),
                    amount: 500,
                },
                IntentType::Stake {
                    amount: 200,
                    min_apy_bps: 500,
                },
            ],
            require_all: true,
        };

        let mut intent = Intent {
            intent_id: [0u8; 32],
            creator: test_addr(1),
            intent_type: batch_type,
            constraints: Vec::new(),
            deadline: 100,
            max_gas: 500_000,
            tip: 50,
            created_at: 0,
            status: IntentStatus::Pending,
            nonce: 0,
        };
        intent.intent_id = intent.compute_id();

        assert_ne!(intent.intent_id, [0u8; 32]);
        match &intent.intent_type {
            IntentType::Batch {
                intents,
                require_all,
            } => {
                assert_eq!(intents.len(), 3);
                assert!(*require_all);
            }
            _ => panic!("expected Batch intent type"),
        }
    }

    #[test]
    fn test_intent_id_deterministic() {
        let a = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            42,
        );
        let b = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            42,
        );

        assert_eq!(a.intent_id, b.intent_id, "same inputs must produce same ID");

        // Different nonce produces different ID.
        let c = Intent::new_swap(
            test_addr(1),
            test_token(10),
            test_token(20),
            1000,
            990,
            100,
            43,
        );
        assert_ne!(a.intent_id, c.intent_id, "different nonce must produce different ID");
    }
}
