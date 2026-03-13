//! Unified Testing Framework
//!
//! Provides a structured test runner for smart contracts and chain operations,
//! supporting unit, integration, e2e, fuzz, property-based, benchmark, and
//! security test categories.

use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Runner-wide configuration.
#[derive(Debug, Clone)]
pub struct TestConfig {
    /// Per-test timeout in milliseconds.
    pub timeout_ms: u64,
    /// Gas limit for test execution.
    pub gas_limit: u64,
    /// Print verbose output.
    pub verbose: bool,
    /// Stop on first failure.
    pub fail_fast: bool,
    /// Run tests in parallel (currently advisory; real parallelism requires
    /// async or thread pool integration).
    pub parallel: bool,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            gas_limit: 10_000_000,
            verbose: false,
            fail_fast: false,
            parallel: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Test categories
// ---------------------------------------------------------------------------

/// Classification of a test case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TestCategory {
    Unit,
    Integration,
    E2E,
    Fuzz,
    Property,
    Benchmark,
    Security,
}

impl fmt::Display for TestCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestCategory::Unit => write!(f, "Unit"),
            TestCategory::Integration => write!(f, "Integration"),
            TestCategory::E2E => write!(f, "E2E"),
            TestCategory::Fuzz => write!(f, "Fuzz"),
            TestCategory::Property => write!(f, "Property"),
            TestCategory::Benchmark => write!(f, "Benchmark"),
            TestCategory::Security => write!(f, "Security"),
        }
    }
}

// ---------------------------------------------------------------------------
// Setup / teardown steps
// ---------------------------------------------------------------------------

/// Actions that can be performed during test setup or teardown.
#[derive(Debug, Clone)]
pub enum TestSetupStep {
    DeployContract {
        bytecode: Vec<u8>,
        constructor_args: Vec<u8>,
    },
    FundAccount {
        address: [u8; 32],
        amount: u64,
    },
    SetState {
        address: [u8; 32],
        key: [u8; 32],
        value: [u8; 32],
    },
    AdvanceBlock {
        count: u64,
    },
    CallContract {
        address: [u8; 32],
        data: Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Declarative assertions that the runner evaluates after execution.
#[derive(Debug, Clone)]
pub enum TestAssertion {
    BalanceEquals {
        address: [u8; 32],
        expected: u64,
    },
    StateEquals {
        address: [u8; 32],
        key: [u8; 32],
        expected: [u8; 32],
    },
    TxSucceeds {
        tx_data: Vec<u8>,
    },
    TxReverts {
        tx_data: Vec<u8>,
        error: String,
    },
    EventEmitted {
        topic: [u8; 32],
    },
    GasLessThan {
        max: u64,
    },
    Custom {
        name: String,
        check: String,
    },
}

// ---------------------------------------------------------------------------
// Test specification
// ---------------------------------------------------------------------------

/// Full specification of a single test case.
#[derive(Debug, Clone)]
pub struct TestSpec {
    pub name: String,
    pub description: String,
    pub category: TestCategory,
    pub setup: Vec<TestSetupStep>,
    pub assertions: Vec<TestAssertion>,
    pub teardown: Vec<TestSetupStep>,
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Outcome of a single test run.
#[derive(Debug, Clone)]
pub struct TestRunResult {
    pub test_name: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub gas_used: u64,
    pub error: Option<String>,
    pub assertions_passed: usize,
    pub assertions_total: usize,
}

/// Aggregate summary of a test suite run.
#[derive(Debug, Clone, Default)]
pub struct TestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub duration_ms: u64,
    pub total_gas: u64,
}

impl fmt::Display for TestSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} total | {} passed | {} failed | {} skipped | {}ms | {} gas",
            self.total, self.passed, self.failed, self.skipped, self.duration_ms, self.total_gas
        )
    }
}

// ---------------------------------------------------------------------------
// Fuzz testing types
// ---------------------------------------------------------------------------

/// Configuration for fuzz test campaigns.
#[derive(Debug, Clone)]
pub struct FuzzConfig {
    pub iterations: u32,
    pub seed: u64,
    pub max_input_size: usize,
}

impl Default for FuzzConfig {
    fn default() -> Self {
        Self {
            iterations: 1_000,
            seed: 42,
            max_input_size: 1024,
        }
    }
}

/// Result of a fuzz campaign.
#[derive(Debug, Clone)]
pub struct FuzzResult {
    pub iterations: u32,
    pub failures: Vec<(Vec<u8>, String)>,
    pub duration_ms: u64,
}

impl FuzzResult {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Property-based testing
// ---------------------------------------------------------------------------

/// Specification for a property-based test.
#[derive(Debug, Clone)]
pub struct PropertyTest {
    pub name: String,
    pub property: String,
    pub generator: String,
    pub iterations: u32,
}

// ---------------------------------------------------------------------------
// Test Runner
// ---------------------------------------------------------------------------

/// Orchestrates test execution, collecting results and producing summaries.
pub struct TestRunner {
    pub tests: Vec<TestSpec>,
    pub results: Vec<TestRunResult>,
    pub config: TestConfig,
    /// Simulated account balances for assertion evaluation.
    balances: HashMap<[u8; 32], u64>,
    /// Simulated state for assertion evaluation.
    state: HashMap<([u8; 32], [u8; 32]), [u8; 32]>,
    /// Simulated emitted events (topics).
    events: Vec<[u8; 32]>,
}

impl TestRunner {
    /// Create a new runner with the given configuration.
    pub fn new(config: TestConfig) -> Self {
        Self {
            tests: Vec::new(),
            results: Vec::new(),
            config,
            balances: HashMap::new(),
            state: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// Add a test specification to the suite.
    pub fn add_test(&mut self, spec: TestSpec) {
        self.tests.push(spec);
    }

    /// Execute a single test by name.
    pub fn run_test(&mut self, name: &str) -> Option<TestRunResult> {
        let spec = self.tests.iter().find(|t| t.name == name)?.clone();
        let result = self.execute_spec(&spec);
        self.results.push(result.clone());
        Some(result)
    }

    /// Execute all tests in the suite, respecting `fail_fast`.
    pub fn run_all(&mut self) -> TestSummary {
        let specs: Vec<TestSpec> = self.tests.clone();
        let start = Instant::now();

        for spec in &specs {
            let result = self.execute_spec(spec);
            let failed = !result.passed;
            self.results.push(result);
            if failed && self.config.fail_fast {
                break;
            }
        }

        let mut summary = self.summary();
        summary.duration_ms = start.elapsed().as_millis() as u64;
        summary
    }

    /// Run all tests in a given category.
    pub fn run_category(&mut self, category: TestCategory) -> Vec<TestRunResult> {
        let specs: Vec<TestSpec> = self
            .tests
            .iter()
            .filter(|t| t.category == category)
            .cloned()
            .collect();
        let mut results = Vec::new();
        for spec in &specs {
            let result = self.execute_spec(spec);
            self.results.push(result.clone());
            results.push(result);
        }
        results
    }

    /// Compute a summary from all collected results.
    pub fn summary(&self) -> TestSummary {
        let mut s = TestSummary::default();
        s.total = self.results.len();
        for r in &self.results {
            if r.passed {
                s.passed += 1;
            } else {
                s.failed += 1;
            }
            s.total_gas += r.gas_used;
            s.duration_ms += r.duration_ms;
        }
        s
    }

    // -----------------------------------------------------------------------
    // Internal execution
    // -----------------------------------------------------------------------

    fn execute_spec(&mut self, spec: &TestSpec) -> TestRunResult {
        let start = Instant::now();
        let mut gas_used: u64 = 0;
        let mut assertions_passed = 0;
        let assertions_total = spec.assertions.len();
        let mut error: Option<String> = None;

        // Setup phase.
        for step in &spec.setup {
            self.apply_setup(step);
            gas_used += 100; // symbolic gas cost per setup step
        }

        // Assertion phase.
        for assertion in &spec.assertions {
            match self.evaluate_assertion(assertion, &mut gas_used) {
                Ok(()) => assertions_passed += 1,
                Err(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                    if self.config.fail_fast {
                        break;
                    }
                }
            }
        }

        // Teardown phase.
        for step in &spec.teardown {
            self.apply_setup(step);
        }

        // Reset simulated state between tests.
        self.balances.clear();
        self.state.clear();
        self.events.clear();

        let duration_ms = start.elapsed().as_millis() as u64;
        let passed = assertions_passed == assertions_total;

        TestRunResult {
            test_name: spec.name.clone(),
            passed,
            duration_ms,
            gas_used,
            error,
            assertions_passed,
            assertions_total,
        }
    }

    fn apply_setup(&mut self, step: &TestSetupStep) {
        match step {
            TestSetupStep::FundAccount { address, amount } => {
                *self.balances.entry(*address).or_insert(0) += amount;
            }
            TestSetupStep::SetState {
                address,
                key,
                value,
            } => {
                self.state.insert((*address, *key), *value);
            }
            TestSetupStep::DeployContract { .. } => {
                // In a real implementation this would deploy to a test VM.
            }
            TestSetupStep::AdvanceBlock { .. } => {
                // Symbolic — advance the simulated block height.
            }
            TestSetupStep::CallContract { .. } => {
                // Symbolic call.
            }
        }
    }

    fn evaluate_assertion(
        &mut self,
        assertion: &TestAssertion,
        gas_used: &mut u64,
    ) -> Result<(), String> {
        *gas_used += 50; // symbolic gas per assertion
        match assertion {
            TestAssertion::BalanceEquals { address, expected } => {
                let actual = self.balances.get(address).copied().unwrap_or(0);
                if actual == *expected {
                    Ok(())
                } else {
                    Err(format!(
                        "balance mismatch: expected {}, got {}",
                        expected, actual
                    ))
                }
            }
            TestAssertion::StateEquals {
                address,
                key,
                expected,
            } => {
                let actual = self.state.get(&(*address, *key)).copied().unwrap_or([0u8; 32]);
                if actual == *expected {
                    Ok(())
                } else {
                    Err(format!("state mismatch at key {:?}", key))
                }
            }
            TestAssertion::TxSucceeds { .. } => {
                // Symbolic: assume success unless setup says otherwise.
                Ok(())
            }
            TestAssertion::TxReverts { error, .. } => {
                // Symbolic: we validate the error message is non-empty.
                if error.is_empty() {
                    Err("expected revert error but message is empty".to_string())
                } else {
                    Ok(())
                }
            }
            TestAssertion::EventEmitted { topic } => {
                if self.events.contains(topic) {
                    Ok(())
                } else {
                    Err(format!("event with topic {:?} not emitted", &topic[..4]))
                }
            }
            TestAssertion::GasLessThan { max } => {
                if *gas_used < *max {
                    Ok(())
                } else {
                    Err(format!("gas {} exceeds max {}", gas_used, max))
                }
            }
            TestAssertion::Custom { name, check } => {
                // Custom assertions are symbolic; pass if check is non-empty.
                if check.is_empty() {
                    Err(format!("custom assertion '{}' has empty check", name))
                } else {
                    Ok(())
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> TestConfig {
        TestConfig::default()
    }

    fn simple_spec(name: &str, category: TestCategory) -> TestSpec {
        TestSpec {
            name: name.to_string(),
            description: format!("Test: {}", name),
            category,
            setup: vec![],
            assertions: vec![],
            teardown: vec![],
        }
    }

    #[test]
    fn test_runner_empty_suite() {
        let mut runner = TestRunner::new(default_config());
        let summary = runner.run_all();
        assert_eq!(summary.total, 0);
        assert_eq!(summary.passed, 0);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn test_run_single_passing_test() {
        let mut runner = TestRunner::new(default_config());
        let spec = TestSpec {
            name: "balance_check".to_string(),
            description: "Verify funded balance".to_string(),
            category: TestCategory::Unit,
            setup: vec![TestSetupStep::FundAccount {
                address: [1u8; 32],
                amount: 1000,
            }],
            assertions: vec![TestAssertion::BalanceEquals {
                address: [1u8; 32],
                expected: 1000,
            }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("balance_check").unwrap();
        assert!(result.passed);
        assert_eq!(result.assertions_passed, 1);
        assert_eq!(result.assertions_total, 1);
    }

    #[test]
    fn test_run_single_failing_test() {
        let mut runner = TestRunner::new(default_config());
        let spec = TestSpec {
            name: "bad_balance".to_string(),
            description: "Balance won't match".to_string(),
            category: TestCategory::Unit,
            setup: vec![TestSetupStep::FundAccount {
                address: [1u8; 32],
                amount: 500,
            }],
            assertions: vec![TestAssertion::BalanceEquals {
                address: [1u8; 32],
                expected: 9999,
            }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("bad_balance").unwrap();
        assert!(!result.passed);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_run_all_collects_results() {
        let mut runner = TestRunner::new(default_config());
        runner.add_test(simple_spec("a", TestCategory::Unit));
        runner.add_test(simple_spec("b", TestCategory::Integration));
        runner.add_test(simple_spec("c", TestCategory::Unit));
        let summary = runner.run_all();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 3); // no assertions = pass
    }

    #[test]
    fn test_run_category_filters() {
        let mut runner = TestRunner::new(default_config());
        runner.add_test(simple_spec("u1", TestCategory::Unit));
        runner.add_test(simple_spec("i1", TestCategory::Integration));
        runner.add_test(simple_spec("u2", TestCategory::Unit));
        let results = runner.run_category(TestCategory::Unit);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.passed));
    }

    #[test]
    fn test_state_assertion() {
        let mut runner = TestRunner::new(default_config());
        let key = [0xAA; 32];
        let val = [0xBB; 32];
        let addr = [0x01; 32];
        let spec = TestSpec {
            name: "state_check".to_string(),
            description: "State set and verified".to_string(),
            category: TestCategory::Unit,
            setup: vec![TestSetupStep::SetState {
                address: addr,
                key,
                value: val,
            }],
            assertions: vec![TestAssertion::StateEquals {
                address: addr,
                key,
                expected: val,
            }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("state_check").unwrap();
        assert!(result.passed);
    }

    #[test]
    fn test_gas_less_than_assertion() {
        let mut runner = TestRunner::new(default_config());
        let spec = TestSpec {
            name: "gas_ok".to_string(),
            description: "Gas under limit".to_string(),
            category: TestCategory::Benchmark,
            setup: vec![],
            assertions: vec![TestAssertion::GasLessThan { max: 1_000_000 }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("gas_ok").unwrap();
        assert!(result.passed);
    }

    #[test]
    fn test_custom_assertion_pass() {
        let mut runner = TestRunner::new(default_config());
        let spec = TestSpec {
            name: "custom_ok".to_string(),
            description: "Custom passes".to_string(),
            category: TestCategory::Unit,
            setup: vec![],
            assertions: vec![TestAssertion::Custom {
                name: "my_check".to_string(),
                check: "x > 0".to_string(),
            }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("custom_ok").unwrap();
        assert!(result.passed);
    }

    #[test]
    fn test_custom_assertion_fail_empty_check() {
        let mut runner = TestRunner::new(default_config());
        let spec = TestSpec {
            name: "custom_fail".to_string(),
            description: "Custom fails".to_string(),
            category: TestCategory::Unit,
            setup: vec![],
            assertions: vec![TestAssertion::Custom {
                name: "bad".to_string(),
                check: "".to_string(),
            }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("custom_fail").unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn test_fail_fast_stops_early() {
        let mut config = default_config();
        config.fail_fast = true;
        let mut runner = TestRunner::new(config);

        // First test will fail.
        let failing = TestSpec {
            name: "will_fail".to_string(),
            description: "".to_string(),
            category: TestCategory::Unit,
            setup: vec![],
            assertions: vec![TestAssertion::BalanceEquals {
                address: [0; 32],
                expected: 999,
            }],
            teardown: vec![],
        };
        runner.add_test(failing);
        runner.add_test(simple_spec("should_not_run", TestCategory::Unit));

        let summary = runner.run_all();
        // Only the first test should have run.
        assert_eq!(summary.total, 1);
        assert_eq!(summary.failed, 1);
    }

    #[test]
    fn test_summary_display() {
        let s = TestSummary {
            total: 10,
            passed: 8,
            failed: 2,
            skipped: 0,
            duration_ms: 123,
            total_gas: 5000,
        };
        let display = format!("{}", s);
        assert!(display.contains("10 total"));
        assert!(display.contains("8 passed"));
        assert!(display.contains("2 failed"));
    }

    #[test]
    fn test_fuzz_result_passed() {
        let r = FuzzResult {
            iterations: 100,
            failures: vec![],
            duration_ms: 50,
        };
        assert!(r.passed());

        let r2 = FuzzResult {
            iterations: 100,
            failures: vec![(vec![1, 2], "overflow".to_string())],
            duration_ms: 50,
        };
        assert!(!r2.passed());
    }

    #[test]
    fn test_tx_reverts_assertion() {
        let mut runner = TestRunner::new(default_config());
        let spec = TestSpec {
            name: "revert_check".to_string(),
            description: "".to_string(),
            category: TestCategory::Unit,
            setup: vec![],
            assertions: vec![TestAssertion::TxReverts {
                tx_data: vec![0xFF],
                error: "insufficient balance".to_string(),
            }],
            teardown: vec![],
        };
        runner.add_test(spec);
        let result = runner.run_test("revert_check").unwrap();
        assert!(result.passed);
    }
}
