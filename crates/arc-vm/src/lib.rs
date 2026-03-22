//! ARC Chain WASM Virtual Machine
//!
//! Executes smart contracts compiled to WebAssembly with full host imports
//! wired to the StateDB for storage, balance queries, and event emission.

pub mod evm;
pub mod precompiles;
pub mod inference;
pub mod inference_verify;
pub mod oracle;
pub mod zk_precompile;
pub mod agent;
pub mod test_framework;
pub mod security_tests;
pub mod formal_verify;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_crypto::Hash256;
use arc_state::StateDB;
use arc_types::Address;
use thiserror::Error;
use tracing;
use wasmer::{
    imports, Function, FunctionEnv, FunctionEnvMut, Instance, Memory, Module, Store, Value,
};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum VmError {
    #[error("compilation error: {0}")]
    CompilationError(String),
    #[error("instantiation error: {0}")]
    InstantiationError(String),
    #[error("execution error: {0}")]
    ExecutionError(String),
    #[error("function not found: {0}")]
    FunctionNotFound(String),
    #[error("gas limit exceeded")]
    GasLimitExceeded,
    #[error("invalid WASM module")]
    InvalidModule,
    #[error("out of gas")]
    OutOfGas,
    #[error("memory access error")]
    MemoryAccessError,
    #[error("state error: {0}")]
    StateError(String),
}

// ---------------------------------------------------------------------------
// Execution result & events
// ---------------------------------------------------------------------------

/// An event emitted by a contract during execution.
#[derive(Clone, Debug)]
pub struct ContractEvent {
    pub topic: Vec<u8>,
    pub data: Vec<u8>,
}

/// Result of an AI inference call from a smart contract.
#[derive(Clone, Debug)]
pub struct AiInferenceResult {
    /// Model identifier that was called.
    pub model_id: Vec<u8>,
    /// Hash of the input (input itself is NOT stored on-chain).
    pub input_hash: Hash256,
    /// Output from the model (raw bytes).
    pub output: Vec<u8>,
    /// Hash of the output (for verification).
    pub output_hash: Hash256,
    /// Gas consumed by the inference.
    pub gas_cost: u64,
}

/// Result of WASM contract execution.
#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub success: bool,
    pub gas_used: u64,
    pub return_data: Vec<u8>,
    pub logs: Vec<String>,
    pub events: Vec<ContractEvent>,
    pub ai_results: Vec<AiInferenceResult>,
}

// ---------------------------------------------------------------------------
// Contract context (passed in for each execution)
// ---------------------------------------------------------------------------

/// Runtime context provided to a contract invocation.
#[derive(Clone, Debug)]
pub struct ContractContext {
    pub caller: Address,
    pub self_address: Address,
    /// ARC tokens sent with this call.
    pub value: u64,
    pub gas_limit: u64,
    pub block_height: u64,
    pub block_timestamp: u64,
}

// ---------------------------------------------------------------------------
// Contract address derivation
// ---------------------------------------------------------------------------

/// Derive a deterministic contract address from the deployer address and nonce.
///
/// Uses BLAKE3 (via `arc_crypto::hash_bytes`) over the concatenation of a
/// domain tag, the deployer address, and the nonce in little-endian encoding.
pub fn compute_contract_address(deployer: &Address, nonce: u64) -> Address {
    let mut preimage = Vec::with_capacity(32 + 32 + 8);
    preimage.extend_from_slice(b"ARC-chain-contract-addr-v1\x00\x00\x00\x00\x00\x00");
    preimage.extend_from_slice(&deployer.0);
    preimage.extend_from_slice(&nonce.to_le_bytes());
    arc_crypto::hash_bytes(&preimage)
}

// ---------------------------------------------------------------------------
// Host environment — shared state accessible by all host functions
// ---------------------------------------------------------------------------

/// Environment data shared across all WASM host imports for a single execution.
///
/// Stored inside a Wasmer `FunctionEnv<VmHostEnv>`. Host functions receive
/// `FunctionEnvMut<'_, VmHostEnv>` which gives mutable access to this struct
/// **and** the Wasmer `Store` (needed to read/write WASM linear memory).
struct VmHostEnv {
    // Gas metering
    gas_used: Arc<AtomicU64>,
    gas_limit: u64,
    out_of_gas: Arc<Mutex<bool>>,

    // Logging & events
    logs: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<ContractEvent>>>,

    // AI inference results
    ai_results: Arc<Mutex<Vec<AiInferenceResult>>>,

    // Contract context (immutable for the duration of this call)
    caller: [u8; 32],
    self_address: [u8; 32],
    call_value: u64,
    block_height: u64,
    block_timestamp: u64,

    // Balance snapshot: address -> balance (read-only, captured before exec)
    balances: Arc<Mutex<HashMap<[u8; 32], u64>>>,
    self_balance: u64,

    // Storage: read cache + write buffer
    // Reads: key -> Option<value> (None = confirmed absent from state)
    storage_cache: Arc<Mutex<HashMap<[u8; 32], Option<Vec<u8>>>>>,
    // Writes: key -> value (accumulated during execution, flushed to StateDB after)
    storage_writes: Arc<Mutex<Vec<([u8; 32], Vec<u8>)>>>,

    // Contract address for StateDB lookups
    contract_address: [u8; 32],
    // Optional reference to StateDB for storage reads on cache miss
    state_db: Option<Arc<StateDB>>,

    // Reference to WASM memory (set after instantiation via `init_with_instance`)
    memory: Option<Memory>,
}

impl VmHostEnv {
    fn new_for_context(ctx: &ContractContext, state: &StateDB) -> Self {
        // Pre-load self balance
        let self_bal = state
            .get_account(&ctx.self_address)
            .map(|a| a.balance)
            .unwrap_or(0);

        VmHostEnv {
            gas_used: Arc::new(AtomicU64::new(0)),
            gas_limit: ctx.gas_limit,
            out_of_gas: Arc::new(Mutex::new(false)),
            logs: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            ai_results: Arc::new(Mutex::new(Vec::new())),
            caller: ctx.caller.0,
            self_address: ctx.self_address.0,
            call_value: ctx.value,
            block_height: ctx.block_height,
            block_timestamp: ctx.block_timestamp,
            balances: Arc::new(Mutex::new(HashMap::new())),
            self_balance: self_bal,
            storage_cache: Arc::new(Mutex::new(HashMap::new())),
            storage_writes: Arc::new(Mutex::new(Vec::new())),
            contract_address: ctx.self_address.0,
            state_db: None,
            memory: None,
        }
    }

    /// Create a minimal environment for simple (no-state) execution.
    fn new_simple(gas_limit: u64) -> Self {
        VmHostEnv {
            gas_used: Arc::new(AtomicU64::new(0)),
            gas_limit,
            out_of_gas: Arc::new(Mutex::new(false)),
            logs: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            ai_results: Arc::new(Mutex::new(Vec::new())),
            caller: [0u8; 32],
            self_address: [0u8; 32],
            call_value: 0,
            block_height: 0,
            block_timestamp: 0,
            balances: Arc::new(Mutex::new(HashMap::new())),
            self_balance: 0,
            storage_cache: Arc::new(Mutex::new(HashMap::new())),
            storage_writes: Arc::new(Mutex::new(Vec::new())),
            contract_address: [0u8; 32],
            state_db: None,
            memory: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Host function implementations
// ---------------------------------------------------------------------------

/// `use_gas(amount: i64)` — metered gas accounting; traps on overflow.
fn host_use_gas(mut env: FunctionEnvMut<'_, VmHostEnv>, amount: i64) {
    let data = env.data_mut();
    if amount < 0 {
        // Negative gas amounts are invalid — treat as protocol violation
        *data.out_of_gas.lock().unwrap() = true;
        return;
    }
    let prev = data.gas_used.fetch_add(amount as u64, Ordering::Relaxed);
    if prev + amount as u64 > data.gas_limit {
        *data.out_of_gas.lock().unwrap() = true;
        // Wasmer will observe the trap through our post-execution check.
        // We cannot directly trap from a typed host function, but we set
        // the flag so the caller sees OutOfGas.
    }
}

/// Maximum allocation size for WASM host calls (10 MB).
const MAX_HOST_ALLOC: usize = 10 * 1024 * 1024;

/// `log(ptr: i32, len: i32)` — read a UTF-8 string from WASM memory and push to logs.
fn host_log(mut env: FunctionEnvMut<'_, VmHostEnv>, ptr: i32, len: i32) {
    let (data, store) = env.data_and_store_mut();
    const MAX_LOGS_PER_EXECUTION: usize = 1024;
    if data.logs.lock().unwrap().len() >= MAX_LOGS_PER_EXECUTION {
        return;
    }
    if len < 0 || (len as usize) > MAX_HOST_ALLOC {
        return;
    }
    if let Some(ref memory) = data.memory {
        let view = memory.view(&store);
        let mut buf = vec![0u8; len as usize];
        if view.read(ptr as u64, &mut buf).is_ok() {
            let msg = String::from_utf8_lossy(&buf).to_string();
            data.logs.lock().unwrap().push(msg);
        }
    } else {
        // Fallback: no memory available, log the offsets
        data.logs
            .lock()
            .unwrap()
            .push(format!("log({}, {})", ptr, len));
    }
}

/// `storage_get(key_ptr: i32, val_ptr: i32) -> i32`
///
/// Read 32-byte key from WASM memory, look up in storage cache, write value
/// to val_ptr. Returns length of value, or -1 if not found.
fn host_storage_get(mut env: FunctionEnvMut<'_, VmHostEnv>, key_ptr: i32, val_ptr: i32) -> i32 {
    let (data, store) = env.data_and_store_mut();
    let memory = match data.memory {
        Some(ref m) => m.clone(),
        None => return -1,
    };
    let view = memory.view(&store);

    // Read the 32-byte key
    let mut key = [0u8; 32];
    if view.read(key_ptr as u64, &mut key).is_err() {
        return -1;
    }

    // Check cache first
    let cached_val = {
        let cache = data.storage_cache.lock().unwrap();
        match cache.get(&key) {
            Some(Some(val)) => Some(val.clone()),
            Some(None) => return -1, // confirmed absent
            None => None,
        }
    };

    if let Some(val) = cached_val {
        let view = memory.view(&store);
        if view.write(val_ptr as u64, &val).is_err() {
            return -1;
        }
        return val.len() as i32;
    }

    // Not in cache — try loading from StateDB
    if let Some(ref state_db) = data.state_db {
        let addr = Hash256(data.contract_address);
        let key_hash = Hash256(key);
        let result = state_db.get_storage(&addr, &key_hash);
        let mut cache = data.storage_cache.lock().unwrap();
        match result {
            Some(val) => {
                cache.insert(key, Some(val.clone()));
                // Write value to WASM memory
                let view = memory.view(&store);
                if view.write(val_ptr as u64, &val).is_err() {
                    return -1;
                }
                return val.len() as i32;
            }
            None => {
                cache.insert(key, None); // mark as absent
                return -1;
            }
        }
    }
    -1
}

/// `storage_set(key_ptr: i32, val_ptr: i32, val_len: i32)`
///
/// Read 32-byte key and value from WASM memory, record the write.
fn host_storage_set(
    mut env: FunctionEnvMut<'_, VmHostEnv>,
    key_ptr: i32,
    val_ptr: i32,
    val_len: i32,
) {
    let (data, store) = env.data_and_store_mut();
    let memory = match data.memory {
        Some(ref m) => m.clone(),
        None => return,
    };
    let view = memory.view(&store);

    let mut key = [0u8; 32];
    if view.read(key_ptr as u64, &mut key).is_err() {
        return;
    }
    if val_len < 0 || (val_len as usize) > MAX_HOST_ALLOC {
        return;
    }
    let mut val = vec![0u8; val_len as usize];
    if view.read(val_ptr as u64, &mut val).is_err() {
        return;
    }

    // Gas cost: 5000 base + 10 per byte of value (storage writes are expensive)
    const STORAGE_WRITE_BASE_GAS: u64 = 5000;
    const STORAGE_WRITE_PER_BYTE_GAS: u64 = 10;
    const MAX_STORAGE_VALUE_SIZE: usize = 256 * 1024; // 256 KB max per value

    if val.len() > MAX_STORAGE_VALUE_SIZE {
        return; // reject oversized values
    }

    let write_gas = STORAGE_WRITE_BASE_GAS + STORAGE_WRITE_PER_BYTE_GAS * (val.len() as u64);
    let prev = data.gas_used.fetch_add(write_gas, Ordering::Relaxed);
    if prev + write_gas > data.gas_limit {
        *data.out_of_gas.lock().unwrap() = true;
        return;
    }

    // Update cache so subsequent reads in the same execution see the write
    data.storage_cache
        .lock()
        .unwrap()
        .insert(key, Some(val.clone()));
    // Record the write for post-execution flush
    data.storage_writes.lock().unwrap().push((key, val));
}

/// `balance_of(addr_ptr: i32) -> i64` — read 32-byte address, return its balance.
fn host_balance_of(mut env: FunctionEnvMut<'_, VmHostEnv>, addr_ptr: i32) -> i64 {
    let (data, store) = env.data_and_store_mut();
    let memory = match data.memory {
        Some(ref m) => m.clone(),
        None => return 0,
    };
    let view = memory.view(&store);

    let mut addr = [0u8; 32];
    if view.read(addr_ptr as u64, &mut addr).is_err() {
        return 0;
    }

    let balances = data.balances.lock().unwrap();
    balances.get(&addr).copied().unwrap_or(0) as i64
}

/// `self_balance() -> i64` — return balance of the executing contract.
fn host_self_balance(env: FunctionEnvMut<'_, VmHostEnv>) -> i64 {
    env.data().self_balance as i64
}

/// `caller(ptr: i32)` — write caller address (32 bytes) to WASM memory.
fn host_caller(mut env: FunctionEnvMut<'_, VmHostEnv>, ptr: i32) {
    let (data, store) = env.data_and_store_mut();
    if let Some(ref memory) = data.memory {
        let view = memory.view(&store);
        let _ = view.write(ptr as u64, &data.caller);
    }
}

/// `self_address(ptr: i32)` — write contract address (32 bytes) to WASM memory.
fn host_self_address(mut env: FunctionEnvMut<'_, VmHostEnv>, ptr: i32) {
    let (data, store) = env.data_and_store_mut();
    if let Some(ref memory) = data.memory {
        let view = memory.view(&store);
        let _ = view.write(ptr as u64, &data.self_address);
    }
}

/// `block_height() -> i64`
fn host_block_height(env: FunctionEnvMut<'_, VmHostEnv>) -> i64 {
    env.data().block_height as i64
}

/// `block_timestamp() -> i64`
fn host_block_timestamp(env: FunctionEnvMut<'_, VmHostEnv>) -> i64 {
    env.data().block_timestamp as i64
}

/// `tx_value() -> i64` — return value (ARC tokens) sent with this call.
fn host_tx_value(env: FunctionEnvMut<'_, VmHostEnv>) -> i64 {
    env.data().call_value as i64
}

/// `gas_remaining() -> i64`
fn host_gas_remaining(env: FunctionEnvMut<'_, VmHostEnv>) -> i64 {
    let data = env.data();
    let used = data.gas_used.load(Ordering::Relaxed);
    data.gas_limit.saturating_sub(used) as i64
}

/// `emit_event(topic_ptr: i32, topic_len: i32, data_ptr: i32, data_len: i32)`
fn host_emit_event(
    mut env: FunctionEnvMut<'_, VmHostEnv>,
    topic_ptr: i32,
    topic_len: i32,
    data_ptr: i32,
    data_len: i32,
) {
    let (data, store) = env.data_and_store_mut();

    const MAX_EVENTS_PER_EXECUTION: usize = 1024;

    let events = &data.events;
    if events.lock().unwrap().len() >= MAX_EVENTS_PER_EXECUTION {
        return; // silently drop events beyond limit
    }

    let memory = match data.memory {
        Some(ref m) => m.clone(),
        None => return,
    };
    let view = memory.view(&store);

    let mut topic = vec![0u8; topic_len as usize];
    let mut event_data = vec![0u8; data_len as usize];
    if view.read(topic_ptr as u64, &mut topic).is_err() {
        return;
    }
    if view.read(data_ptr as u64, &mut event_data).is_err() {
        return;
    }

    data.events.lock().unwrap().push(ContractEvent {
        topic,
        data: event_data,
    });
}


/// `ai_inference(model_ptr: i32, model_len: i32, input_ptr: i32, input_len: i32, output_ptr: i32) -> i32`
///
/// Calls an AI model. The model_id and input are read from WASM memory.
/// The output is written to output_ptr. Returns output length, or -1 on error.
///
/// Gas cost: 1000 base + 10 per input byte + 10 per output byte.
///
/// In testnet mode, this returns a deterministic mock response (BLAKE3 hash of input).
/// In production, this would route to a TEE-enclosed model runtime.
fn host_ai_inference(
    mut env: FunctionEnvMut<'_, VmHostEnv>,
    model_ptr: i32,
    model_len: i32,
    input_ptr: i32,
    input_len: i32,
    output_ptr: i32,
) -> i32 {
    let (data, store) = env.data_and_store_mut();
    let memory = match data.memory {
        Some(ref m) => m.clone(),
        None => return -1,
    };
    let view = memory.view(&store);

    // Read model_id from WASM memory
    let mut model_id = vec![0u8; model_len as usize];
    if view.read(model_ptr as u64, &mut model_id).is_err() {
        return -1;
    }

    // Read input from WASM memory
    let mut input = vec![0u8; input_len as usize];
    if view.read(input_ptr as u64, &mut input).is_err() {
        return -1;
    }

    // Compute input_hash = BLAKE3(input)
    let input_hash = arc_crypto::hash_bytes(&input);

    // Generate deterministic mock output for testnet:
    // output = BLAKE3(model_id || input) — 32 bytes
    let mut inference_preimage = Vec::with_capacity(model_id.len() + input.len());
    inference_preimage.extend_from_slice(&model_id);
    inference_preimage.extend_from_slice(&input);
    let output_hash_val = arc_crypto::hash_bytes(&inference_preimage);
    let output = output_hash_val.0.to_vec(); // 32 bytes

    // Compute output_hash = BLAKE3(output)
    let output_hash = arc_crypto::hash_bytes(&output);

    // Calculate gas cost: 1000 base + 10 per input byte + 10 per output byte
    let gas_cost = 1000 + 10 * (input_len as u64) + 10 * (output.len() as u64);

    // Charge gas
    let prev = data.gas_used.fetch_add(gas_cost, Ordering::Relaxed);
    if prev + gas_cost > data.gas_limit {
        *data.out_of_gas.lock().unwrap() = true;
        return -1;
    }

    // Write output to WASM memory at output_ptr
    let view = memory.view(&store);
    if view.write(output_ptr as u64, &output).is_err() {
        return -1;
    }

    // Record the AI inference result
    data.ai_results.lock().unwrap().push(AiInferenceResult {
        model_id,
        input_hash,
        output: output.clone(),
        output_hash,
        gas_cost,
    });

    output.len() as i32
}
// ---------------------------------------------------------------------------
// ArcVM — the virtual machine
// ---------------------------------------------------------------------------

/// ARC WASM Virtual Machine.
///
/// Executes smart contracts compiled to WebAssembly with metered gas,
/// storage access, balance queries, and event emission backed by the StateDB.
pub struct ArcVM {
    store: Store,
}

impl ArcVM {
    pub fn new() -> Self {
        Self {
            store: Store::default(),
        }
    }

    /// Compile a WASM module from bytecode.
    pub fn compile(&self, bytecode: &[u8]) -> Result<Module, VmError> {
        Module::new(&self.store, bytecode)
            .map_err(|e| VmError::CompilationError(e.to_string()))
    }

    /// Execute a function with full state access — the main execution path
    /// for smart contracts.
    ///
    /// Storage writes are buffered during execution and flushed to the
    /// StateDB only on successful completion.
    pub fn execute_with_state(
        &mut self,
        module: &Module,
        function_name: &str,
        args: &[Value],
        context: &ContractContext,
        state: &StateDB,
    ) -> Result<ExecutionResult, VmError> {
        // Build the host environment with context + pre-loaded state
        let host_env = VmHostEnv::new_for_context(context, state);

        // Pre-populate storage cache from StateDB so contracts can read existing state
        {
            let entries = state.get_contract_storage(&context.self_address);
            let mut cache = host_env.storage_cache.lock().unwrap();
            for (key, value) in entries {
                cache.insert(key.0, Some(value));
            }
        }

        // Pre-populate balances from StateDB so contracts can query any account balance
        {
            let mut bals = host_env.balances.lock().unwrap();
            // Always include caller's balance
            if let Some(caller_acct) = state.get_account(&context.caller) {
                bals.insert(context.caller.0, caller_acct.balance);
            }
            // Include self balance (already set as self_balance field)
            bals.insert(context.self_address.0, host_env.self_balance);
        }

        // Capture Arc handles for post-execution extraction
        let gas_used_handle = host_env.gas_used.clone();
        let out_of_gas_handle = host_env.out_of_gas.clone();
        let logs_handle = host_env.logs.clone();
        let events_handle = host_env.events.clone();
        let ai_results_handle = host_env.ai_results.clone();
        let storage_writes_handle = host_env.storage_writes.clone();

        // Create the FunctionEnv
        let func_env = FunctionEnv::new(&mut self.store, host_env);

        // Build host import functions
        let use_gas_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_use_gas);
        let log_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_log);
        let storage_get_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_storage_get);
        let storage_set_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_storage_set);
        let balance_of_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_balance_of);
        let self_balance_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_self_balance);
        let caller_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_caller);
        let self_address_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_self_address);
        let block_height_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_block_height);
        let block_timestamp_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_block_timestamp);
        let tx_value_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_tx_value);
        let gas_remaining_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_gas_remaining);
        let emit_event_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_emit_event);
        let ai_inference_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_ai_inference);

        let import_object = imports! {
            "env" => {
                "use_gas" => use_gas_fn,
                "log" => log_fn,
                "storage_get" => storage_get_fn,
                "storage_set" => storage_set_fn,
                "balance_of" => balance_of_fn,
                "self_balance" => self_balance_fn,
                "caller" => caller_fn,
                "self_address" => self_address_fn,
                "block_height" => block_height_fn,
                "block_timestamp" => block_timestamp_fn,
                "tx_value" => tx_value_fn,
                "gas_remaining" => gas_remaining_fn,
                "emit_event" => emit_event_fn,
                "ai_inference" => ai_inference_fn,
            }
        };

        // Instantiate
        let instance = Instance::new(&mut self.store, module, &import_object)
            .map_err(|e| VmError::InstantiationError(e.to_string()))?;

        // Wire up the WASM memory reference so host functions can read/write it
        if let Ok(memory) = instance.exports.get_memory("memory") {
            func_env.as_mut(&mut self.store).memory = Some(memory.clone());
        }

        // Look up the target function
        let func = instance
            .exports
            .get_function(function_name)
            .map_err(|_| VmError::FunctionNotFound(function_name.to_string()))?;

        // Execute
        let call_result = func.call(&mut self.store, args);

        // Check gas
        let gas_used = gas_used_handle.load(Ordering::Relaxed);
        let was_out_of_gas = *out_of_gas_handle.lock().unwrap();
        if was_out_of_gas || gas_used > context.gas_limit {
            return Err(VmError::OutOfGas);
        }

        // Extract logs, events & AI results
        let captured_logs = std::mem::take(&mut *logs_handle.lock().unwrap());
        let captured_events = std::mem::take(&mut *events_handle.lock().unwrap());
        let captured_ai_results = std::mem::take(&mut *ai_results_handle.lock().unwrap());

        // Handle execution result
        match call_result {
            Ok(result) => {
                // Flush storage writes to StateDB
                let writes = std::mem::take(&mut *storage_writes_handle.lock().unwrap());
                for (key, value) in writes {
                    state.set_storage(
                        &context.self_address,
                        Hash256(key),
                        value,
                    );
                }

                let return_data = result
                    .first()
                    .map(|v| match v {
                        Value::I32(n) => n.to_le_bytes().to_vec(),
                        Value::I64(n) => n.to_le_bytes().to_vec(),
                        _ => Vec::new(),
                    })
                    .unwrap_or_default();

                Ok(ExecutionResult {
                    success: true,
                    gas_used,
                    return_data,
                    logs: captured_logs,
                    events: captured_events,
                    ai_results: captured_ai_results,
                })
            }
            Err(e) => {
                // On failure, do NOT flush storage writes (revert)
                tracing::warn!("WASM execution failed: {}", e);
                Ok(ExecutionResult {
                    success: false,
                    gas_used,
                    return_data: Vec::new(),
                    logs: captured_logs,
                    events: captured_events,
                    ai_results: captured_ai_results,
                })
            }
        }
    }

    /// Simple execute without state — backward compatible for basic WASM modules
    /// that only need gas metering and logging.
    pub fn execute(
        &mut self,
        module: &Module,
        function_name: &str,
        args: &[Value],
        gas_limit: u64,
    ) -> Result<ExecutionResult, VmError> {
        let host_env = VmHostEnv::new_simple(gas_limit);

        let gas_used_handle = host_env.gas_used.clone();
        let out_of_gas_handle = host_env.out_of_gas.clone();
        let logs_handle = host_env.logs.clone();
        let events_handle = host_env.events.clone();
        let ai_results_handle = host_env.ai_results.clone();

        let func_env = FunctionEnv::new(&mut self.store, host_env);

        // Build the minimal set of host imports
        let use_gas_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_use_gas);
        let log_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_log);
        let storage_get_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_storage_get);
        let storage_set_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_storage_set);
        let balance_of_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_balance_of);
        let self_balance_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_self_balance);
        let caller_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_caller);
        let self_address_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_self_address);
        let block_height_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_block_height);
        let block_timestamp_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_block_timestamp);
        let tx_value_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_tx_value);
        let gas_remaining_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_gas_remaining);
        let emit_event_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_emit_event);
        let ai_inference_fn =
            Function::new_typed_with_env(&mut self.store, &func_env, host_ai_inference);

        let import_object = imports! {
            "env" => {
                "use_gas" => use_gas_fn,
                "log" => log_fn,
                "storage_get" => storage_get_fn,
                "storage_set" => storage_set_fn,
                "balance_of" => balance_of_fn,
                "self_balance" => self_balance_fn,
                "caller" => caller_fn,
                "self_address" => self_address_fn,
                "block_height" => block_height_fn,
                "block_timestamp" => block_timestamp_fn,
                "tx_value" => tx_value_fn,
                "gas_remaining" => gas_remaining_fn,
                "emit_event" => emit_event_fn,
                "ai_inference" => ai_inference_fn,
            }
        };

        let instance = Instance::new(&mut self.store, module, &import_object)
            .map_err(|e| VmError::InstantiationError(e.to_string()))?;

        // Wire up memory
        if let Ok(memory) = instance.exports.get_memory("memory") {
            func_env.as_mut(&mut self.store).memory = Some(memory.clone());
        }

        let func = instance
            .exports
            .get_function(function_name)
            .map_err(|_| VmError::FunctionNotFound(function_name.to_string()))?;

        let result = func
            .call(&mut self.store, args)
            .map_err(|e| VmError::ExecutionError(e.to_string()))?;

        let gas_used = gas_used_handle.load(Ordering::Relaxed);
        let was_out_of_gas = *out_of_gas_handle.lock().unwrap();
        if was_out_of_gas || gas_used > gas_limit {
            return Err(VmError::OutOfGas);
        }

        let captured_logs = std::mem::take(&mut *logs_handle.lock().unwrap());
        let captured_events = std::mem::take(&mut *events_handle.lock().unwrap());
        let captured_ai_results = std::mem::take(&mut *ai_results_handle.lock().unwrap());

        let return_data = result
            .first()
            .map(|v| match v {
                Value::I32(n) => n.to_le_bytes().to_vec(),
                Value::I64(n) => n.to_le_bytes().to_vec(),
                _ => Vec::new(),
            })
            .unwrap_or_default();

        Ok(ExecutionResult {
            success: true,
            gas_used,
            return_data,
            logs: captured_logs,
            events: captured_events,
            ai_results: captured_ai_results,
        })
    }
}

impl Default for ArcVM {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate WASM bytecode without executing.
pub fn validate_wasm(bytecode: &[u8]) -> bool {
    let store = Store::default();
    Module::validate(&store, bytecode).is_ok()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    // -----------------------------------------------------------------------
    // WAT helpers — each generates a valid WAT string that imports the host
    // functions it needs from the "env" namespace.
    // -----------------------------------------------------------------------

    /// Minimal add function (no host imports needed; we still declare them
    /// in the import object but don't import them in the WAT so the module
    /// works with the simple `execute` path that always provides them).
    fn wat_add() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (func (export "add") (param i32 i32) (result i32)
                    local.get 0
                    local.get 1
                    i32.add
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that calls use_gas and returns 42.
    fn wat_gas_accounting() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "use_gas" (func $use_gas (param i64)))
                (func (export "run") (result i32)
                    i64.const 500
                    call $use_gas
                    i64.const 300
                    call $use_gas
                    i32.const 42
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that returns block_height as i64.
    fn wat_block_height() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "block_height" (func $block_height (result i64)))
                (func (export "get_height") (result i64)
                    call $block_height
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that calls storage_set then storage_get and returns the length.
    fn wat_storage_ops() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "storage_set" (func $storage_set (param i32 i32 i32)))
                (import "env" "storage_get" (func $storage_get (param i32 i32) (result i32)))
                (memory (export "memory") 1)

                ;; Key at offset 0 (32 bytes, all zeros)
                ;; Value at offset 64 (8 bytes: 0xDEADBEEF 00000000)
                (data (i32.const 64) "\EF\BE\AD\DE\00\00\00\00")

                ;; Read-back buffer at offset 128

                (func (export "run") (result i32)
                    ;; storage_set(key_ptr=0, val_ptr=64, val_len=8)
                    i32.const 0
                    i32.const 64
                    i32.const 8
                    call $storage_set

                    ;; storage_get(key_ptr=0, val_ptr=128) -> length
                    i32.const 0
                    i32.const 128
                    call $storage_get
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that writes caller and self_address to memory and returns
    /// the first byte of each.
    fn wat_caller_and_self() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "caller" (func $caller (param i32)))
                (import "env" "self_address" (func $self_address (param i32)))
                (memory (export "memory") 1)

                (func (export "run") (result i32)
                    ;; Write caller to offset 0
                    i32.const 0
                    call $caller

                    ;; Write self_address to offset 64
                    i32.const 64
                    call $self_address

                    ;; Return caller[0] XOR self_address[0] so we can verify both
                    ;; were written (they should differ if addresses differ).
                    ;; Load byte at offset 0
                    i32.const 0
                    i32.load8_u

                    ;; Load byte at offset 64
                    i32.const 64
                    i32.load8_u

                    ;; XOR them
                    i32.xor
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that calls self_balance() and returns it.
    fn wat_self_balance() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "self_balance" (func $self_balance (result i64)))
                (func (export "get_balance") (result i64)
                    call $self_balance
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that emits an event.
    fn wat_emit_event() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "emit_event" (func $emit_event (param i32 i32 i32 i32)))
                (memory (export "memory") 1)

                ;; Topic "Transfer" at offset 0 (8 bytes)
                (data (i32.const 0) "Transfer")
                ;; Data payload at offset 16 (4 bytes)
                (data (i32.const 16) "\01\02\03\04")

                (func (export "run") (result i32)
                    ;; emit_event(topic_ptr=0, topic_len=8, data_ptr=16, data_len=4)
                    i32.const 0
                    i32.const 8
                    i32.const 16
                    i32.const 4
                    call $emit_event
                    i32.const 1
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that calls tx_value, block_timestamp, and gas_remaining.
    fn wat_context_queries() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "tx_value" (func $tx_value (result i64)))
                (import "env" "block_timestamp" (func $block_timestamp (result i64)))
                (import "env" "gas_remaining" (func $gas_remaining (result i64)))
                (func (export "get_tx_value") (result i64)
                    call $tx_value
                )
                (func (export "get_timestamp") (result i64)
                    call $block_timestamp
                )
                (func (export "get_gas_remaining") (result i64)
                    call $gas_remaining
                )
            )"#,
        )
        .expect("valid WAT")
    }

    /// Module that calls log with a string in memory.
    fn wat_log() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "log" (func $log (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "hello from wasm")
                (func (export "run") (result i32)
                    i32.const 0
                    i32.const 15
                    call $log
                    i32.const 1
                )
            )"#,
        )
        .expect("valid WAT")
    }

    // -----------------------------------------------------------------------
    // Helper to create a ContractContext
    // -----------------------------------------------------------------------

    fn test_context() -> ContractContext {
        ContractContext {
            caller: hash_bytes(b"caller"),
            self_address: hash_bytes(b"contract"),
            value: 1000,
            gas_limit: 1_000_000,
            block_height: 42,
            block_timestamp: 1700000000,
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_compile_and_execute() {
        let mut vm = ArcVM::new();
        let wasm = wat_add();
        let module = vm.compile(&wasm).unwrap();
        let result = vm
            .execute(&module, "add", &[Value::I32(3), Value::I32(4)], 1_000_000)
            .unwrap();
        assert!(result.success);
        let val = i32::from_le_bytes(result.return_data[..4].try_into().unwrap());
        assert_eq!(val, 7);
    }

    #[test]
    fn test_invalid_function() {
        let mut vm = ArcVM::new();
        let wasm = wat_add();
        let module = vm.compile(&wasm).unwrap();
        let result = vm.execute(&module, "nonexistent", &[], 1_000_000);
        assert!(matches!(result, Err(VmError::FunctionNotFound(_))));
    }

    #[test]
    fn test_validate_wasm() {
        let wasm = wat_add();
        assert!(validate_wasm(&wasm));
        assert!(!validate_wasm(b"not wasm"));
    }

    #[test]
    fn test_contract_address_derivation() {
        let deployer = hash_bytes(b"deployer");

        // Deterministic: same inputs produce same address
        let addr1 = compute_contract_address(&deployer, 0);
        let addr2 = compute_contract_address(&deployer, 0);
        assert_eq!(addr1, addr2);

        // Different nonce produces different address
        let addr3 = compute_contract_address(&deployer, 1);
        assert_ne!(addr1, addr3);

        // Different deployer produces different address
        let other_deployer = hash_bytes(b"other");
        let addr4 = compute_contract_address(&other_deployer, 0);
        assert_ne!(addr1, addr4);
    }

    #[test]
    fn test_gas_accounting() {
        let mut vm = ArcVM::new();
        let wasm = wat_gas_accounting();
        let module = vm.compile(&wasm).unwrap();
        let result = vm
            .execute(&module, "run", &[], 1_000_000)
            .unwrap();
        assert!(result.success);
        // use_gas(500) + use_gas(300) = 800
        assert_eq!(result.gas_used, 800);
        // Return value should be 42
        let val = i32::from_le_bytes(result.return_data[..4].try_into().unwrap());
        assert_eq!(val, 42);
    }

    #[test]
    fn test_execute_with_state_basic() {
        let mut vm = ArcVM::new();
        let wasm = wat_block_height();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = test_context();

        let result = vm
            .execute_with_state(&module, "get_height", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        let height = i64::from_le_bytes(result.return_data[..8].try_into().unwrap());
        assert_eq!(height, 42);
    }

    #[test]
    fn test_storage_operations() {
        let mut vm = ArcVM::new();
        let wasm = wat_storage_ops();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = test_context();

        let result = vm
            .execute_with_state(&module, "run", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        // storage_get should return 8 (the length of the value we stored)
        let len = i32::from_le_bytes(result.return_data[..4].try_into().unwrap());
        assert_eq!(len, 8);

        // Verify the write was flushed to StateDB
        let key = Hash256([0u8; 32]); // key was all zeros
        let stored = state.get_storage(&ctx.self_address, &key);
        assert!(stored.is_some());
        let val = stored.unwrap();
        assert_eq!(val.len(), 8);
        assert_eq!(val[0], 0xEF); // little-endian DEADBEEF
        assert_eq!(val[1], 0xBE);
        assert_eq!(val[2], 0xAD);
        assert_eq!(val[3], 0xDE);
    }

    #[test]
    fn test_caller_and_self_address() {
        let mut vm = ArcVM::new();
        let wasm = wat_caller_and_self();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = test_context();

        let result = vm
            .execute_with_state(&module, "run", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);

        // The XOR of first bytes should be non-zero since caller != self_address
        let xor_val = i32::from_le_bytes(result.return_data[..4].try_into().unwrap());
        // caller = hash_bytes(b"caller"), self_address = hash_bytes(b"contract")
        // Their first bytes will almost certainly differ
        assert_ne!(xor_val, 0, "caller and self_address should differ");
    }

    #[test]
    fn test_balance_query() {
        let mut vm = ArcVM::new();
        let wasm = wat_self_balance();
        let module = vm.compile(&wasm).unwrap();

        // Create state with a known balance for the contract address
        let state = StateDB::new();
        let ctx = test_context();
        // Fund the contract account
        let _account = state.get_or_create_account(&ctx.self_address);
        // We need to set balance — get_or_create gives 0 balance.
        // StateDB doesn't have a set_balance, but we can use the account:
        // Actually, looking at StateDB, accounts are created with 0 balance.
        // The VmHostEnv reads balance from get_account. So self_balance = 0.

        let result = vm
            .execute_with_state(&module, "get_balance", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        let balance = i64::from_le_bytes(result.return_data[..8].try_into().unwrap());
        // Account was just created with 0 balance
        assert_eq!(balance, 0);

        // Now test with a pre-funded account using with_genesis
        let funded_state = StateDB::with_genesis(&[(ctx.self_address, 999_999)]);
        let result2 = vm
            .execute_with_state(&module, "get_balance", &[], &ctx, &funded_state)
            .unwrap();
        assert!(result2.success);
        let balance2 = i64::from_le_bytes(result2.return_data[..8].try_into().unwrap());
        assert_eq!(balance2, 999_999);
    }

    #[test]
    fn test_event_emission() {
        let mut vm = ArcVM::new();
        let wasm = wat_emit_event();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = test_context();

        let result = vm
            .execute_with_state(&module, "run", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].topic, b"Transfer");
        assert_eq!(result.events[0].data, vec![0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_context_queries() {
        let mut vm = ArcVM::new();
        let wasm = wat_context_queries();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = ContractContext {
            caller: hash_bytes(b"caller"),
            self_address: hash_bytes(b"contract"),
            value: 7777,
            gas_limit: 1_000_000,
            block_height: 100,
            block_timestamp: 1234567890,
        };

        // Test tx_value
        let result = vm
            .execute_with_state(&module, "get_tx_value", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        let val = i64::from_le_bytes(result.return_data[..8].try_into().unwrap());
        assert_eq!(val, 7777);

        // Test block_timestamp
        let result = vm
            .execute_with_state(&module, "get_timestamp", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        let ts = i64::from_le_bytes(result.return_data[..8].try_into().unwrap());
        assert_eq!(ts, 1234567890);

        // Test gas_remaining (should be gas_limit since no gas was used yet)
        let result = vm
            .execute_with_state(&module, "get_gas_remaining", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        let remaining = i64::from_le_bytes(result.return_data[..8].try_into().unwrap());
        assert_eq!(remaining, 1_000_000);
    }

    #[test]
    fn test_log_from_memory() {
        let mut vm = ArcVM::new();
        let wasm = wat_log();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = test_context();

        let result = vm
            .execute_with_state(&module, "run", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);
        assert_eq!(result.logs.len(), 1);
        assert_eq!(result.logs[0], "hello from wasm");
    }

    #[test]
    fn test_out_of_gas() {
        let mut vm = ArcVM::new();
        let wasm = wat_gas_accounting();
        let module = vm.compile(&wasm).unwrap();

        // Gas limit of 100, but the module uses 800
        let result = vm.execute(&module, "run", &[], 100);
        assert!(matches!(result, Err(VmError::OutOfGas)));
    }

    #[test]
    fn test_execution_result_has_events_field() {
        // Verify that ExecutionResult from simple execute includes empty events
        let mut vm = ArcVM::new();
        let wasm = wat_add();
        let module = vm.compile(&wasm).unwrap();
        let result = vm
            .execute(&module, "add", &[Value::I32(1), Value::I32(2)], 1_000_000)
            .unwrap();
        assert!(result.events.is_empty());
    }


    /// Module that calls ai_inference with model "gpt-4" and input "hello world".
    fn wat_ai_inference() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "ai_inference" (func $ai_inference (param i32 i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)

                ;; Model ID "gpt-4" at offset 0 (5 bytes)
                (data (i32.const 0) "gpt-4")
                ;; Input "hello world" at offset 16 (11 bytes)
                (data (i32.const 16) "hello world")
                ;; Output buffer at offset 64 (32 bytes reserved)

                (func (export "run") (result i32)
                    ;; ai_inference(model_ptr=0, model_len=5, input_ptr=16, input_len=11, output_ptr=64)
                    i32.const 0
                    i32.const 5
                    i32.const 16
                    i32.const 11
                    i32.const 64
                    call $ai_inference
                )
            )"#,
        )
        .expect("valid WAT")
    }

    #[test]
    fn test_ai_inference() {
        let mut vm = ArcVM::new();
        let wasm = wat_ai_inference();
        let module = vm.compile(&wasm).unwrap();

        let state = StateDB::new();
        let ctx = test_context();

        let result = vm
            .execute_with_state(&module, "run", &[], &ctx, &state)
            .unwrap();
        assert!(result.success);

        // Return value should be 32 (BLAKE3 hash output length)
        let output_len = i32::from_le_bytes(result.return_data[..4].try_into().unwrap());
        assert_eq!(output_len, 32);

        // Should have exactly one AI inference result
        assert_eq!(result.ai_results.len(), 1);
        let ai_result = &result.ai_results[0];

        // Model ID should be "gpt-4"
        assert_eq!(ai_result.model_id, b"gpt-4");

        // Output should be 32 bytes
        assert_eq!(ai_result.output.len(), 32);

        // Verify deterministic: output = BLAKE3("gpt-4" || "hello world")
        let mut expected_preimage = Vec::new();
        expected_preimage.extend_from_slice(b"gpt-4");
        expected_preimage.extend_from_slice(b"hello world");
        let expected_output = hash_bytes(&expected_preimage);
        assert_eq!(ai_result.output, expected_output.0.to_vec());

        // Verify input_hash = BLAKE3("hello world")
        let expected_input_hash = hash_bytes(b"hello world");
        assert_eq!(ai_result.input_hash, expected_input_hash);

        // Verify output_hash = BLAKE3(output)
        let expected_output_hash = hash_bytes(&ai_result.output);
        assert_eq!(ai_result.output_hash, expected_output_hash);

        // Verify gas was consumed: 1000 + 10*11 + 10*32 = 1000 + 110 + 320 = 1430
        assert_eq!(ai_result.gas_cost, 1430);
        assert_eq!(result.gas_used, 1430);
    }

}
