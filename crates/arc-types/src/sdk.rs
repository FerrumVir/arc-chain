use crate::devtools::{AbiFunction, AbiType, ContractAbi, StateMutability};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SdkLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
    Swift,
    Kotlin,
}

impl SdkLanguage {
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Rust => "rs",
            Self::TypeScript => "ts",
            Self::Python => "py",
            Self::Go => "go",
            Self::Java => "java",
            Self::Swift => "swift",
            Self::Kotlin => "kt",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::TypeScript => "TypeScript",
            Self::Python => "Python",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::Swift => "Swift",
            Self::Kotlin => "Kotlin",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkConfig {
    pub language: SdkLanguage,
    pub output_dir: String,
    pub package_name: String,
    pub version: String,
}

impl SdkConfig {
    pub fn new(language: SdkLanguage, output_dir: &str, package_name: &str, version: &str) -> Self {
        Self {
            language,
            output_dir: output_dir.to_string(),
            package_name: package_name.to_string(),
            version: version.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ParamType {
    Uint256,
    Int256,
    Address,
    Bool,
    String,
    Bytes,
    FixedBytes(usize),
    Array(Box<ParamType>),
    Tuple(Vec<ParamType>),
}

impl ParamType {
    pub fn to_language_type(&self, language: SdkLanguage) -> String {
        match language {
            SdkLanguage::Rust => self.to_rust_type(),
            SdkLanguage::TypeScript => self.to_ts_type(),
            SdkLanguage::Python => self.to_python_type(),
            _ => self.to_ts_type(),
        }
    }

    fn to_rust_type(&self) -> String {
        match self {
            Self::Uint256 => "U256".to_string(),
            Self::Int256 => "I256".to_string(),
            Self::Address => "[u8; 32]".to_string(),
            Self::Bool => "bool".to_string(),
            Self::String => "String".to_string(),
            Self::Bytes => "Vec<u8>".to_string(),
            Self::FixedBytes(n) => format!("[u8; {}]", n),
            Self::Array(inner) => format!("Vec<{}>", inner.to_rust_type()),
            Self::Tuple(types) => {
                let inner: Vec<String> = types.iter().map(|t| t.to_rust_type()).collect();
                format!("({})", inner.join(", "))
            }
        }
    }

    fn to_ts_type(&self) -> String {
        match self {
            Self::Uint256 | Self::Int256 => "bigint".to_string(),
            Self::Address => "string".to_string(),
            Self::Bool => "boolean".to_string(),
            Self::String => "string".to_string(),
            Self::Bytes | Self::FixedBytes(_) => "Uint8Array".to_string(),
            Self::Array(inner) => format!("{}[]", inner.to_ts_type()),
            Self::Tuple(types) => {
                let inner: Vec<String> = types.iter().map(|t| t.to_ts_type()).collect();
                format!("[{}]", inner.join(", "))
            }
        }
    }

    fn to_python_type(&self) -> String {
        match self {
            Self::Uint256 | Self::Int256 => "int".to_string(),
            Self::Address => "str".to_string(),
            Self::Bool => "bool".to_string(),
            Self::String => "str".to_string(),
            Self::Bytes | Self::FixedBytes(_) => "bytes".to_string(),
            Self::Array(inner) => format!("list[{}]", inner.to_python_type()),
            Self::Tuple(types) => {
                let inner: Vec<String> = types.iter().map(|t| t.to_python_type()).collect();
                format!("tuple[{}]", inner.join(", "))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodBinding {
    pub name: String,
    pub inputs: Vec<ParamType>,
    pub outputs: Vec<ParamType>,
    pub is_view: bool,
    pub gas_estimate: u64,
}

impl MethodBinding {
    pub fn new(
        name: &str,
        inputs: Vec<ParamType>,
        outputs: Vec<ParamType>,
        is_view: bool,
        gas_estimate: u64,
    ) -> Self {
        Self {
            name: name.to_string(),
            inputs,
            outputs,
            is_view,
            gas_estimate,
        }
    }

    pub fn from_abi_function(func: &AbiFunction) -> Self {
        let inputs: Vec<ParamType> = func
            .inputs
            .iter()
            .map(|p| abi_type_to_param_type(&p.param_type))
            .collect();
        let outputs: Vec<ParamType> = func
            .outputs
            .iter()
            .map(|p| abi_type_to_param_type(&p.param_type))
            .collect();
        let is_view = matches!(
            func.state_mutability,
            StateMutability::View | StateMutability::Pure
        );

        Self {
            name: func.name.clone(),
            inputs,
            outputs,
            is_view,
            gas_estimate: 0,
        }
    }
}

fn abi_type_to_param_type(abi: &AbiType) -> ParamType {
    match abi {
        AbiType::Uint256 => ParamType::Uint256,
        AbiType::Int256 => ParamType::Int256,
        AbiType::Address => ParamType::Address,
        AbiType::Bool => ParamType::Bool,
        AbiType::String => ParamType::String,
        AbiType::Bytes => ParamType::Bytes,
        AbiType::BytesN(n) => ParamType::FixedBytes(*n as usize),
        AbiType::Array(inner) => ParamType::Array(Box::new(abi_type_to_param_type(inner))),
        AbiType::Tuple(types) => {
            ParamType::Tuple(types.iter().map(abi_type_to_param_type).collect())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractBinding {
    pub contract_name: String,
    pub address: [u8; 32],
    pub abi: ContractAbi,
    pub methods: Vec<MethodBinding>,
}

impl ContractBinding {
    pub fn new(contract_name: &str, address: [u8; 32], abi: ContractAbi) -> Self {
        let methods = abi
            .functions
            .iter()
            .map(MethodBinding::from_abi_function)
            .collect();
        Self {
            contract_name: contract_name.to_string(),
            address,
            abi,
            methods,
        }
    }

    pub fn get_method(&self, name: &str) -> Option<&MethodBinding> {
        self.methods.iter().find(|m| m.name == name)
    }

    pub fn view_method_count(&self) -> usize {
        self.methods.iter().filter(|m| m.is_view).count()
    }

    pub fn mutating_method_count(&self) -> usize {
        self.methods.iter().filter(|m| !m.is_view).count()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedFile {
    pub path: String,
    pub content: String,
}

impl GeneratedFile {
    pub fn new(path: &str, content: &str) -> Self {
        Self {
            path: path.to_string(),
            content: content.to_string(),
        }
    }

    pub fn size_bytes(&self) -> usize {
        self.content.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedCode {
    pub language: SdkLanguage,
    pub files: Vec<GeneratedFile>,
}

impl GeneratedCode {
    pub fn new(language: SdkLanguage) -> Self {
        Self {
            language,
            files: Vec::new(),
        }
    }

    pub fn add_file(&mut self, file: GeneratedFile) {
        self.files.push(file);
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn total_size_bytes(&self) -> usize {
        self.files.iter().map(|f| f.size_bytes()).sum()
    }
}

#[derive(Debug)]
pub struct SdkGenerator {
    config: SdkConfig,
    bindings: Vec<ContractBinding>,
}

impl SdkGenerator {
    pub fn new(config: SdkConfig) -> Self {
        Self {
            config,
            bindings: Vec::new(),
        }
    }

    pub fn add_binding(&mut self, binding: ContractBinding) {
        self.bindings.push(binding);
    }

    pub fn generate_types(&self) -> GeneratedCode {
        let mut code = GeneratedCode::new(self.config.language);
        let ext = self.config.language.extension();

        for binding in &self.bindings {
            let content = self.render_types(binding);
            let path = format!(
                "{}/types/{}.{}",
                self.config.output_dir,
                binding.contract_name.to_lowercase(),
                ext,
            );
            code.add_file(GeneratedFile::new(&path, &content));
        }

        code
    }

    pub fn generate_bindings(&self) -> GeneratedCode {
        let mut code = GeneratedCode::new(self.config.language);
        let ext = self.config.language.extension();

        for binding in &self.bindings {
            let content = self.render_bindings(binding);
            let path = format!(
                "{}/bindings/{}.{}",
                self.config.output_dir,
                binding.contract_name.to_lowercase(),
                ext,
            );
            code.add_file(GeneratedFile::new(&path, &content));
        }

        code
    }

    pub fn generate_client(&self) -> GeneratedCode {
        let mut code = GeneratedCode::new(self.config.language);
        let ext = self.config.language.extension();

        let types = self.generate_types();
        for f in types.files {
            code.add_file(f);
        }

        let bindings = self.generate_bindings();
        for f in bindings.files {
            code.add_file(f);
        }

        let index_content = self.render_index();
        let index_path = format!("{}/index.{}", self.config.output_dir, ext);
        code.add_file(GeneratedFile::new(&index_path, &index_content));

        code
    }

    fn render_types(&self, binding: &ContractBinding) -> String {
        let lang = self.config.language;
        let mut lines = Vec::new();

        lines.push(format!(
            "// Auto-generated {} types for {}",
            lang.display_name(),
            binding.contract_name,
        ));
        lines.push(format!(
            "// Package: {} v{}",
            self.config.package_name, self.config.version
        ));
        lines.push(String::new());

        for method in &binding.methods {
            let inputs: Vec<String> = method
                .inputs
                .iter()
                .enumerate()
                .map(|(i, p)| format!("  param_{}: {}", i, p.to_language_type(lang)))
                .collect();
            let outputs: Vec<String> = method
                .outputs
                .iter()
                .map(|p| p.to_language_type(lang))
                .collect();

            lines.push(format!(
                "// {} ({}) -> ({})",
                method.name,
                inputs.join(", "),
                outputs.join(", "),
            ));
        }

        lines.join("\n")
    }

    fn render_bindings(&self, binding: &ContractBinding) -> String {
        let lang = self.config.language;
        let mut lines = Vec::new();

        lines.push(format!(
            "// Auto-generated {} bindings for {}",
            lang.display_name(),
            binding.contract_name,
        ));
        lines.push(String::new());

        for method in &binding.methods {
            let marker = if method.is_view { "view" } else { "send" };
            lines.push(format!(
                "// [{}] {} - gas estimate: {}",
                marker, method.name, method.gas_estimate,
            ));
        }

        lines.join("\n")
    }

    fn render_index(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "// {} SDK - {} v{}",
            self.config.package_name,
            self.config.language.display_name(),
            self.config.version,
        ));
        lines.push(String::new());

        for binding in &self.bindings {
            lines.push(format!("// export: {}", binding.contract_name));
        }

        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devtools::{AbiFunction, AbiParam, AbiType, ContractAbi, StateMutability};

    fn test_hash(n: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0] = n;
        h
    }

    fn sample_abi() -> ContractAbi {
        let mut abi = ContractAbi::new();
        abi.add_function(AbiFunction::new(
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
        ));
        abi.add_function(AbiFunction::new(
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
        ));
        abi
    }

    #[test]
    fn test_sdk_language_extensions() {
        assert_eq!(SdkLanguage::Rust.extension(), "rs");
        assert_eq!(SdkLanguage::TypeScript.extension(), "ts");
        assert_eq!(SdkLanguage::Python.extension(), "py");
        assert_eq!(SdkLanguage::Go.extension(), "go");
        assert_eq!(SdkLanguage::Java.extension(), "java");
        assert_eq!(SdkLanguage::Swift.extension(), "swift");
        assert_eq!(SdkLanguage::Kotlin.extension(), "kt");
    }

    #[test]
    fn test_contract_binding_from_abi() {
        let abi = sample_abi();
        let binding = ContractBinding::new("Token", test_hash(1), abi);

        assert_eq!(binding.contract_name, "Token");
        assert_eq!(binding.methods.len(), 2);
        assert_eq!(binding.view_method_count(), 1);
        assert_eq!(binding.mutating_method_count(), 1);

        let transfer = binding.get_method("transfer").unwrap();
        assert!(!transfer.is_view);
        assert_eq!(transfer.inputs.len(), 2);
        assert_eq!(transfer.outputs.len(), 1);

        let balance_of = binding.get_method("balanceOf").unwrap();
        assert!(balance_of.is_view);
    }

    #[test]
    fn test_param_type_rust_rendering() {
        assert_eq!(
            ParamType::Uint256.to_language_type(SdkLanguage::Rust),
            "U256"
        );
        assert_eq!(
            ParamType::Address.to_language_type(SdkLanguage::Rust),
            "[u8; 32]"
        );
        assert_eq!(ParamType::Bool.to_language_type(SdkLanguage::Rust), "bool");
        assert_eq!(
            ParamType::String.to_language_type(SdkLanguage::Rust),
            "String"
        );
        assert_eq!(
            ParamType::Bytes.to_language_type(SdkLanguage::Rust),
            "Vec<u8>"
        );
        assert_eq!(
            ParamType::FixedBytes(32).to_language_type(SdkLanguage::Rust),
            "[u8; 32]"
        );

        let arr = ParamType::Array(Box::new(ParamType::Uint256));
        assert_eq!(arr.to_language_type(SdkLanguage::Rust), "Vec<U256>");

        let tuple = ParamType::Tuple(vec![ParamType::Address, ParamType::Bool]);
        assert_eq!(
            tuple.to_language_type(SdkLanguage::Rust),
            "([u8; 32], bool)"
        );
    }

    #[test]
    fn test_param_type_typescript_rendering() {
        assert_eq!(
            ParamType::Uint256.to_language_type(SdkLanguage::TypeScript),
            "bigint"
        );
        assert_eq!(
            ParamType::Address.to_language_type(SdkLanguage::TypeScript),
            "string"
        );
        assert_eq!(
            ParamType::Bool.to_language_type(SdkLanguage::TypeScript),
            "boolean"
        );
        assert_eq!(
            ParamType::String.to_language_type(SdkLanguage::TypeScript),
            "string"
        );
        assert_eq!(
            ParamType::Bytes.to_language_type(SdkLanguage::TypeScript),
            "Uint8Array"
        );

        let arr = ParamType::Array(Box::new(ParamType::Address));
        assert_eq!(arr.to_language_type(SdkLanguage::TypeScript), "string[]");
    }

    #[test]
    fn test_param_type_python_rendering() {
        assert_eq!(
            ParamType::Uint256.to_language_type(SdkLanguage::Python),
            "int"
        );
        assert_eq!(
            ParamType::Address.to_language_type(SdkLanguage::Python),
            "str"
        );
        assert_eq!(
            ParamType::Bool.to_language_type(SdkLanguage::Python),
            "bool"
        );
        assert_eq!(
            ParamType::Bytes.to_language_type(SdkLanguage::Python),
            "bytes"
        );

        let arr = ParamType::Array(Box::new(ParamType::Int256));
        assert_eq!(arr.to_language_type(SdkLanguage::Python), "list[int]");
    }

    #[test]
    fn test_sdk_generator_types() {
        let config = SdkConfig::new(SdkLanguage::TypeScript, "./out", "arc-sdk", "1.0.0");
        let mut generator = SdkGenerator::new(config);
        generator.add_binding(ContractBinding::new("Token", test_hash(1), sample_abi()));

        let types = generator.generate_types();
        assert_eq!(types.language, SdkLanguage::TypeScript);
        assert_eq!(types.file_count(), 1);
        assert!(types.files[0].path.ends_with("token.ts"));
        assert!(
            types.files[0]
                .content
                .contains("TypeScript types for Token")
        );
    }

    #[test]
    fn test_sdk_generator_client() {
        let config = SdkConfig::new(SdkLanguage::Rust, "./out", "arc-sdk", "0.1.0");
        let mut generator = SdkGenerator::new(config);
        generator.add_binding(ContractBinding::new("Token", test_hash(1), sample_abi()));

        let client = generator.generate_client();
        assert_eq!(client.file_count(), 3);
        assert!(client.total_size_bytes() > 0);

        let index = client.files.iter().find(|f| f.path.contains("index."));
        assert!(index.is_some());
        assert!(index.unwrap().content.contains("arc-sdk"));
    }

    #[test]
    fn test_generated_code_sizes() {
        let mut code = GeneratedCode::new(SdkLanguage::Python);
        assert_eq!(code.file_count(), 0);
        assert_eq!(code.total_size_bytes(), 0);

        code.add_file(GeneratedFile::new("a.py", "print('hello')"));
        code.add_file(GeneratedFile::new("b.py", "x = 42"));
        assert_eq!(code.file_count(), 2);
        assert_eq!(
            code.total_size_bytes(),
            "print('hello')".len() + "x = 42".len()
        );
    }

    #[test]
    fn test_method_binding_from_abi() {
        let func = AbiFunction::new(
            "approve",
            vec![
                AbiParam {
                    name: "spender".to_string(),
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

        let method = MethodBinding::from_abi_function(&func);
        assert_eq!(method.name, "approve");
        assert!(!method.is_view);
        assert_eq!(method.inputs.len(), 2);
        assert_eq!(method.outputs.len(), 1);
        assert_eq!(method.gas_estimate, 0);

        let view_func = AbiFunction::new(
            "totalSupply",
            vec![],
            vec![AbiParam {
                name: "supply".to_string(),
                param_type: AbiType::Uint256,
            }],
            StateMutability::View,
        );
        let view_method = MethodBinding::from_abi_function(&view_func);
        assert!(view_method.is_view);
    }

    #[test]
    fn test_sdk_config_serde() {
        let config = SdkConfig::new(SdkLanguage::Go, "./gen", "arc-go-sdk", "2.0.0");
        let json = serde_json::to_string(&config).expect("serialize SdkConfig");
        let back: SdkConfig = serde_json::from_str(&json).expect("deserialize SdkConfig");

        assert_eq!(back.language, SdkLanguage::Go);
        assert_eq!(back.output_dir, "./gen");
        assert_eq!(back.package_name, "arc-go-sdk");
        assert_eq!(back.version, "2.0.0");
    }
}
