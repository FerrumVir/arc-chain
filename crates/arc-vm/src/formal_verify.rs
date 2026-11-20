use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationStatus {
    Unverified,
    Verified,
    Violated,
    Timeout,
    Unknown,
}

impl fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerificationStatus::Unverified => write!(f, "Unverified"),
            VerificationStatus::Verified => write!(f, "Verified"),
            VerificationStatus::Violated => write!(f, "Violated"),
            VerificationStatus::Timeout => write!(f, "Timeout"),
            VerificationStatus::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StateValue {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Address([u8; 32]),
    Array(Vec<StateValue>),
}

#[derive(Debug, Clone)]
pub struct StateTransition {
    pub name: String,
    pub preconditions: Vec<String>,
    pub postconditions: Vec<String>,
    pub action: String,
}

type UintCmp = dyn Fn(u64, u64) -> bool;

#[derive(Debug, Clone)]
pub struct ModelState {
    pub variables: HashMap<String, StateValue>,
    pub transitions: Vec<StateTransition>,
}

impl ModelState {
    pub fn new() -> Self {
        Self {
            variables: HashMap::new(),
            transitions: Vec::new(),
        }
    }

    pub fn evaluate(&self, expr: &str) -> Option<bool> {
        let expr = expr.trim();
        if expr == "true" {
            return Some(true);
        }
        if expr == "false" {
            return Some(false);
        }

        let operators: &[(&str, &UintCmp)] = &[
            (">=", &|a: u64, b: u64| a >= b),
            ("==", &|a: u64, b: u64| a == b),
            (">", &|a: u64, b: u64| a > b),
            ("<", &|a: u64, b: u64| a < b),
        ];
        for &(op, cmp_fn) in operators {
            if let Some((lhs, rhs)) = expr.split_once(op) {
                let var_name = lhs.trim();
                let literal = rhs.trim();

                if let Some(val) = self.variables.get(var_name) {
                    if op == "==" {
                        match (val, literal) {
                            (StateValue::Bool(b), "true") => return Some(*b),
                            (StateValue::Bool(b), "false") => return Some(!*b),
                            _ => {}
                        }
                    }
                    if let StateValue::Uint(v) = val
                        && let Ok(lit) = literal.parse::<u64>()
                    {
                        return Some(cmp_fn(*v, lit));
                    }
                    if let StateValue::Int(v) = val
                        && let Ok(lit) = literal.parse::<i64>()
                    {
                        return Some(match op {
                            ">=" => *v >= lit,
                            "==" => *v == lit,
                            ">" => *v > lit,
                            "<" => *v < lit,
                            _ => return None,
                        });
                    }
                }
            }
        }
        None
    }
}

impl Default for ModelState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct Invariant {
    pub name: String,
    pub expression: String,
    pub holds: Option<bool>,
    pub checked_at: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SafetyProperty {
    pub name: String,
    pub description: String,
    pub formula: String,
    pub status: VerificationStatus,
}

#[derive(Debug, Clone)]
pub struct LivenessProperty {
    pub name: String,
    pub description: String,
    pub formula: String,
    pub status: VerificationStatus,
}

#[derive(Debug, Clone)]
pub struct PropertyResult {
    pub property_name: String,
    pub satisfied: bool,
    pub counterexample: Option<CounterExample>,
}

#[derive(Debug, Clone)]
pub struct CounterExample {
    pub states: Vec<ModelState>,
    pub transitions: Vec<String>,
    pub description: String,
}

pub struct ModelChecker {
    invariants: Vec<Invariant>,
    safety_properties: Vec<SafetyProperty>,
    liveness_properties: Vec<LivenessProperty>,
}

impl ModelChecker {
    pub fn new() -> Self {
        Self {
            invariants: Vec::new(),
            safety_properties: Vec::new(),
            liveness_properties: Vec::new(),
        }
    }

    pub fn add_invariant(&mut self, inv: Invariant) {
        self.invariants.push(inv);
    }

    pub fn add_safety_property(&mut self, prop: SafetyProperty) {
        self.safety_properties.push(prop);
    }

    pub fn add_liveness_property(&mut self, prop: LivenessProperty) {
        self.liveness_properties.push(prop);
    }

    pub fn check_state(&mut self, state: &ModelState) -> Vec<PropertyResult> {
        let mut results = Vec::new();

        for inv in &mut self.invariants {
            let satisfied = state.evaluate(&inv.expression).unwrap_or(false);
            inv.holds = Some(satisfied);
            results.push(PropertyResult {
                property_name: inv.name.clone(),
                satisfied,
                counterexample: if satisfied {
                    None
                } else {
                    Some(CounterExample {
                        states: vec![state.clone()],
                        transitions: vec![],
                        description: format!(
                            "Invariant '{}' violated: {}",
                            inv.name, inv.expression
                        ),
                    })
                },
            });
        }

        for prop in &mut self.safety_properties {
            let satisfied = state.evaluate(&prop.formula).unwrap_or(false);
            prop.status = if satisfied {
                VerificationStatus::Verified
            } else {
                VerificationStatus::Violated
            };
            results.push(PropertyResult {
                property_name: prop.name.clone(),
                satisfied,
                counterexample: if satisfied {
                    None
                } else {
                    Some(CounterExample {
                        states: vec![state.clone()],
                        transitions: vec![],
                        description: format!(
                            "Safety property '{}' violated: {}",
                            prop.name, prop.formula
                        ),
                    })
                },
            });
        }

        for prop in &mut self.liveness_properties {
            let satisfied = state.evaluate(&prop.formula).unwrap_or(false);
            prop.status = if satisfied {
                VerificationStatus::Verified
            } else {
                VerificationStatus::Unknown
            };
            results.push(PropertyResult {
                property_name: prop.name.clone(),
                satisfied,
                counterexample: None,
            });
        }

        results
    }

    pub fn check_transition(
        &mut self,
        from: &ModelState,
        to: &ModelState,
        transition: &str,
    ) -> Vec<PropertyResult> {
        let mut results = Vec::new();

        if let Some(trans) = from.transitions.iter().find(|t| t.name == transition) {
            for pre in &trans.preconditions {
                let holds = from.evaluate(pre).unwrap_or(false);
                results.push(PropertyResult {
                    property_name: format!("precondition:{}", pre),
                    satisfied: holds,
                    counterexample: if holds {
                        None
                    } else {
                        Some(CounterExample {
                            states: vec![from.clone()],
                            transitions: vec![transition.to_string()],
                            description: format!(
                                "Precondition '{}' not met for transition '{}'",
                                pre, transition
                            ),
                        })
                    },
                });
            }

            for post in &trans.postconditions {
                let holds = to.evaluate(post).unwrap_or(false);
                results.push(PropertyResult {
                    property_name: format!("postcondition:{}", post),
                    satisfied: holds,
                    counterexample: if holds {
                        None
                    } else {
                        Some(CounterExample {
                            states: vec![from.clone(), to.clone()],
                            transitions: vec![transition.to_string()],
                            description: format!(
                                "Postcondition '{}' not met after transition '{}'",
                                post, transition
                            ),
                        })
                    },
                });
            }
        }

        let inv_results = self.check_state(to);
        results.extend(inv_results.into_iter().filter(|r| !r.satisfied));

        results
    }

    pub fn generate_counterexample(&self, property: &str) -> Option<CounterExample> {
        for inv in &self.invariants {
            if inv.name == property
                && let Some(false) = inv.holds
            {
                return Some(CounterExample {
                    states: vec![],
                    transitions: vec![],
                    description: format!(
                        "Invariant '{}' was violated. Expression: {}",
                        inv.name, inv.expression
                    ),
                });
            }
        }

        for prop in &self.safety_properties {
            if prop.name == property && prop.status == VerificationStatus::Violated {
                return Some(CounterExample {
                    states: vec![],
                    transitions: vec![],
                    description: format!(
                        "Safety property '{}' was violated. Formula: {}",
                        prop.name, prop.formula
                    ),
                });
            }
        }

        None
    }

    pub fn invariants(&self) -> &[Invariant] {
        &self.invariants
    }

    pub fn safety_properties(&self) -> &[SafetyProperty] {
        &self.safety_properties
    }

    pub fn liveness_properties(&self) -> &[LivenessProperty] {
        &self.liveness_properties
    }
}

impl Default for ModelChecker {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ConsensusModel;

impl ConsensusModel {
    pub fn build_bft_model(validators: usize, faulty: usize) -> ModelChecker {
        let mut checker = ModelChecker::new();

        let threshold = validators / 3;
        let _inv_holds = faulty < threshold || (validators >= 3 && 3 * faulty < validators);

        checker.add_invariant(Invariant {
            name: "bft_fault_tolerance".to_string(),
            expression: format!("faulty < {}", validators.div_ceil(3)),
            holds: Some(3 * faulty < validators),
            checked_at: Some(0),
        });

        checker.add_safety_property(SafetyProperty {
            name: "no_conflicting_commits".to_string(),
            description: format!(
                "No two honest validators commit conflicting blocks (n={}, f={})",
                validators, faulty
            ),
            formula: "conflicting_commits == 0".to_string(),
            status: if 3 * faulty < validators {
                VerificationStatus::Verified
            } else {
                VerificationStatus::Unknown
            },
        });

        let honest = validators - faulty;
        checker.add_liveness_property(LivenessProperty {
            name: "chain_progress".to_string(),
            description: format!(
                "Chain makes progress when > 2/3 validators are honest (honest={}, n={})",
                honest, validators
            ),
            formula: "committed_blocks > 0".to_string(),
            status: if 3 * honest > 2 * validators {
                VerificationStatus::Verified
            } else {
                VerificationStatus::Unknown
            },
        });

        checker.add_invariant(Invariant {
            name: "validator_count_consistent".to_string(),
            expression: format!("total_validators == {}", validators),
            holds: Some(true),
            checked_at: Some(0),
        });

        checker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(vars: Vec<(&str, StateValue)>) -> ModelState {
        let mut state = ModelState::new();
        for (k, v) in vars {
            state.variables.insert(k.to_string(), v);
        }
        state
    }

    #[test]
    fn test_model_state_evaluate_bool() {
        let state = make_state(vec![("active", StateValue::Bool(true))]);
        assert_eq!(state.evaluate("active == true"), Some(true));
        assert_eq!(state.evaluate("active == false"), Some(false));
    }

    #[test]
    fn test_model_state_evaluate_uint() {
        let state = make_state(vec![("balance", StateValue::Uint(100))]);
        assert_eq!(state.evaluate("balance == 100"), Some(true));
        assert_eq!(state.evaluate("balance > 50"), Some(true));
        assert_eq!(state.evaluate("balance < 50"), Some(false));
        assert_eq!(state.evaluate("balance >= 100"), Some(true));
    }

    #[test]
    fn test_model_state_evaluate_constants() {
        let state = ModelState::new();
        assert_eq!(state.evaluate("true"), Some(true));
        assert_eq!(state.evaluate("false"), Some(false));
    }

    #[test]
    fn test_invariant_holds() {
        let mut checker = ModelChecker::new();
        checker.add_invariant(Invariant {
            name: "positive_balance".to_string(),
            expression: "balance > 0".to_string(),
            holds: None,
            checked_at: None,
        });

        let state = make_state(vec![("balance", StateValue::Uint(42))]);
        let results = checker.check_state(&state);
        assert_eq!(results.len(), 1);
        assert!(results[0].satisfied);
        assert!(results[0].counterexample.is_none());
    }

    #[test]
    fn test_invariant_violated() {
        let mut checker = ModelChecker::new();
        checker.add_invariant(Invariant {
            name: "positive_balance".to_string(),
            expression: "balance > 0".to_string(),
            holds: None,
            checked_at: None,
        });

        let state = make_state(vec![("balance", StateValue::Uint(0))]);
        let results = checker.check_state(&state);
        assert!(!results[0].satisfied);
        assert!(results[0].counterexample.is_some());
    }

    #[test]
    fn test_safety_property() {
        let mut checker = ModelChecker::new();
        checker.add_safety_property(SafetyProperty {
            name: "no_negative".to_string(),
            description: "Balance never negative".to_string(),
            formula: "balance >= 0".to_string(),
            status: VerificationStatus::Unverified,
        });

        let state = make_state(vec![("balance", StateValue::Uint(10))]);
        let results = checker.check_state(&state);
        assert!(results[0].satisfied);
    }

    #[test]
    fn test_liveness_property() {
        let mut checker = ModelChecker::new();
        checker.add_liveness_property(LivenessProperty {
            name: "progress".to_string(),
            description: "Eventually commits".to_string(),
            formula: "committed_blocks > 0".to_string(),
            status: VerificationStatus::Unverified,
        });

        let state = make_state(vec![("committed_blocks", StateValue::Uint(5))]);
        let results = checker.check_state(&state);
        assert!(results[0].satisfied);
    }

    #[test]
    fn test_check_transition_preconditions() {
        let mut checker = ModelChecker::new();

        let mut from = make_state(vec![("balance", StateValue::Uint(100))]);
        from.transitions.push(StateTransition {
            name: "transfer".to_string(),
            preconditions: vec!["balance >= 50".to_string()],
            postconditions: vec!["balance == 50".to_string()],
            action: "transfer 50".to_string(),
        });

        let to = make_state(vec![("balance", StateValue::Uint(50))]);

        let results = checker.check_transition(&from, &to, "transfer");
        assert!(results.iter().all(|r| r.satisfied));
    }

    #[test]
    fn test_check_transition_postcondition_fails() {
        let mut checker = ModelChecker::new();

        let mut from = make_state(vec![("balance", StateValue::Uint(100))]);
        from.transitions.push(StateTransition {
            name: "transfer".to_string(),
            preconditions: vec!["balance >= 50".to_string()],
            postconditions: vec!["balance == 50".to_string()],
            action: "transfer 50".to_string(),
        });

        let to = make_state(vec![("balance", StateValue::Uint(70))]);

        let results = checker.check_transition(&from, &to, "transfer");
        let post_results: Vec<_> = results
            .iter()
            .filter(|r| r.property_name.starts_with("postcondition"))
            .collect();
        assert!(!post_results.is_empty());
        assert!(!post_results[0].satisfied);
    }

    #[test]
    fn test_generate_counterexample() {
        let mut checker = ModelChecker::new();
        checker.add_invariant(Invariant {
            name: "test_inv".to_string(),
            expression: "x > 0".to_string(),
            holds: None,
            checked_at: None,
        });

        let state = make_state(vec![("x", StateValue::Uint(0))]);
        checker.check_state(&state);

        let ce = checker.generate_counterexample("test_inv");
        assert!(ce.is_some());
        assert!(ce.unwrap().description.contains("test_inv"));
    }

    #[test]
    fn test_generate_counterexample_none_when_ok() {
        let mut checker = ModelChecker::new();
        checker.add_invariant(Invariant {
            name: "test_inv".to_string(),
            expression: "x > 0".to_string(),
            holds: None,
            checked_at: None,
        });

        let state = make_state(vec![("x", StateValue::Uint(5))]);
        checker.check_state(&state);

        assert!(checker.generate_counterexample("test_inv").is_none());
    }

    #[test]
    fn test_bft_model_safe() {
        let checker = ConsensusModel::build_bft_model(4, 1);
        assert_eq!(checker.invariants().len(), 2);
        assert!(checker.invariants()[0].holds.unwrap());
        assert_eq!(
            checker.safety_properties()[0].status,
            VerificationStatus::Verified
        );
        assert_eq!(
            checker.liveness_properties()[0].status,
            VerificationStatus::Verified
        );
    }

    #[test]
    fn test_bft_model_unsafe() {
        let checker = ConsensusModel::build_bft_model(4, 2);
        assert!(!checker.invariants()[0].holds.unwrap());
        assert_eq!(
            checker.safety_properties()[0].status,
            VerificationStatus::Unknown
        );
    }

    #[test]
    fn test_state_value_array() {
        let arr = StateValue::Array(vec![StateValue::Uint(1), StateValue::Bool(true)]);
        if let StateValue::Array(items) = arr {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected Array");
        }
    }
}
