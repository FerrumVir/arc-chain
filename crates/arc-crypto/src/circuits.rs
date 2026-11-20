use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CircuitError {
    #[error("invalid wire index {0}: circuit has {1} wires")]
    InvalidWire(usize, usize),

    #[error("constraint violation at gate {index}: expected {expected}, got {got}")]
    ConstraintViolation {
        index: usize,
        expected: u64,
        got: u64,
    },

    #[error("missing input for wire {0}")]
    MissingInput(usize),

    #[error("arithmetic overflow at gate {0}")]
    ArithmeticOverflow(usize),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Gate {
    Add(usize, usize, usize),
    Mul(usize, usize, usize),
    Const(usize, u64),
    PublicInput(usize),
    Assert(usize, u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wire {
    pub id: usize,
    pub value: Option<u64>,
    pub is_public: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Circuit {
    pub id: [u8; 32],
    pub name: String,
    pub gates: Vec<Gate>,
    pub wires: Vec<Wire>,
    pub public_inputs: Vec<usize>,
    pub constraints: usize,
}

impl Circuit {
    fn compute_id(name: &str, gates: &[Gate], wire_count: usize) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arc-circuit-id");
        hasher.update(name.as_bytes());
        hasher.update(&(gates.len() as u64).to_le_bytes());
        hasher.update(&(wire_count as u64).to_le_bytes());
        for (i, gate) in gates.iter().enumerate() {
            hasher.update(&(i as u64).to_le_bytes());
            match gate {
                Gate::Add(a, b, c) => {
                    hasher.update(&[0u8]);
                    hasher.update(&(*a as u64).to_le_bytes());
                    hasher.update(&(*b as u64).to_le_bytes());
                    hasher.update(&(*c as u64).to_le_bytes());
                }
                Gate::Mul(a, b, c) => {
                    hasher.update(&[1u8]);
                    hasher.update(&(*a as u64).to_le_bytes());
                    hasher.update(&(*b as u64).to_le_bytes());
                    hasher.update(&(*c as u64).to_le_bytes());
                }
                Gate::Const(idx, val) => {
                    hasher.update(&[2u8]);
                    hasher.update(&(*idx as u64).to_le_bytes());
                    hasher.update(&val.to_le_bytes());
                }
                Gate::PublicInput(idx) => {
                    hasher.update(&[3u8]);
                    hasher.update(&(*idx as u64).to_le_bytes());
                }
                Gate::Assert(idx, val) => {
                    hasher.update(&[4u8]);
                    hasher.update(&(*idx as u64).to_le_bytes());
                    hasher.update(&val.to_le_bytes());
                }
            }
        }
        *hasher.finalize().as_bytes()
    }
}

pub struct CircuitBuilder {
    name: String,
    gates: Vec<Gate>,
    wire_count: usize,
    public_inputs: Vec<usize>,
}

impl CircuitBuilder {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            gates: Vec::new(),
            wire_count: 0,
            public_inputs: Vec::new(),
        }
    }

    fn alloc_wire(&mut self, is_public: bool) -> usize {
        let idx = self.wire_count;
        self.wire_count += 1;
        if is_public {
            self.public_inputs.push(idx);
        }
        idx
    }

    pub fn add_public_input(&mut self) -> usize {
        let idx = self.alloc_wire(true);
        self.gates.push(Gate::PublicInput(idx));
        idx
    }

    pub fn add_private_input(&mut self) -> usize {
        self.alloc_wire(false)
    }

    pub fn add(&mut self, a: usize, b: usize) -> usize {
        let out = self.alloc_wire(false);
        self.gates.push(Gate::Add(a, b, out));
        out
    }

    pub fn mul(&mut self, a: usize, b: usize) -> usize {
        let out = self.alloc_wire(false);
        self.gates.push(Gate::Mul(a, b, out));
        out
    }

    pub fn constant(&mut self, val: u64) -> usize {
        let out = self.alloc_wire(false);
        self.gates.push(Gate::Const(out, val));
        out
    }

    pub fn assert_equal(&mut self, wire: usize, val: u64) {
        self.gates.push(Gate::Assert(wire, val));
    }

    pub fn build(self) -> Circuit {
        let constraints = self
            .gates
            .iter()
            .filter(|g| matches!(g, Gate::Assert(..) | Gate::Mul(..)))
            .count();

        let wires: Vec<Wire> = (0..self.wire_count)
            .map(|i| Wire {
                id: i,
                value: None,
                is_public: self.public_inputs.contains(&i),
            })
            .collect();

        let id = Circuit::compute_id(&self.name, &self.gates, self.wire_count);

        Circuit {
            id,
            name: self.name,
            gates: self.gates,
            wires,
            public_inputs: self.public_inputs,
            constraints,
        }
    }
}

pub struct CircuitEvaluator;

impl CircuitEvaluator {
    pub fn evaluate(circuit: &Circuit, inputs: &[u64]) -> Result<Vec<u64>, CircuitError> {
        let wire_count = circuit.wires.len();
        let mut values: Vec<Option<u64>> = vec![None; wire_count];

        let mut produced = vec![false; wire_count];
        for gate in &circuit.gates {
            match gate {
                Gate::Add(_, _, out) | Gate::Mul(_, _, out) => produced[*out] = true,
                Gate::Const(idx, _) => produced[*idx] = true,
                Gate::PublicInput(_) | Gate::Assert(..) => {}
            }
        }

        let mut input_idx = 0;

        for gate in &circuit.gates {
            if let Gate::PublicInput(idx) = gate {
                if *idx >= wire_count {
                    return Err(CircuitError::InvalidWire(*idx, wire_count));
                }
                if input_idx >= inputs.len() {
                    return Err(CircuitError::MissingInput(*idx));
                }
                values[*idx] = Some(inputs[input_idx]);
                input_idx += 1;
            }
        }

        for i in 0..wire_count {
            if values[i].is_none() && !produced[i] && input_idx < inputs.len() {
                values[i] = Some(inputs[input_idx]);
                input_idx += 1;
            }
        }

        for (gate_idx, gate) in circuit.gates.iter().enumerate() {
            match gate {
                Gate::PublicInput(_) => {}
                Gate::Const(idx, val) => {
                    if *idx >= wire_count {
                        return Err(CircuitError::InvalidWire(*idx, wire_count));
                    }
                    values[*idx] = Some(*val);
                }
                Gate::Add(a, b, out) => {
                    if *a >= wire_count {
                        return Err(CircuitError::InvalidWire(*a, wire_count));
                    }
                    if *b >= wire_count {
                        return Err(CircuitError::InvalidWire(*b, wire_count));
                    }
                    if *out >= wire_count {
                        return Err(CircuitError::InvalidWire(*out, wire_count));
                    }
                    let va = values[*a].ok_or(CircuitError::MissingInput(*a))?;
                    let vb = values[*b].ok_or(CircuitError::MissingInput(*b))?;
                    let sum = va
                        .checked_add(vb)
                        .ok_or(CircuitError::ArithmeticOverflow(gate_idx))?;
                    values[*out] = Some(sum);
                }
                Gate::Mul(a, b, out) => {
                    if *a >= wire_count {
                        return Err(CircuitError::InvalidWire(*a, wire_count));
                    }
                    if *b >= wire_count {
                        return Err(CircuitError::InvalidWire(*b, wire_count));
                    }
                    if *out >= wire_count {
                        return Err(CircuitError::InvalidWire(*out, wire_count));
                    }
                    let va = values[*a].ok_or(CircuitError::MissingInput(*a))?;
                    let vb = values[*b].ok_or(CircuitError::MissingInput(*b))?;
                    let product = va
                        .checked_mul(vb)
                        .ok_or(CircuitError::ArithmeticOverflow(gate_idx))?;
                    values[*out] = Some(product);
                }
                Gate::Assert(idx, expected) => {
                    if *idx >= wire_count {
                        return Err(CircuitError::InvalidWire(*idx, wire_count));
                    }
                    let actual = values[*idx].ok_or(CircuitError::MissingInput(*idx))?;
                    if actual != *expected {
                        return Err(CircuitError::ConstraintViolation {
                            index: gate_idx,
                            expected: *expected,
                            got: actual,
                        });
                    }
                }
            }
        }

        Ok(values.into_iter().map(|v| v.unwrap_or(0)).collect())
    }

    pub fn verify_constraints(circuit: &Circuit, witness: &[u64]) -> bool {
        if witness.len() < circuit.wires.len() {
            return false;
        }

        for gate in &circuit.gates {
            match gate {
                Gate::Add(a, b, out) => {
                    if let Some(sum) = witness[*a].checked_add(witness[*b]) {
                        if sum != witness[*out] {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                Gate::Mul(a, b, out) => {
                    if let Some(product) = witness[*a].checked_mul(witness[*b]) {
                        if product != witness[*out] {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                Gate::Assert(idx, expected) => {
                    if witness[*idx] != *expected {
                        return false;
                    }
                }
                Gate::Const(idx, val) => {
                    if witness[*idx] != *val {
                        return false;
                    }
                }
                Gate::PublicInput(_) => {}
            }
        }

        true
    }
}

pub struct TransferCircuit;

impl TransferCircuit {
    pub fn build_transfer_circuit() -> Circuit {
        let mut builder = CircuitBuilder::new("transfer-verification");

        let _sender_bal = builder.add_public_input();
        let receiver_bal = builder.add_public_input();
        let amount = builder.add_public_input();

        let new_sender = builder.add_private_input();

        let _sum_check = builder.add(new_sender, amount);

        let _new_receiver = builder.add(receiver_bal, amount);

        builder.build()
    }

    pub fn verify_transfer(
        sender_balance: u64,
        receiver_balance: u64,
        amount: u64,
    ) -> Result<TransferResult, CircuitError> {
        if amount > sender_balance {
            return Err(CircuitError::ConstraintViolation {
                index: 0,
                expected: amount,
                got: sender_balance,
            });
        }

        let new_receiver = receiver_balance
            .checked_add(amount)
            .ok_or(CircuitError::ArithmeticOverflow(1))?;

        let new_sender = sender_balance - amount;

        let circuit = Self::build_transfer_circuit();
        let inputs = vec![sender_balance, receiver_balance, amount, new_sender];
        let witness = CircuitEvaluator::evaluate(&circuit, &inputs)?;

        if !CircuitEvaluator::verify_constraints(&circuit, &witness) {
            return Err(CircuitError::ConstraintViolation {
                index: 0,
                expected: 0,
                got: 1,
            });
        }

        Ok(TransferResult {
            new_sender_balance: new_sender,
            new_receiver_balance: new_receiver,
            witness,
            circuit_id: circuit.id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TransferResult {
    pub new_sender_balance: u64,
    pub new_receiver_balance: u64,
    pub witness: Vec<u64>,
    pub circuit_id: [u8; 32],
}

pub struct StateTransitionCircuit;

impl StateTransitionCircuit {
    pub fn build_state_transition_circuit(num_updates: usize) -> Circuit {
        let mut builder = CircuitBuilder::new("state-transition-verification");

        let mut old_root_wires = Vec::new();
        for _ in 0..4 {
            old_root_wires.push(builder.add_public_input());
        }
        let mut new_root_wires = Vec::new();
        for _ in 0..4 {
            new_root_wires.push(builder.add_public_input());
        }

        let mut _update_wires = Vec::new();
        for _ in 0..num_updates {
            let leaf_idx = builder.add_private_input();
            let old_val = builder.add_private_input();
            let new_val = builder.add_private_input();
            let update_hash = builder.mul(leaf_idx, old_val);
            let chained = builder.add(update_hash, new_val);
            _update_wires.push(chained);
        }

        builder.build()
    }

    pub fn verify_transition(
        old_root: &[u8; 32],
        new_root: &[u8; 32],
        updates: &[(u64, u64, u64)],
    ) -> Result<bool, CircuitError> {
        let circuit = Self::build_state_transition_circuit(updates.len());

        let mut inputs = Vec::new();
        for chunk in old_root.chunks(8) {
            let mut bytes = [0u8; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            inputs.push(u64::from_le_bytes(bytes));
        }
        for chunk in new_root.chunks(8) {
            let mut bytes = [0u8; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            inputs.push(u64::from_le_bytes(bytes));
        }

        for (leaf_idx, old_val, new_val) in updates {
            inputs.push(*leaf_idx);
            inputs.push(*old_val);
            inputs.push(*new_val);
        }

        let witness = CircuitEvaluator::evaluate(&circuit, &inputs)?;
        Ok(CircuitEvaluator::verify_constraints(&circuit, &witness))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_empty_circuit() {
        let circuit = CircuitBuilder::new("empty").build();
        assert_eq!(circuit.name, "empty");
        assert!(circuit.gates.is_empty());
        assert!(circuit.wires.is_empty());
        assert_eq!(circuit.constraints, 0);
    }

    #[test]
    fn test_builder_public_input() {
        let mut builder = CircuitBuilder::new("input-test");
        let w0 = builder.add_public_input();
        let w1 = builder.add_public_input();
        let circuit = builder.build();

        assert_eq!(w0, 0);
        assert_eq!(w1, 1);
        assert_eq!(circuit.public_inputs.len(), 2);
        assert!(circuit.wires[0].is_public);
        assert!(circuit.wires[1].is_public);
    }

    #[test]
    fn test_builder_add_gate() {
        let mut builder = CircuitBuilder::new("add-test");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let c = builder.add(a, b);
        let circuit = builder.build();

        assert_eq!(c, 2);
        assert_eq!(circuit.wires.len(), 3);
        assert_eq!(circuit.constraints, 0);
    }

    #[test]
    fn test_builder_mul_gate() {
        let mut builder = CircuitBuilder::new("mul-test");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let c = builder.mul(a, b);
        let circuit = builder.build();

        assert_eq!(c, 2);
        assert_eq!(circuit.constraints, 1);
    }

    #[test]
    fn test_evaluate_simple_add() {
        let mut builder = CircuitBuilder::new("eval-add");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let _c = builder.add(a, b);
        let circuit = builder.build();

        let witness = CircuitEvaluator::evaluate(&circuit, &[10, 20]).unwrap();
        assert_eq!(witness[0], 10);
        assert_eq!(witness[1], 20);
        assert_eq!(witness[2], 30);
    }

    #[test]
    fn test_evaluate_simple_mul() {
        let mut builder = CircuitBuilder::new("eval-mul");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let _c = builder.mul(a, b);
        let circuit = builder.build();

        let witness = CircuitEvaluator::evaluate(&circuit, &[7, 6]).unwrap();
        assert_eq!(witness[2], 42);
    }

    #[test]
    fn test_evaluate_constant() {
        let mut builder = CircuitBuilder::new("const-test");
        let a = builder.add_public_input();
        let c = builder.constant(100);
        let _sum = builder.add(a, c);
        let circuit = builder.build();

        let witness = CircuitEvaluator::evaluate(&circuit, &[50]).unwrap();
        assert_eq!(witness[0], 50);
        assert_eq!(witness[1], 100);
        assert_eq!(witness[2], 150);
    }

    #[test]
    fn test_evaluate_assert_pass() {
        let mut builder = CircuitBuilder::new("assert-pass");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let c = builder.add(a, b);
        builder.assert_equal(c, 30);
        let circuit = builder.build();

        let result = CircuitEvaluator::evaluate(&circuit, &[10, 20]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_evaluate_assert_fail() {
        let mut builder = CircuitBuilder::new("assert-fail");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let c = builder.add(a, b);
        builder.assert_equal(c, 99);
        let circuit = builder.build();

        let result = CircuitEvaluator::evaluate(&circuit, &[10, 20]);
        assert!(result.is_err());
        match result.unwrap_err() {
            CircuitError::ConstraintViolation { expected, got, .. } => {
                assert_eq!(expected, 99);
                assert_eq!(got, 30);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_verify_constraints_valid() {
        let mut builder = CircuitBuilder::new("verify-valid");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let _c = builder.mul(a, b);
        let circuit = builder.build();

        assert!(CircuitEvaluator::verify_constraints(&circuit, &[5, 6, 30]));
    }

    #[test]
    fn test_verify_constraints_invalid() {
        let mut builder = CircuitBuilder::new("verify-invalid");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let _c = builder.mul(a, b);
        let circuit = builder.build();

        assert!(!CircuitEvaluator::verify_constraints(&circuit, &[5, 6, 31]));
    }

    #[test]
    fn test_transfer_circuit_valid() {
        let result = TransferCircuit::verify_transfer(100, 50, 30).unwrap();
        assert_eq!(result.new_sender_balance, 70);
        assert_eq!(result.new_receiver_balance, 80);
    }

    #[test]
    fn test_transfer_circuit_insufficient_funds() {
        let result = TransferCircuit::verify_transfer(10, 50, 30);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CircuitError::ConstraintViolation { .. }
        ));
    }

    #[test]
    fn test_transfer_circuit_overflow() {
        let result = TransferCircuit::verify_transfer(100, u64::MAX, 1);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CircuitError::ArithmeticOverflow(_)
        ));
    }

    #[test]
    fn test_transfer_circuit_zero_amount() {
        let result = TransferCircuit::verify_transfer(100, 50, 0).unwrap();
        assert_eq!(result.new_sender_balance, 100);
        assert_eq!(result.new_receiver_balance, 50);
    }

    #[test]
    fn test_state_transition_circuit_build() {
        let circuit = StateTransitionCircuit::build_state_transition_circuit(3);
        assert_eq!(circuit.name, "state-transition-verification");
        assert_eq!(circuit.public_inputs.len(), 8);
    }

    #[test]
    fn test_state_transition_verify() {
        let old_root = [1u8; 32];
        let new_root = [2u8; 32];
        let updates = vec![(0, 100, 200), (1, 300, 400)];
        let result = StateTransitionCircuit::verify_transition(&old_root, &new_root, &updates);
        assert!(result.is_ok());
    }

    #[test]
    fn test_circuit_id_deterministic() {
        let c1 = CircuitBuilder::new("test").build();
        let c2 = CircuitBuilder::new("test").build();
        assert_eq!(c1.id, c2.id, "identical circuits must have identical IDs");
    }

    #[test]
    fn test_circuit_id_differs_by_name() {
        let c1 = CircuitBuilder::new("circuit-a").build();
        let c2 = CircuitBuilder::new("circuit-b").build();
        assert_ne!(
            c1.id, c2.id,
            "differently-named circuits must have different IDs"
        );
    }

    #[test]
    fn test_evaluate_overflow_add() {
        let mut builder = CircuitBuilder::new("overflow-add");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let _c = builder.add(a, b);
        let circuit = builder.build();

        let result = CircuitEvaluator::evaluate(&circuit, &[u64::MAX, 1]);
        assert!(matches!(result, Err(CircuitError::ArithmeticOverflow(_))));
    }

    #[test]
    fn test_evaluate_overflow_mul() {
        let mut builder = CircuitBuilder::new("overflow-mul");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let _c = builder.mul(a, b);
        let circuit = builder.build();

        let result = CircuitEvaluator::evaluate(&circuit, &[u64::MAX, 2]);
        assert!(matches!(result, Err(CircuitError::ArithmeticOverflow(_))));
    }

    #[test]
    fn test_chained_operations() {
        let mut builder = CircuitBuilder::new("chained");
        let a = builder.add_public_input();
        let b = builder.add_public_input();
        let c = builder.add_public_input();
        let sum = builder.add(a, b);
        let product = builder.mul(sum, c);
        builder.assert_equal(product, 150);
        let circuit = builder.build();

        let result = CircuitEvaluator::evaluate(&circuit, &[10, 20, 5]);
        assert!(result.is_ok());
        let witness = result.unwrap();
        assert_eq!(witness[3], 30);
        assert_eq!(witness[4], 150);
    }
}
