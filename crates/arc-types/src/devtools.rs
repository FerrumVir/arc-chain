use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmTarget {
    Wasm,
    Evm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AbiType {
    Uint256,
    Int256,
    Address,
    Bool,
    String,
    Bytes,
    BytesN(u8),
    Array(Box<AbiType>),
    Tuple(Vec<AbiType>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AbiValue {
    Uint(u128),
    Int(i128),
    Address([u8; 32]),
    Bool(bool),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<AbiValue>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbiParam {
    pub name: String,
    pub param_type: AbiType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateMutability {
    Pure,
    View,
    NonPayable,
    Payable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbiFunction {
    pub name: String,
    pub inputs: Vec<AbiParam>,
    pub outputs: Vec<AbiParam>,
    pub state_mutability: StateMutability,
    pub selector: [u8; 4],
}

impl AbiFunction {
    pub fn new(
        name: &str,
        inputs: Vec<AbiParam>,
        outputs: Vec<AbiParam>,
        mutability: StateMutability,
    ) -> Self {
        let sig = Self::build_signature(name, &inputs);
        let selector = Self::compute_selector(&sig);
        Self {
            name: name.to_string(),
            inputs,
            outputs,
            state_mutability: mutability,
            selector,
        }
    }

    pub fn signature(&self) -> String {
        Self::build_signature(&self.name, &self.inputs)
    }

    pub fn compute_selector(signature: &str) -> [u8; 4] {
        let hash = blake3::hash(signature.as_bytes());
        let bytes = hash.as_bytes();
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }

    fn build_signature(name: &str, inputs: &[AbiParam]) -> String {
        let param_types: Vec<String> = inputs
            .iter()
            .map(|p| abi_type_string(&p.param_type))
            .collect();
        format!("{}({})", name, param_types.join(","))
    }
}

fn abi_type_string(t: &AbiType) -> String {
    match t {
        AbiType::Uint256 => "uint256".to_string(),
        AbiType::Int256 => "int256".to_string(),
        AbiType::Address => "address".to_string(),
        AbiType::Bool => "bool".to_string(),
        AbiType::String => "string".to_string(),
        AbiType::Bytes => "bytes".to_string(),
        AbiType::BytesN(n) => format!("bytes{}", n),
        AbiType::Array(inner) => format!("{}[]", abi_type_string(inner)),
        AbiType::Tuple(types) => {
            let inner: Vec<String> = types.iter().map(abi_type_string).collect();
            format!("({})", inner.join(","))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbiEventParam {
    pub name: String,
    pub param_type: AbiType,
    pub indexed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbiEvent {
    pub name: String,
    pub inputs: Vec<AbiEventParam>,
    pub topic0: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractAbi {
    pub functions: Vec<AbiFunction>,
    pub events: Vec<AbiEvent>,
    pub constructor: Option<AbiFunction>,
}

impl ContractAbi {
    pub fn new() -> Self {
        Self {
            functions: Vec::new(),
            events: Vec::new(),
            constructor: None,
        }
    }

    pub fn add_function(&mut self, func: AbiFunction) {
        self.functions.push(func);
    }

    pub fn add_event(&mut self, event: AbiEvent) {
        self.events.push(event);
    }

    pub fn get_function(&self, name: &str) -> Option<&AbiFunction> {
        self.functions.iter().find(|f| f.name == name)
    }

    pub fn get_function_by_selector(&self, selector: &[u8; 4]) -> Option<&AbiFunction> {
        self.functions.iter().find(|f| &f.selector == selector)
    }

    pub fn function_count(&self) -> usize {
        self.functions.len()
    }
}

impl Default for ContractAbi {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractManifest {
    pub name: String,
    pub version: String,
    pub vm_type: VmTarget,
    pub bytecode_hash: [u8; 32],
    pub bytecode_size: usize,
    pub abi: ContractAbi,
    pub constructor_args: Vec<AbiValue>,
    pub deployer: [u8; 32],
    pub deploy_gas: u64,
}

impl ContractManifest {
    pub fn new(name: String, vm_type: VmTarget, bytecode_hash: [u8; 32], size: usize) -> Self {
        Self {
            name,
            version: "0.1.0".to_string(),
            vm_type,
            bytecode_hash,
            bytecode_size: size,
            abi: ContractAbi::new(),
            constructor_args: Vec::new(),
            deployer: [0u8; 32],
            deploy_gas: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TestAction {
    DeployContract {
        manifest: Box<ContractManifest>,
        bytecode: Vec<u8>,
    },
    CallFunction {
        contract: [u8; 32],
        function: String,
        args: Vec<AbiValue>,
        value: u64,
    },
    Transfer {
        from: [u8; 32],
        to: [u8; 32],
        amount: u64,
    },
    SetBalance {
        address: [u8; 32],
        amount: u64,
    },
    AdvanceBlocks(u64),
    SetTimestamp(u64),
}

#[derive(Debug, Clone)]
pub enum TestAssertion {
    BalanceEquals {
        address: [u8; 32],
        expected: u64,
    },
    StorageEquals {
        contract: [u8; 32],
        key: [u8; 32],
        expected: Vec<u8>,
    },
    EventEmitted {
        contract: [u8; 32],
        topic0: [u8; 32],
    },
    TxSucceeded,
    TxReverted,
    ReturnValueEquals(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub passed: bool,
    pub gas_used: u64,
    pub elapsed_ms: u64,
    pub error: Option<String>,
    pub logs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TestCase {
    pub name: String,
    pub description: String,
    pub setup: Vec<TestAction>,
    pub actions: Vec<TestAction>,
    pub assertions: Vec<TestAssertion>,
    pub result: Option<TestResult>,
}

impl TestCase {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: String::new(),
            setup: Vec::new(),
            actions: Vec::new(),
            assertions: Vec::new(),
            result: None,
        }
    }

    pub fn add_action(&mut self, action: TestAction) {
        self.actions.push(action);
    }

    pub fn add_assertion(&mut self, assertion: TestAssertion) {
        self.assertions.push(assertion);
    }

    pub fn is_passed(&self) -> Option<bool> {
        self.result.as_ref().map(|r| r.passed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasProfile {
    pub function_name: String,
    pub total_gas: u64,
    pub computation_gas: u64,
    pub storage_gas: u64,
    pub call_depth: u32,
    pub sub_calls: Vec<GasProfile>,
}

impl GasProfile {
    pub fn new(function_name: &str, total_gas: u64) -> Self {
        Self {
            function_name: function_name.to_string(),
            total_gas,
            computation_gas: 0,
            storage_gas: 0,
            call_depth: 0,
            sub_calls: Vec::new(),
        }
    }

    pub fn total_with_subcalls(&self) -> u64 {
        let sub_gas: u64 = self.sub_calls.iter().map(|s| s.total_with_subcalls()).sum();
        self.total_gas + sub_gas
    }

    pub fn add_subcall(&mut self, profile: GasProfile) {
        self.sub_calls.push(profile);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStats {
    pub chain_id: u64,
    pub block_height: u64,
    pub tps_current: f64,
    pub tps_peak: f64,
    pub validator_count: u32,
    pub total_staked: u128,
    pub total_accounts: u64,
    pub total_transactions: u64,
    pub total_contracts: u64,
    pub avg_block_time_ms: u64,
    pub uptime_percentage: f64,
}

impl NetworkStats {
    pub fn default_stats() -> Self {
        Self {
            chain_id: 1,
            block_height: 0,
            tps_current: 0.0,
            tps_peak: 0.0,
            validator_count: 0,
            total_staked: 0,
            total_accounts: 0,
            total_transactions: 0,
            total_contracts: 0,
            avg_block_time_ms: 400,
            uptime_percentage: 100.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hash(n: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0] = n;
        h
    }

    #[test]
    fn test_contract_manifest_creation() {
        let hash = test_hash(0xAB);
        let manifest = ContractManifest::new("token".to_string(), VmTarget::Wasm, hash, 4096);

        assert_eq!(manifest.name, "token");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.vm_type, VmTarget::Wasm);
        assert_eq!(manifest.bytecode_hash, hash);
        assert_eq!(manifest.bytecode_size, 4096);
        assert_eq!(manifest.abi.function_count(), 0);
        assert!(manifest.constructor_args.is_empty());
        assert_eq!(manifest.deployer, [0u8; 32]);
        assert_eq!(manifest.deploy_gas, 0);
    }

    #[test]
    fn test_abi_add_function() {
        let mut abi = ContractAbi::new();
        assert_eq!(abi.function_count(), 0);

        let transfer = AbiFunction::new(
            "transfer",
            vec![
                AbiParam {
                    name: "to".to_string(),
                    param_type: AbiType::Address,
                },
                AbiParam {
                    name: "amount".to_string(),
                    param_type: AbiType::Uint256,
                },
            ],
            vec![AbiParam {
                name: "success".to_string(),
                param_type: AbiType::Bool,
            }],
            StateMutability::NonPayable,
        );

        abi.add_function(transfer);
        assert_eq!(abi.function_count(), 1);

        let found = abi.get_function("transfer");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "transfer");
        assert_eq!(found.unwrap().inputs.len(), 2);
        assert_eq!(found.unwrap().outputs.len(), 1);

        assert!(abi.get_function("approve").is_none());
    }

    #[test]
    fn test_abi_selector() {
        let sig = "transfer(address,uint256)";
        let selector = AbiFunction::compute_selector(sig);

        let hash = blake3::hash(sig.as_bytes());
        let expected = &hash.as_bytes()[0..4];
        assert_eq!(&selector, expected);
    }

    #[test]
    fn test_abi_get_by_selector() {
        let mut abi = ContractAbi::new();

        let transfer = AbiFunction::new(
            "transfer",
            vec![
                AbiParam {
                    name: "to".to_string(),
                    param_type: AbiType::Address,
                },
                AbiParam {
                    name: "amount".to_string(),
                    param_type: AbiType::Uint256,
                },
            ],
            vec![],
            StateMutability::NonPayable,
        );
        let selector = transfer.selector;
        abi.add_function(transfer);

        let balance_of = AbiFunction::new(
            "balanceOf",
            vec![AbiParam {
                name: "owner".to_string(),
                param_type: AbiType::Address,
            }],
            vec![AbiParam {
                name: "balance".to_string(),
                param_type: AbiType::Uint256,
            }],
            StateMutability::View,
        );
        abi.add_function(balance_of);

        let found = abi.get_function_by_selector(&selector);
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "transfer");

        let bad_selector = [0xFF, 0xFF, 0xFF, 0xFF];
        assert!(abi.get_function_by_selector(&bad_selector).is_none());
    }

    #[test]
    fn test_function_signature() {
        let func = AbiFunction::new(
            "transfer",
            vec![
                AbiParam {
                    name: "to".to_string(),
                    param_type: AbiType::Address,
                },
                AbiParam {
                    name: "amount".to_string(),
                    param_type: AbiType::Uint256,
                },
            ],
            vec![],
            StateMutability::NonPayable,
        );
        assert_eq!(func.signature(), "transfer(address,uint256)");

        let no_args = AbiFunction::new("totalSupply", vec![], vec![], StateMutability::View);
        assert_eq!(no_args.signature(), "totalSupply()");

        let complex = AbiFunction::new(
            "batchTransfer",
            vec![
                AbiParam {
                    name: "recipients".to_string(),
                    param_type: AbiType::Array(Box::new(AbiType::Address)),
                },
                AbiParam {
                    name: "data".to_string(),
                    param_type: AbiType::Tuple(vec![AbiType::Uint256, AbiType::Bool]),
                },
            ],
            vec![],
            StateMutability::NonPayable,
        );
        assert_eq!(
            complex.signature(),
            "batchTransfer(address[],(uint256,bool))"
        );
    }

    #[test]
    fn test_test_case_creation() {
        let mut tc = TestCase::new("token_transfer");
        assert_eq!(tc.name, "token_transfer");
        assert!(tc.is_passed().is_none());

        tc.add_action(TestAction::SetBalance {
            address: test_hash(1),
            amount: 1_000_000,
        });
        tc.add_action(TestAction::Transfer {
            from: test_hash(1),
            to: test_hash(2),
            amount: 500,
        });
        tc.add_assertion(TestAssertion::BalanceEquals {
            address: test_hash(2),
            expected: 500,
        });
        tc.add_assertion(TestAssertion::TxSucceeded);

        assert_eq!(tc.actions.len(), 2);
        assert_eq!(tc.assertions.len(), 2);

        tc.result = Some(TestResult {
            passed: true,
            gas_used: 21_000,
            elapsed_ms: 5,
            error: None,
            logs: vec!["Transfer executed".to_string()],
        });
        assert_eq!(tc.is_passed(), Some(true));
    }

    #[test]
    fn test_gas_profile_recursive() {
        let mut root = GasProfile::new("swap", 100);

        let mut inner = GasProfile::new("transferFrom", 50);
        let leaf = GasProfile::new("approve", 30);
        inner.add_subcall(leaf);

        root.add_subcall(inner);

        assert_eq!(root.total_with_subcalls(), 180);
        assert_eq!(root.sub_calls.len(), 1);

        assert_eq!(root.sub_calls[0].sub_calls[0].total_with_subcalls(), 30);
    }

    #[test]
    fn test_abi_types_complete() {
        let types = vec![
            AbiType::Uint256,
            AbiType::Int256,
            AbiType::Address,
            AbiType::Bool,
            AbiType::String,
            AbiType::Bytes,
            AbiType::BytesN(32),
            AbiType::Array(Box::new(AbiType::Uint256)),
            AbiType::Tuple(vec![AbiType::Address, AbiType::Bool]),
        ];

        for t in &types {
            let json = serde_json::to_string(t).expect("serialize AbiType");
            let _back: AbiType = serde_json::from_str(&json).expect("deserialize AbiType");
        }

        let values = vec![
            AbiValue::Uint(42),
            AbiValue::Int(-1),
            AbiValue::Address([0u8; 32]),
            AbiValue::Bool(true),
            AbiValue::String("hello".to_string()),
            AbiValue::Bytes(vec![0xDE, 0xAD]),
            AbiValue::Array(vec![AbiValue::Uint(1), AbiValue::Uint(2)]),
        ];

        for v in &values {
            let json = serde_json::to_string(v).expect("serialize AbiValue");
            let _back: AbiValue = serde_json::from_str(&json).expect("deserialize AbiValue");
        }
    }

    #[test]
    fn test_vm_target_variants() {
        let wasm = VmTarget::Wasm;
        let evm = VmTarget::Evm;

        assert_ne!(wasm, evm);
        assert_eq!(wasm, VmTarget::Wasm);
        assert_eq!(evm, VmTarget::Evm);

        let wasm_json = serde_json::to_string(&wasm).expect("serialize Wasm");
        let evm_json = serde_json::to_string(&evm).expect("serialize Evm");

        let wasm_back: VmTarget = serde_json::from_str(&wasm_json).expect("deserialize Wasm");
        let evm_back: VmTarget = serde_json::from_str(&evm_json).expect("deserialize Evm");

        assert_eq!(wasm_back, VmTarget::Wasm);
        assert_eq!(evm_back, VmTarget::Evm);
    }

    #[test]
    fn test_network_stats_defaults() {
        let stats = NetworkStats::default_stats();

        assert_eq!(stats.chain_id, 1);
        assert_eq!(stats.block_height, 0);
        assert!(stats.tps_current.abs() < f64::EPSILON);
        assert!(stats.tps_peak.abs() < f64::EPSILON);
        assert_eq!(stats.validator_count, 0);
        assert_eq!(stats.total_staked, 0);
        assert_eq!(stats.total_accounts, 0);
        assert_eq!(stats.total_transactions, 0);
        assert_eq!(stats.total_contracts, 0);
        assert_eq!(stats.avg_block_time_ms, 400);
        assert!((stats.uptime_percentage - 100.0).abs() < f64::EPSILON);
    }
}
