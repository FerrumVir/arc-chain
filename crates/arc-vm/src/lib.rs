use thiserror::Error;
use wasmer::{imports, Function, Instance, Module, Store, Value};

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
}

/// Result of WASM contract execution.
#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub success: bool,
    pub gas_used: u64,
    pub return_data: Vec<u8>,
    pub logs: Vec<String>,
}

/// ARC WASM Virtual Machine.
/// Executes smart contracts compiled to WebAssembly.
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

    /// Execute a function in a compiled WASM module.
    pub fn execute(
        &mut self,
        module: &Module,
        function_name: &str,
        args: &[Value],
        gas_limit: u64,
    ) -> Result<ExecutionResult, VmError> {
        // Set up host imports (chain API available to contracts)
        let gas_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let gas_limit_arc = std::sync::Arc::new(gas_limit);
        let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let gc = gas_counter.clone();
        let gl = gas_limit_arc.clone();
        let log_fn_logs = logs.clone();

        let log_fn = Function::new_typed(&mut self.store, move |ptr: i32, len: i32| {
            let _ = (ptr, len); // In real impl, read from WASM memory
            log_fn_logs.lock().unwrap().push(format!("log({}, {})", ptr, len));
        });

        let gc2 = gas_counter.clone();
        let gl2 = gas_limit_arc.clone();
        let use_gas_fn = Function::new_typed(&mut self.store, move |amount: i64| {
            gc2.fetch_add(amount as u64, std::sync::atomic::Ordering::Relaxed);
        });

        let import_object = imports! {
            "env" => {
                "log" => log_fn,
                "use_gas" => use_gas_fn,
            }
        };

        let instance = Instance::new(&mut self.store, module, &import_object)
            .map_err(|e| VmError::InstantiationError(e.to_string()))?;

        let func = instance
            .exports
            .get_function(function_name)
            .map_err(|_| VmError::FunctionNotFound(function_name.to_string()))?;

        let result = func
            .call(&mut self.store, args)
            .map_err(|e| VmError::ExecutionError(e.to_string()))?;

        let gas_used = gas_counter.load(std::sync::atomic::Ordering::Relaxed);
        let captured_logs = std::mem::take(&mut *logs.lock().unwrap());

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

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal WASM module that exports an `add` function: (i32, i32) -> i32
    fn minimal_add_wasm() -> Vec<u8> {
        // WAT: (module (func (export "add") (param i32 i32) (result i32) (i32.add (local.get 0) (local.get 1))))
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

    #[test]
    fn test_compile_and_execute() {
        let mut vm = ArcVM::new();
        let wasm = minimal_add_wasm();
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
        let wasm = minimal_add_wasm();
        let module = vm.compile(&wasm).unwrap();
        let result = vm.execute(&module, "nonexistent", &[], 1_000_000);
        assert!(matches!(result, Err(VmError::FunctionNotFound(_))));
    }

    #[test]
    fn test_validate_wasm() {
        let wasm = minimal_add_wasm();
        assert!(validate_wasm(&wasm));
        assert!(!validate_wasm(b"not wasm"));
    }
}
