// Add to lib.rs: pub mod docs;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ─── Doc category ────────────────────────────────────────────────────────────

/// Category of a documentation entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocCategory {
    API,
    Contract,
    Transaction,
    Guide,
    Tutorial,
    Reference,
}

impl fmt::Display for DocCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::API => write!(f, "API"),
            Self::Contract => write!(f, "Contract"),
            Self::Transaction => write!(f, "Transaction"),
            Self::Guide => write!(f, "Guide"),
            Self::Tutorial => write!(f, "Tutorial"),
            Self::Reference => write!(f, "Reference"),
        }
    }
}

// ─── HTTP method ─────────────────────────────────────────────────────────────

/// HTTP method for API documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Get => write!(f, "GET"),
            Self::Post => write!(f, "POST"),
            Self::Put => write!(f, "PUT"),
            Self::Delete => write!(f, "DELETE"),
            Self::Patch => write!(f, "PATCH"),
        }
    }
}

// ─── Doc example ─────────────────────────────────────────────────────────────

/// A code example within a documentation entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocExample {
    pub title: String,
    pub language: String,
    pub code: String,
    pub description: String,
}

impl DocExample {
    /// Create a new documentation example.
    pub fn new(title: &str, language: &str, code: &str, description: &str) -> Self {
        Self {
            title: title.to_string(),
            language: language.to_string(),
            code: code.to_string(),
            description: description.to_string(),
        }
    }

    /// Render this example as a Markdown code block.
    pub fn to_markdown(&self) -> String {
        format!(
            "### {}\n\n{}\n\n```{}\n{}\n```",
            self.title, self.description, self.language, self.code,
        )
    }
}

// ─── Doc entry ───────────────────────────────────────────────────────────────

/// A documentation entry — the primary unit of generated docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocEntry {
    pub id: String,
    pub title: String,
    pub category: DocCategory,
    pub content: String,
    pub examples: Vec<DocExample>,
    pub last_updated: u64,
}

impl DocEntry {
    /// Create a new documentation entry.
    pub fn new(
        id: &str,
        title: &str,
        category: DocCategory,
        content: &str,
        last_updated: u64,
    ) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            category,
            content: content.to_string(),
            examples: Vec::new(),
            last_updated,
        }
    }

    /// Add a code example to this entry.
    pub fn add_example(&mut self, example: DocExample) {
        self.examples.push(example);
    }

    /// Render this entry as a Markdown document.
    pub fn to_markdown(&self) -> String {
        let mut parts = Vec::new();

        parts.push(format!("# {}", self.title));
        parts.push(format!("**Category:** {}", self.category));
        parts.push(String::new());
        parts.push(self.content.clone());

        if !self.examples.is_empty() {
            parts.push(String::new());
            parts.push("## Examples".to_string());
            for example in &self.examples {
                parts.push(String::new());
                parts.push(example.to_markdown());
            }
        }

        parts.join("\n")
    }
}

// ─── API param ───────────────────────────────────────────────────────────────

/// A parameter in an API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiParam {
    pub name: String,
    pub param_type: String,
    pub required: bool,
    pub description: String,
    pub default_value: Option<String>,
}

impl ApiParam {
    /// Create a new required API parameter.
    pub fn required(name: &str, param_type: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            param_type: param_type.to_string(),
            required: true,
            description: description.to_string(),
            default_value: None,
        }
    }

    /// Create a new optional API parameter with a default value.
    pub fn optional(name: &str, param_type: &str, description: &str, default: &str) -> Self {
        Self {
            name: name.to_string(),
            param_type: param_type.to_string(),
            required: false,
            description: description.to_string(),
            default_value: Some(default.to_string()),
        }
    }
}

// ─── API response ────────────────────────────────────────────────────────────

/// Describes the response from an API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    pub status_code: u16,
    pub content_type: String,
    pub schema: String,
    pub example: String,
}

impl ApiResponse {
    /// Create a JSON API response descriptor.
    pub fn json(status_code: u16, schema: &str, example: &str) -> Self {
        Self {
            status_code,
            content_type: "application/json".to_string(),
            schema: schema.to_string(),
            example: example.to_string(),
        }
    }
}

// ─── API doc ─────────────────────────────────────────────────────────────────

/// Full documentation for an API endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiDoc {
    pub endpoint: String,
    pub method: HttpMethod,
    pub description: String,
    pub params: Vec<ApiParam>,
    pub response: ApiResponse,
    pub examples: Vec<DocExample>,
}

impl ApiDoc {
    /// Create a new API doc entry.
    pub fn new(
        endpoint: &str,
        method: HttpMethod,
        description: &str,
        params: Vec<ApiParam>,
        response: ApiResponse,
    ) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            method,
            description: description.to_string(),
            params,
            response,
            examples: Vec::new(),
        }
    }

    /// Add an example to this API doc.
    pub fn add_example(&mut self, example: DocExample) {
        self.examples.push(example);
    }

    /// Render this API doc as a Markdown section.
    pub fn to_markdown(&self) -> String {
        let mut parts = Vec::new();

        parts.push(format!("## `{} {}`", self.method, self.endpoint));
        parts.push(String::new());
        parts.push(self.description.clone());
        parts.push(String::new());

        // Parameters table.
        if !self.params.is_empty() {
            parts.push("### Parameters".to_string());
            parts.push(String::new());
            parts.push("| Name | Type | Required | Description | Default |".to_string());
            parts.push("|------|------|----------|-------------|---------|".to_string());
            for p in &self.params {
                let req = if p.required { "Yes" } else { "No" };
                let default = p.default_value.as_deref().unwrap_or("-");
                parts.push(format!(
                    "| {} | {} | {} | {} | {} |",
                    p.name, p.param_type, req, p.description, default,
                ));
            }
            parts.push(String::new());
        }

        // Response.
        parts.push("### Response".to_string());
        parts.push(String::new());
        parts.push(format!("**Status:** {}", self.response.status_code));
        parts.push(format!("**Content-Type:** {}", self.response.content_type));
        parts.push(String::new());
        parts.push(format!("```json\n{}\n```", self.response.example));

        // Examples.
        if !self.examples.is_empty() {
            parts.push(String::new());
            parts.push("### Examples".to_string());
            for example in &self.examples {
                parts.push(String::new());
                parts.push(example.to_markdown());
            }
        }

        parts.join("\n")
    }
}

// ─── Doc registry ────────────────────────────────────────────────────────────

/// Central registry for all documentation entries.
#[derive(Debug)]
pub struct DocRegistry {
    entries: HashMap<String, DocEntry>,
    api_docs: Vec<ApiDoc>,
}

impl DocRegistry {
    /// Create a new, empty doc registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            api_docs: Vec::new(),
        }
    }

    /// Register a documentation entry.
    pub fn register(&mut self, entry: DocEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Register an API documentation entry.
    pub fn register_api(&mut self, doc: ApiDoc) {
        self.api_docs.push(doc);
    }

    /// Look up a documentation entry by its ID.
    pub fn lookup(&self, id: &str) -> Option<&DocEntry> {
        self.entries.get(id)
    }

    /// Search entries by keyword in title or content (case-insensitive).
    pub fn search(&self, keyword: &str) -> Vec<&DocEntry> {
        let kw_lower = keyword.to_lowercase();
        self.entries.values()
            .filter(|e| {
                e.title.to_lowercase().contains(&kw_lower)
                    || e.content.to_lowercase().contains(&kw_lower)
            })
            .collect()
    }

    /// List all entries in a given category.
    pub fn list_by_category(&self, category: DocCategory) -> Vec<&DocEntry> {
        self.entries.values()
            .filter(|e| e.category == category)
            .collect()
    }

    /// Generate a full Markdown document from all registered entries.
    pub fn generate_markdown(&self) -> String {
        let mut parts = Vec::new();

        parts.push("# ARC Chain Documentation".to_string());
        parts.push(String::new());

        // Group entries by category.
        let categories = [
            DocCategory::Guide,
            DocCategory::Tutorial,
            DocCategory::API,
            DocCategory::Contract,
            DocCategory::Transaction,
            DocCategory::Reference,
        ];

        for cat in &categories {
            let entries = self.list_by_category(*cat);
            if entries.is_empty() {
                continue;
            }

            parts.push(format!("## {}", cat));
            parts.push(String::new());

            for entry in entries {
                parts.push(entry.to_markdown());
                parts.push(String::new());
                parts.push("---".to_string());
                parts.push(String::new());
            }
        }

        // API docs.
        if !self.api_docs.is_empty() {
            parts.push("## API Reference".to_string());
            parts.push(String::new());

            for doc in &self.api_docs {
                parts.push(doc.to_markdown());
                parts.push(String::new());
                parts.push("---".to_string());
                parts.push(String::new());
            }
        }

        parts.join("\n")
    }

    /// Total number of registered entries (excluding API docs).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Total number of registered API docs.
    pub fn api_doc_count(&self) -> usize {
        self.api_docs.len()
    }
}

impl Default for DocRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(id: &str, title: &str, cat: DocCategory) -> DocEntry {
        DocEntry::new(id, title, cat, "Sample content.", 1_700_000_000)
    }

    fn sample_api_doc() -> ApiDoc {
        ApiDoc::new(
            "/api/v1/blocks/{height}",
            HttpMethod::Get,
            "Fetch a block by height.",
            vec![
                ApiParam::required("height", "u64", "Block height"),
            ],
            ApiResponse::json(200, "Block", r#"{"height": 42}"#),
        )
    }

    // 1. Register and lookup a doc entry.
    #[test]
    fn test_register_and_lookup() {
        let mut reg = DocRegistry::new();
        reg.register(sample_entry("tx-guide", "Transaction Guide", DocCategory::Guide));

        let found = reg.lookup("tx-guide");
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Transaction Guide");
        assert_eq!(reg.entry_count(), 1);

        // Non-existent entry.
        assert!(reg.lookup("does-not-exist").is_none());
    }

    // 2. Search entries by keyword.
    #[test]
    fn test_search() {
        let mut reg = DocRegistry::new();
        reg.register(sample_entry("guide-1", "Getting Started", DocCategory::Guide));
        reg.register(sample_entry("ref-1", "Transaction Reference", DocCategory::Reference));
        reg.register(sample_entry("tut-1", "Deploy a Contract", DocCategory::Tutorial));

        // Case-insensitive title search.
        let results = reg.search("transaction");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "ref-1");

        // Content search — all entries have "Sample content."
        let results = reg.search("sample");
        assert_eq!(results.len(), 3);

        // No results.
        let results = reg.search("zzz_nonexistent");
        assert_eq!(results.len(), 0);
    }

    // 3. List entries by category.
    #[test]
    fn test_list_by_category() {
        let mut reg = DocRegistry::new();
        reg.register(sample_entry("g1", "Guide 1", DocCategory::Guide));
        reg.register(sample_entry("g2", "Guide 2", DocCategory::Guide));
        reg.register(sample_entry("r1", "Ref 1", DocCategory::Reference));

        let guides = reg.list_by_category(DocCategory::Guide);
        assert_eq!(guides.len(), 2);

        let refs = reg.list_by_category(DocCategory::Reference);
        assert_eq!(refs.len(), 1);

        let contracts = reg.list_by_category(DocCategory::Contract);
        assert_eq!(contracts.len(), 0);
    }

    // 4. DocEntry to_markdown.
    #[test]
    fn test_doc_entry_markdown() {
        let mut entry = sample_entry("test", "Test Entry", DocCategory::API);
        entry.add_example(DocExample::new(
            "Basic Usage",
            "rust",
            "let x = 42;",
            "Shows basic usage.",
        ));

        let md = entry.to_markdown();
        assert!(md.contains("# Test Entry"));
        assert!(md.contains("**Category:** API"));
        assert!(md.contains("Sample content."));
        assert!(md.contains("## Examples"));
        assert!(md.contains("```rust"));
        assert!(md.contains("let x = 42;"));
    }

    // 5. ApiDoc to_markdown.
    #[test]
    fn test_api_doc_markdown() {
        let mut doc = sample_api_doc();
        doc.add_example(DocExample::new(
            "cURL Example",
            "bash",
            "curl http://localhost:8080/api/v1/blocks/42",
            "Fetch block 42.",
        ));

        let md = doc.to_markdown();
        assert!(md.contains("## `GET /api/v1/blocks/{height}`"));
        assert!(md.contains("Fetch a block by height."));
        assert!(md.contains("### Parameters"));
        assert!(md.contains("| height | u64 | Yes |"));
        assert!(md.contains("### Response"));
        assert!(md.contains("**Status:** 200"));
        assert!(md.contains("### Examples"));
        assert!(md.contains("```bash"));
    }

    // 6. DocRegistry generate_markdown.
    #[test]
    fn test_generate_markdown() {
        let mut reg = DocRegistry::new();
        reg.register(sample_entry("g1", "Getting Started", DocCategory::Guide));
        reg.register(sample_entry("a1", "Block API", DocCategory::API));
        reg.register_api(sample_api_doc());

        let md = reg.generate_markdown();
        assert!(md.contains("# ARC Chain Documentation"));
        assert!(md.contains("## Guide"));
        assert!(md.contains("Getting Started"));
        assert!(md.contains("## API"));
        assert!(md.contains("Block API"));
        assert!(md.contains("## API Reference"));
        assert!(md.contains("/api/v1/blocks/{height}"));
    }

    // 7. ApiParam required vs optional.
    #[test]
    fn test_api_param_variants() {
        let req = ApiParam::required("address", "string", "Wallet address");
        assert!(req.required);
        assert!(req.default_value.is_none());

        let opt = ApiParam::optional("limit", "u32", "Max results", "100");
        assert!(!opt.required);
        assert_eq!(opt.default_value.as_deref(), Some("100"));
    }

    // 8. HttpMethod display.
    #[test]
    fn test_http_method_display() {
        assert_eq!(format!("{}", HttpMethod::Get), "GET");
        assert_eq!(format!("{}", HttpMethod::Post), "POST");
        assert_eq!(format!("{}", HttpMethod::Put), "PUT");
        assert_eq!(format!("{}", HttpMethod::Delete), "DELETE");
        assert_eq!(format!("{}", HttpMethod::Patch), "PATCH");
    }

    // 9. DocCategory display.
    #[test]
    fn test_doc_category_display() {
        assert_eq!(format!("{}", DocCategory::API), "API");
        assert_eq!(format!("{}", DocCategory::Contract), "Contract");
        assert_eq!(format!("{}", DocCategory::Transaction), "Transaction");
        assert_eq!(format!("{}", DocCategory::Guide), "Guide");
        assert_eq!(format!("{}", DocCategory::Tutorial), "Tutorial");
        assert_eq!(format!("{}", DocCategory::Reference), "Reference");
    }

    // 10. Serde round-trip for DocEntry.
    #[test]
    fn test_doc_entry_serde() {
        let mut entry = sample_entry("serde-test", "Serde Test", DocCategory::Tutorial);
        entry.add_example(DocExample::new("Ex", "rust", "fn main() {}", "Main fn."));

        let json = serde_json::to_string(&entry).expect("serialize DocEntry");
        let back: DocEntry = serde_json::from_str(&json).expect("deserialize DocEntry");

        assert_eq!(back.id, "serde-test");
        assert_eq!(back.title, "Serde Test");
        assert_eq!(back.category, DocCategory::Tutorial);
        assert_eq!(back.examples.len(), 1);
        assert_eq!(back.examples[0].language, "rust");
    }
}
